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
    #[structopt(long,default_value="12")]
    num_ports: u32,
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
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const MAX_NAMES : usize = 1024;
const MAX_ADDRS: usize = 16;
const NAME_EXPIRE_SECONDS : u64 = 120;
const ADDR_EXPIRE_SECONDS : u64 = 120;

use serde::{Serialize,Deserialize};
use serde_cbor::{Value as CborValue};

const IP_MASK : u32 = 0x44556677;
const PORT_MASK : u16 = 0x8899;

async fn server(sa: SocketAddr) -> Result<!> {
    use ttl_cache::TtlCache;

    type Registry = TtlCache<String, TtlCache<SocketAddr, ()>>;
    let mut registry : Registry = TtlCache::new(MAX_NAMES);

    /// Server's view on control message.
    #[derive(Serialize, Deserialize)]
    struct ControlMessage {
        na : String,
        ip : Option<u32>,
        po : Option<u16>,
        /// client fields are hidden here.
        #[serde(flatten)]
        _rest: CborValue,
    }

    let mut u = UdpSocket::bind(sa).await?;

    let mut buf = [0u8; 2048];

    async fn handle_packet(registry: &mut Registry, u: &mut UdpSocket, b : &[u8], from: SocketAddr) -> Result<()> {
        if b.len() == 0 {
            return Ok(());
        }
        let mut p : ControlMessage = serde_cbor::from_slice(b)?;
        p.ip = Some(match from.ip() {
            std::net::IpAddr::V4(a) => Into::<u32>::into(a) ^ IP_MASK,
            std::net::IpAddr::V6(_) => 0 ^ IP_MASK,
        });
        p.po = Some(from.port() & PORT_MASK);
        let v = serde_cbor::to_vec(&p)?;

        if p.na == "" {
            // Just send this back. Act as poor man's STUN.
            u.send_to(&v[..], from).await?;
            return Ok(());
        }
        
        let mut addrs = registry.remove(&p.na).unwrap_or_else(||TtlCache::new(MAX_ADDRS));

        // Broadcast this message (with filled in source address) to all other subscribed peers
        for (x,()) in addrs.iter() {
            if x == &from { continue }
            let _ = u.send_to(&v[..], x).await;
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
            Ok((len, from)) => if let Err(e) = handle_packet(&mut registry, &mut u, &buf[0..len], from).await {
                maybereporterr(e);
            },
            Err(e) => {
                maybereporterr(Box::new(e));
            },
        }
    }
}

async fn client(
    sa: SocketAddr,
    name: String,
    pco: PlayloadCommunicationOptions,
    cs : ClientSettings,
) -> Result<!> {
    let bindaddr = if let Some(x) = pco.bind { x } else {
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
    let mut s = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap()).await?;
    s.connect(sa).await?;


    unimplemented!()
}

#[tokio::main]
async fn main() -> Result<!> {
    use structopt::StructOpt;
    let opt = Opt::from_args();

    if opt.server.is_some() && opt.client.is_some() {
        Err("Can't be both --server and --client")?;
    }
    if opt.server.is_none() && opt.client.is_none() {
        Err("Please specify --server or --client")?;
    }
    if let Some(sa) = opt.server {
        if opt.pco.bind.is_some() || opt.pco.sendto.is_some() {
            Err("--bind or --sendto are meaningless in server mode")?;
        }
        if opt.cs.keepalive_interval != 30 {
            eprintln!("--keepalive-interval is meaningless in server mode");
        }
        server(sa).await
    } else {
        if opt.name.is_none() {
            Err("--name is required in client mode")?;
        }
        if opt.pco.bind.is_none() && opt.pco.sendto.is_none() {
            Err("Please specify --bind or --sendto in client mode")?;
        }
        let name = opt.name.unwrap();
        let sa = opt.client.unwrap();

        client(sa, name, opt.pco, opt.cs).await
    }
}
