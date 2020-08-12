#![allow(unused)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use structopt::StructOpt;

use tokio::net::UdpSocket;

/// Options for client mode for communicating to other apps
#[derive(StructOpt)]
pub struct PlayloadCommunicationOptions {
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
pub struct ClientSettings {
    /// Keep-alive interval for clients, in seconds
    #[structopt(long, short = "i", default_value = "30")]
    keepalive_interval: u64,

    /// Number of ports to try
    #[structopt(long, default_value = "12")]
    num_ports: usize,

    /// Ignore received IP address, use this one instead
    #[structopt(long)]
    override_sendto_addr: Option<std::net::Ipv4Addr>,
    
    /// Resolve each port's mapped number (like in STUN) before processing
    #[structopt(long, short = "S")]
    stun: bool,
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
    lone: u64,
}

mod client;
mod server;

#[tokio::main]
async fn main() -> Result<()> {
    use structopt::StructOpt;
    let opt = Opt::from_args();

    let mut modeopts : usize = 0;
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
        server::server(sa).await?;
    } else {
        if opt.name.is_none() {
            Err("--name is required in client mode")?;
        }
        if opt.pco.bind.is_none() && opt.pco.sendto.is_none() {
            Err("Please specify --bind or --sendto in client mode")?;
        }
        let name = opt.name.unwrap();
        let sa = opt.client.unwrap();

        client::client(sa, name, opt.pco, opt.cs).await?;
    }
    Ok(())
}
