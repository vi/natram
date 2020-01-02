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

struct CommunicationActorState {
    ports: Vec<SendHalf>,
    client_tx: SendHalf,
    client_peer: Option<SocketAddr>,
}

impl CommunicationActorState {
    async fn msg(&mut self, message: CommunicationActorMessage) -> Result<()> {
        Ok(())
    }
}

pub async fn client(
    server_addr: SocketAddr,
    name: String,
    pco: PlayloadCommunicationOptions,
    cs: ClientSettings,
) -> Result<!> {
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
    // Basic setup complete. Proceed to starting agents.
    // ----------

    let (mut client_rx, mut client_tx) = client.split();
    let (mut servrecv, mut servsend) = server.split();

    /// Sender of messages to server
    let (mut triggeradvsend, mut triggeradvsend_h) = channel::<()>(1);
    let mut triggeradvsend2 = triggeradvsend.clone();

    let (mut commagent, mut commagent_h) = channel::<CommunicationActorMessage>(32);

    use unzip3::Unzip3;
    let (ports_tx, ports_rx, port_numbers): (Vec<SendHalf>, Vec<RecvHalf>, Vec<u16>) = ports
        .into_iter()
        .map(|pp| (pp.u_tx, pp.u_rx, pp.p))
        .unzip3();

    /// Main, central agent:
    let mut commagent_st = CommunicationActorState {
        ports: ports_tx,
        client_tx,
        client_peer: sendaddr,
    };
    tokio::spawn(async move {
        while let Some(msg) = commagent_h.recv().await {
            if let Err(e) = commagent_st.msg(msg).await {
                eprintln!("Error: {}", e);
            }
        }
    });
    /// Data port receivers:
    for (i, mut u_rx) in ports_rx.into_iter().enumerate() {
        let mut commagent = commagent.clone();
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
                        let _ = commagent
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
    let mut commagent2 = commagent.clone();
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
                            let _ = commagent2.send(CommunicationActorMessage::NewClientPeerAddress(from)).await;
                        }
                    }
                    let _ = commagent2.send(CommunicationActorMessage::ClientData(buf[0..len].to_vec())).await;
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

            let _ = commagent
                .send(CommunicationActorMessage::ControlMessage(msg))
                .await;
            //eprintln!("Incoming message from server: {:?}", msg);

            //let peer_ip : std::net::Ipv4Addr = (msg.ip ^ IP_MASK).into();

            //let usable_ports_n = msg.ports.len().min(ports.len());

            //todo!();
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
