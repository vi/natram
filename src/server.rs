use crate::{MAX_NAMES, Result, ServerMetrics, IP_MASK, PORT_MASK, ADDR_EXPIRE_SECONDS, NAME_EXPIRE_SECONDS, MAX_ADDRS};
use std::{time::{Duration, Instant}, net::SocketAddr};
use serde::{Serialize,Deserialize};
use serde_cbor::Value as CborValue;
use tokio::net::UdpSocket;

pub async fn server(sa: SocketAddr) -> Result<()> {
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
        p.po = Some(from.port() ^ PORT_MASK);
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
        let mut lone = true;
        for (x, ()) in addrs.iter() {
            if x == &from {
                continue;
            }
            lone = false;
            match u.send_to(&v[..], x).await {
                Ok(_) => metrics.sent += 1,
                Err(_) => metrics.snderr += 1,
            }
        }
        if lone {
            metrics.lone += 1;
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