use super::*;

use tokio::net::udp::{RecvHalf, SendHalf};
use tokio::sync::mpsc::{channel, Receiver, Sender};

/// Control message as sent by client
#[derive(Serialize)]
struct OutgoingControlMessage {
    /// Client name
    na: String,
    /// List of points we opened
    ports: Vec<u16>,
    /// Set to true for the first message to trigger early reply
    disc: bool,
}

#[derive(Deserialize, Debug)]
struct IncomingControlMessage {
    /// Name chosen by CLI option
    na: String,
    /// Filled in by server
    ip: u32,
    /// Filled in by server
    po: u16,
    /// List of peer's opened ports
    ports: Vec<u16>,
    /// Peer requests us to set OutgoingControlMessage sooner
    disc: bool,
}

type PortIndex = usize;
enum CommunicationActorMessage {
    /// Route this to client (unless a bubble), also mark it as active
    PortData(PortIndex, Vec<u8>, SocketAddr),
    /// Client wants us to deliver this to the peer
    ClientData(Vec<u8>),
    /// Messaged arrived from server
    ControlMessage(IncomingControlMessage),
    /// If we are running without --sendto and new client arrived
    NewClientPeerAddress(SocketAddr),
}

#[derive(Default,Clone)]
struct PortStatus {
    last_seen_packet: Option<Instant>,
    configured_peer_addr: Option<SocketAddr>,
    seen_peer_addr: Option<SocketAddr>,
}

struct CommunicationActorState {
    ports: Vec<SendHalf>,
    client_tx: SendHalf,
    client_peer: Option<SocketAddr>,
    port_statuses: Vec<PortStatus>,
    max_norecv_time: Duration,
    override_sendto_addr: Option<std::net::Ipv4Addr>,
}

impl CommunicationActorState {
    async fn try_send_though_port(&mut self, i : PortIndex, buf: &[u8]) -> Result<bool> {
        
        if self.port_statuses[i].last_seen_packet.is_none() {
            return Ok(false);
        }
        let norecv = Instant::now().saturating_duration_since(self.port_statuses[i].last_seen_packet.unwrap());

        if norecv > self.max_norecv_time {
            return Ok(false);
        }

        match (self.port_statuses[i].configured_peer_addr, self.port_statuses[i].seen_peer_addr) {
            (None, None) => Err("Strange that no address, yet seen packets")?,
            (Some(x), None) | (None, Some(x)) => {
                self.ports[i].send_to(buf, &x).await?;
                Ok(true)
            }
            (Some(x), Some(y)) if x == y => {
                self.ports[i].send_to(buf, &x).await?;
                Ok(true)
            }
            (Some(x), Some(y)) => {
                eprintln!("Different seen and configured addresses?");
                self.ports[i].send_to(buf, &x).await?;
                self.ports[i].send_to(buf, &y).await?;
                Ok(true)
            }
        }
    }

    async fn msg(&mut self, message: CommunicationActorMessage) -> Result<()> {
        match message {
            CommunicationActorMessage::ControlMessage(cm) => {
                let mut ip : std::net::Ipv4Addr = (cm.ip ^ IP_MASK).into();
                if let Some(ovrip) = self.override_sendto_addr {
                    ip = ovrip;
                }
                let maxusableports = self.ports.len().min(cm.ports.len());
                for i in 0..maxusableports {
                    let po = cm.ports[i] ^ PORT_MASK;
                    let pa = SocketAddr::V4(std::net::SocketAddrV4::new(ip, po));
                    self.port_statuses[i].configured_peer_addr = Some(pa);
                    // Send a bubble to specified address
                    self.ports[i].send_to(b"", &pa).await?;
                }
            },
            CommunicationActorMessage::PortData(idx, buf, from) => {
                self.port_statuses[idx].seen_peer_addr = Some(from);
                self.port_statuses[idx].last_seen_packet = Some(Instant::now());
                if ! buf.is_empty() {
                    if let Some(claddr) = self.client_peer {
                        let _ = self.client_tx.send_to(&buf[..], &claddr).await?;
                    } else {
                        eprintln!("No client available yet");
                    }
                }
            },
            CommunicationActorMessage::ClientData(buf) => {
                use rand::Rng;

                // Try sending though a random port
                for _ in 0..4 {
                    let i = rand::thread_rng().gen_range(0, self.ports.len());
                    if self.try_send_though_port(i, &buf[..]).await? {
                        return Ok(())
                    }
                }
                // If failed, just try all ports sequentially
                for i in 0..self.ports.len() {
                    if self.try_send_though_port(i, &buf[..]).await? {
                        return Ok(())
                    }
                }
                // If still failed then we cannot relay anything now
                Err("No usable ports to send now")?;
            },
            CommunicationActorMessage::NewClientPeerAddress(newaddr) => {
                self.client_peer = Some(newaddr);
            }
        }
        Ok(())
    }
}





pub async fn client(
    server_addr: SocketAddr,
    name: String,
    pco: PlayloadCommunicationOptions,
    cs: ClientSettings,
) -> Result<()> {
    let sendaddr = pco.sendto;
    let bindaddr = if let Some(x) = pco.bind {
        x
    } else {
        if let Some(SocketAddr::V4(st)) = pco.sendto {
            if st.ip().is_loopback() {
                "127.0.0.1:0".parse().unwrap()
            } else {
                "0.0.0.0:0".parse().unwrap()
            }
        } else {
            "0.0.0.0:0".parse().unwrap()
        }
    };
    let keepalive_interval = Duration::from_secs(cs.keepalive_interval);

    let client = UdpSocket::bind(bindaddr).await?;

    let mut server = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap()).await?;
    server.connect(server_addr).await?;

    struct Port {
        u_rx: RecvHalf,
        u_tx: SendHalf,
        p: u16,
    }

    let mut sanity_limit: usize = 65536;
    /// UDP sockets with random ports
    let mut ports: Vec<Port> = Vec::with_capacity(cs.num_ports);

    let mut rnd = rand::thread_rng();
    use rand::RngCore;

    loop {
        let p: u16 = (rnd.next_u32() & 0xFFFF) as u16;
        if p <= 1024 {
            continue;
        }

        let nullip = std::net::Ipv4Addr::UNSPECIFIED;
        if let Ok(u) = UdpSocket::bind(SocketAddr::V4(std::net::SocketAddrV4::new(nullip, p))).await
        {
            let (u_rx, u_tx) = u.split();
            ports.push(Port { u_rx, u_tx, p });
        }

        if ports.len() == cs.num_ports {
            break;
        }

        sanity_limit -= 1;
        if sanity_limit == 0 {
            break;
        }
    }

    // Not `cs.num_ports` UDP sockets should be created
    if ports.is_empty() {
        Err("Failed to open any ports for communication")?;
    }

    let mut advertisment = OutgoingControlMessage {
        na: name,
        ports: ports.iter().map(|pp| pp.p ^ PORT_MASK).collect(),
        disc: true,
    };
    let mut advertisment_b = serde_cbor::to_vec(&advertisment)?;

    // ----------
    // Basic setup complete. Proceed to starting actors.
    // ----------

    let (mut client_rx, mut client_tx) = client.split();
    let (mut servrecv, mut servsend) = server.split();

    /// Sender of messages to server
    let (mut triggeradvsend, mut triggeradvsend_h) = channel::<()>(1);
    let mut triggeradvsend2 = triggeradvsend.clone();

    let (mut commactor, mut commactor_h) = channel::<CommunicationActorMessage>(32);

    use unzip3::Unzip3;
    let (ports_tx, ports_rx, port_numbers): (Vec<SendHalf>, Vec<RecvHalf>, Vec<u16>) = ports
        .into_iter()
        .map(|pp| (pp.u_tx, pp.u_rx, pp.p))
        .unzip3();

    /// Main, central actor:
    let mut commactor_st = CommunicationActorState {
        ports: ports_tx,
        client_tx,
        client_peer: sendaddr,
        port_statuses: vec![Default::default(); port_numbers.len()],
        max_norecv_time: Duration::from_secs(cs.keepalive_interval * 2),
        override_sendto_addr: cs.override_sendto_addr,
    };
    tokio::spawn(async move {
        while let Some(msg) = commactor_h.recv().await {
            if let Err(e) = commactor_st.msg(msg).await {
                eprintln!("Error: {}", e);
            }
        }
    });
    /// Data port receivers:
    for (i, mut u_rx) in ports_rx.into_iter().enumerate() {
        let mut commactor = commactor.clone();
        tokio::spawn(async move {
            loop {
                let mut buf = [0u8; 2048];
                match u_rx.recv_from(&mut buf).await {
                    Err(e) => {
                        eprintln!("Error receiving from data port: {}", e);
                        tokio::time::delay_for(Duration::from_millis(100)).await;
                        continue;
                    }
                    Ok((len, from)) => {
                        let _ = commactor
                            .send(CommunicationActorMessage::PortData(
                                i,
                                buf[0..len].to_vec(),
                                from,
                            ))
                            .await;
                    }
                }
            }
        });
    }

    /// Handler of messages from client
    let mut commactor2 = commactor.clone();
    tokio::spawn(async move {
        loop {
            let mut buf = [0u8; 2048];
            let mut last_known_peeraddr : Option<SocketAddr> = None;
            match client_rx.recv_from(&mut buf[..]).await {
                Err(e) => {
                    eprintln!("error receiving from client: {}", e);
                    tokio::time::delay_for(Duration::from_millis(33)).await;
                    continue;
                }
                Ok((len, from)) => {
                    if let Some(forcedaddr) = sendaddr {
                        // stricter mode
                        if from != forcedaddr {
                            eprintln!("Dropping foreign message to client port");
                            continue;
                        }
                    } else {
                        // lax mode
                        if last_known_peeraddr != Some(from) {
                            last_known_peeraddr = Some(from);
                            let _ = commactor2.send(CommunicationActorMessage::NewClientPeerAddress(from)).await;
                        }
                    }
                    let _ = commactor2.send(CommunicationActorMessage::ClientData(buf[0..len].to_vec())).await;
                }
            }
        }
    });

    /// Handler of incoming adverts
    tokio::spawn(async move {
        loop {
            let mut buf = [0u8; 2048];
            let len = match servrecv.recv(&mut buf[..]).await {
                Err(e) => {
                    eprintln!("error receiving from server: {}", e);
                    tokio::time::delay_for(Duration::from_millis(50)).await;
                    continue;
                }
                Ok(x) => x,
            };
            let msg: IncomingControlMessage = match serde_cbor::from_slice(&buf[0..len]) {
                Err(e) => {
                    eprintln!("Error decoding from server: {}", e);
                    continue;
                }
                Ok(x) => x,
            };

            if msg.disc {
                let _ = triggeradvsend2.send(()).await;
            }

            let _ = commactor
                .send(CommunicationActorMessage::ControlMessage(msg))
                .await;
        }
    });

    tokio::spawn(async move {
        let mut first = true;
        while let Some(()) = triggeradvsend_h.recv().await {
            if let Err(e) = servsend.send(&advertisment_b[..]).await {
                eprintln!("Error sending advert to server: {}", e);
            }

            if first {
                first = false;
                advertisment.disc = false;
                advertisment_b = serde_cbor::to_vec(&advertisment).unwrap();
            }
        }
    });

    // Periodic sender of pings

    let mut ticker = tokio::time::interval(Duration::from_secs(cs.keepalive_interval));
    loop {
        ticker.tick().await;
        triggeradvsend.send(()).await?;
    }
}
