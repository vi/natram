#![allow(unused)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use structopt::StructOpt;

use tokio::net::UdpSocket;

/// Options for client mode for communicating to other apps
#[derive(StructOpt)]
struct PlayloadCommunicationOptions {
    #[structopt(long, short = "b")]
    /// Bind to specified UDP port for actual communication.
    /// All incoming packets will be forwarded to the peer.
    /// Replies will be sent to most recently seen address.
    ///
    /// Only relevant in --client mode. If unset, bound port will be random.
    bind: Option<SocketAddr>,

    /// Send peer's packets to the specified
    /// UDP address and accept replies only from that address.
    ///
    /// Only relevant in --client mode. If unset, most recent address will be used.
    #[structopt(long, short = "t")]
    sendto: Option<SocketAddr>,
}

#[derive(StructOpt)]
struct ClientSettings {
    /// Keep-alive interval for clients, in seconds
    #[structopt(long, short = "i", default_value = "30")]
    keepalive_interval: u64,

    /// Number of ports to try
    #[structopt(long, default_value = "12")]
    num_ports: usize,
}

#[derive(StructOpt)]
struct Opt {
    /// Server mode: don't relay any traffic, only serve as a server.
    /// To be run on non-NAT public IPv4 address.
    /// Suggested address: 0.0.0.0:19929
    #[structopt(long, short = "s")]
    server: Option<SocketAddr>,

    /// Client mode: use specified socket as a setup server.
    /// Suggested address: 104.131.203.210:19929
    #[structopt(long, short = "c")]
    client: Option<SocketAddr>,

    /// Set name to match this peer and the other peer.
    ///
    /// Only relevant in --client mode.
    #[structopt(long, short = "n")]
    name: Option<String>,

    #[structopt(flatten)]
    pco: PlayloadCommunicationOptions,

    #[structopt(flatten)]
    cs: ClientSettings,

    /// Just retrieve and print statistics from server
    #[structopt(long)]
    stats: Option<SocketAddr>,
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const MAX_NAMES: usize = 1024;
const MAX_ADDRS: usize = 16;
const NAME_EXPIRE_SECONDS: u64 = 120;
const ADDR_EXPIRE_SECONDS: u64 = 120;

use serde::{Deserialize, Serialize};
use serde_cbor::Value as CborValue;

const IP_MASK: u32 = 0x44556677;
const PORT_MASK: u16 = 0x8899;


#[derive(Serialize, Deserialize, Default, Debug)]
struct ServerMetrics {
    pkt_rcv: u64,
    han_err: u64,
    rcv_err: u64,
    zeropkt: u64,
    me_rq: u64,
    ping: u64,
    named: u64,
    newnm: u64,
    sent: u64,
    snderr: u64,
}

async fn server(sa: SocketAddr) -> Result<!> {
    use ttl_cache::TtlCache;

    type Registry = TtlCache<String, TtlCache<SocketAddr, ()>>;
    let mut registry: Registry = TtlCache::new(MAX_NAMES);

    /// Server's view on control message.
    #[derive(Serialize, Deserialize)]
    struct ControlMessage {
        na: String,
        ip: Option<u32>,
        po: Option<u16>,
        /// client fields are hidden here.
        #[serde(flatten)]
        _rest: CborValue,
    }

    let mut metrics = ServerMetrics::default();

    let mut u = UdpSocket::bind(sa).await?;

    let mut buf = [0u8; 2048];

    async fn handle_packet(
        metrics: &mut ServerMetrics,
        registry: &mut Registry,
        u: &mut UdpSocket,
        b: &[u8],
        from: SocketAddr,
    ) -> Result<()> {
        if b.len() == 0 {
            metrics.zeropkt += 1;
            return Ok(());
        }
        if b.len() == 1 && b[0] == b'?' {
            metrics.me_rq+=1;
            let v = serde_cbor::to_vec(metrics)?;
            u.send_to(&v[..], from).await?;
            return Ok(());
            // metrics request
        }
        let mut p: ControlMessage = serde_cbor::from_slice(b)?;
        p.ip = Some(match from.ip() {
            std::net::IpAddr::V4(a) => Into::<u32>::into(a) ^ IP_MASK,
            std::net::IpAddr::V6(_) => 0 ^ IP_MASK,
        });
        p.po = Some(from.port() & PORT_MASK);
        let v = serde_cbor::to_vec(&p)?;

        if p.na == "" {
            metrics.ping += 1;
            // Just send this back. Act as poor man's STUN.
            u.send_to(&v[..], from).await?;
            return Ok(());
        }
        metrics.named += 1;

        let mut addrs = registry
            .remove(&p.na)
            .unwrap_or_else(|| {metrics.newnm+=1; TtlCache::new(MAX_ADDRS)});
        // Broadcast this message (with filled in source address) to all other subscribed peers
        for (x, ()) in addrs.iter() {
            if x == &from {
                continue;
            }
            match u.send_to(&v[..], x).await {
                Ok(_) => metrics.sent += 1,
                Err(_) => metrics.snderr += 1,
            }
        }

        addrs.insert(from, (), Duration::from_secs(ADDR_EXPIRE_SECONDS));

        registry.insert(p.na, addrs, Duration::from_secs(NAME_EXPIRE_SECONDS));
        Ok(())
    }

    let mut error_ratelimiter = Instant::now();
    let mut maybereporterr = |e| {
        if Instant::now() >= error_ratelimiter {
            eprintln!("error: {}", e);
            error_ratelimiter = Instant::now() + Duration::from_millis(50);
        }
    };
    loop {
        match u.recv_from(&mut buf[..]).await {
            Ok((len, from)) => {
                metrics.pkt_rcv+=1;
                if let Err(e) =
                    handle_packet(&mut metrics, &mut registry, &mut u, &buf[0..len], from).await
                {
                    metrics.han_err+=1;
                    maybereporterr(e);
                }
            }
            Err(e) => {
                metrics.rcv_err+=1;
                maybereporterr(Box::new(e));
            }
        }
    }
}

async fn client(
    sa: SocketAddr,
    name: String,
    pco: PlayloadCommunicationOptions,
    cs: ClientSettings,
) -> Result<!> {
    use tokio::sync::mpsc::{channel, Sender, Receiver};
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

    /// Socket for communicating with server
    let mut server = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap()).await?;
    server.connect(sa).await?;

    struct Port {
        u : UdpSocket,
        p : u16,
        buddy: Option<SocketAddr>,
    }

    let mut sanity_limit : usize = 65536;
    /// UDP sockets with random ports
    let mut ports : Vec<Port> = Vec::with_capacity(cs.num_ports);

    let mut rnd = rand::thread_rng();
    use rand::RngCore;

    loop {
        let p : u16 = (rnd.next_u32() & 0xFFFF) as u16;
        if p <= 1024 { continue }

        let nullip = std::net::Ipv4Addr::UNSPECIFIED;
        if let Ok(u) = UdpSocket::bind(SocketAddr::V4(std::net::SocketAddrV4::new(nullip, p))).await {
            ports.push(Port { u, p, buddy: None});
        }

        if ports.len() == cs.num_ports { break; }

        sanity_limit-=1;
        if sanity_limit == 0 {
            break;
        }
    }

    // Not `cs.num_ports` UDP sockets should be created
    if ports.is_empty() {
        Err("Failed to open any ports for communication")?;
    }

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

    let mut advertisment = OutgoingControlMessage {
        na: name,
        ports: ports.iter().map(|pp|pp.p ^ PORT_MASK).collect(),
        disc: true,
    };
    let mut advertisment_b = serde_cbor::to_vec(&advertisment)?;

    #[derive(Deserialize,Debug)]
    struct IncomingControlMessage {
        na: String,
        ip: u32,
        po: u16,
        ports: Vec<u16>,
        disc: bool,
    }

    let (mut servrecv, mut servsend) = server.split();

    /// Sender of messages to server
    let (mut triggeradvsend, mut triggeradvsend_h) = channel::<()>(1);
    let mut triggeradvsend2 = triggeradvsend.clone();

    /// Handler of incoming adverts
    tokio::spawn(async move {
        loop {
            let mut buf = [0u8; 2048];
            let len = match servrecv.recv(&mut buf[..]).await {
                Err(e) => {
                    eprintln!("error receiving from server: {}", e);
                    tokio::time::delay_for(Duration::from_millis(50)).await;
                    continue
                }
                Ok(x) => x,
            };
            let msg : IncomingControlMessage = match serde_cbor::from_slice(&buf[0..len]) {
                Err(e) => {
                    eprintln!("Error decoding from server: {}", e);
                    continue
                }
                Ok(x) => x,
            };

            if msg.disc {
                let _ = triggeradvsend2.send(()).await;
            }

            eprintln!("Incoming message from server: {:?}", msg);

            let peer_ip : std::net::Ipv4Addr = (msg.ip ^ IP_MASK).into();

            let usable_ports_n = msg.ports.len().min(ports.len());



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

#[tokio::main]
async fn main() -> Result<()> {
    use structopt::StructOpt;
    let opt = Opt::from_args();

    let mut modeopts = 0;
    if opt.server.is_some() { modeopts += 1  }
    if opt.client.is_some() { modeopts += 1  }
    if opt.stats.is_some() { modeopts += 1  }

    if modeopts != 1 {
        Err("Specify exactly one of --server, --client or --stats")?;
    }
    
    if let Some(sa) = opt.stats {
        let mut s = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap()).await?;
        s.connect(sa).await?;
        s.send(b"?").await?;
        let mut b = [0u8; 512];
        let l = s.recv(&mut b[..]).await?;
        let b = &b[0..l];
        let metrics : ServerMetrics = serde_cbor::from_slice(b)?;
        //csv::Writer::from_writer(std::io::stdout()).serialize(metrics);
        println!("{}",serde_yaml::to_string(&metrics)?);
        //println!("{:#?}", metrics);
        return Ok(());
    }

    if let Some(sa) = opt.server {
        if opt.pco.bind.is_some() || opt.pco.sendto.is_some() {
            Err("--bind or --sendto are meaningless in server mode")?;
        }
        if opt.cs.keepalive_interval != 30 {
            eprintln!("--keepalive-interval is meaningless in server mode");
        }
        server(sa).await?
    } else {
        if opt.name.is_none() {
            Err("--name is required in client mode")?;
        }
        if opt.pco.bind.is_none() && opt.pco.sendto.is_none() {
            Err("Please specify --bind or --sendto in client mode")?;
        }
        let name = opt.name.unwrap();
        let sa = opt.client.unwrap();

        client(sa, name, opt.pco, opt.cs).await?
    }
}
