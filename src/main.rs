#![allow(unused)]

use std::net::SocketAddr;
use std::time::Duration;

#[derive(structopt::StructOpt)]
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

    /// Set name to match this peer and the other peer.
    ///
    /// Only relevant in --client mode.
    #[structopt(long, short = "n")]
    name: Option<String>,

    /// Keep-alive interval for clients, in seconds
    #[structopt(long, short = "i", default_value = "30")]
    keepalive_interval: u64,
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn server(sa: SocketAddr) -> Result<!> {
    unimplemented!()
}

async fn client(
    sa: SocketAddr,
    name: String,
    bind: SocketAddr,
    sendto: Option<SocketAddr>,
    keepalive_interval: Duration,
) -> Result<!> {
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
        if opt.bind.is_some() || opt.sendto.is_some() {
            Err("--bind or --sendto are meaningless in server mode")?;
        }
        if opt.keepalive_interval != 30 {
            eprintln!("--keepalive-interval is meaningless in server mode");
        }
        server(sa).await
    } else {
        if opt.name.is_none() {
            Err("--name is required in client mode")?;
        }
        if opt.bind.is_none() && opt.sendto.is_none() {
            Err("Please specify --bind or --sendto in client mode")?;
        }

        let sa = opt.client.unwrap();
        let bindaddr = if let Some(x) = opt.bind { x } else {
            if let Some(SocketAddr::V4(st)) = opt.sendto {
                if st.ip().is_loopback() {
                    "127.0.0.1:0".parse().unwrap()
                } else {
                    "0.0.0.0:0".parse().unwrap()
                }
            } else {
                "0.0.0.0:0".parse().unwrap()
            }
        };
        let name = opt.name.unwrap();
        let keepalive_interval = Duration::from_secs(opt.keepalive_interval);

        client(sa, name, bindaddr, opt.sendto, keepalive_interval).await
    }
}
