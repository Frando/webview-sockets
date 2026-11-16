//! Native UDP echo server and client for testing the webview-sockets UDP demo page.
//!
//! `udp-echo listen [--port N]` binds a socket, prints its addresses and echoes
//! every datagram back to its sender. `udp-echo send <addr> <text>` sends one
//! datagram and prints the reply.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use n0_error::{Result, StdResultExt, anyerr};
use tokio::net::UdpSocket;

/// Largest UDP payload we accept.
const MAX_DATAGRAM: usize = 65535;
/// Datagrams printed in full before switching to periodic counts.
const PRINT_FIRST: u64 = 20;
/// Interval between count lines after the first datagrams.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// UDP echo tool for the webview-sockets UDP demo.
#[derive(Parser, Debug)]
#[command(name = "udp-echo")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Echo every received datagram back to its sender.
    Listen {
        /// UDP port to bind (0 picks a free one).
        #[arg(long, short, default_value_t = 0)]
        port: u16,
        /// Local IP to bind (`::` is dual-stack on Linux).
        #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
        bind: IpAddr,
        /// Text prepended to each echoed datagram (default: echo verbatim).
        #[arg(long, default_value = "")]
        prefix: String,
        /// Print every datagram instead of the first 20 and then counts.
        #[arg(long, short)]
        verbose: bool,
    },
    /// Send one datagram and print the reply.
    Send {
        /// Destination `ip:port` (or `host:port`).
        addr: String,
        /// Text to send.
        text: String,
        /// Seconds to wait for the reply.
        #[arg(long, default_value_t = 3.0)]
        timeout: f64,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Listen {
            port,
            bind,
            prefix,
            verbose,
        } => listen(SocketAddr::new(bind, port), prefix.into_bytes(), verbose).await,
        Command::Send {
            addr,
            text,
            timeout,
        } => send(&addr, &text, Duration::from_secs_f64(timeout)).await,
    }
}

async fn listen(bind: SocketAddr, prefix: Vec<u8>, verbose: bool) -> Result<()> {
    let sock = UdpSocket::bind(bind)
        .await
        .std_context(format!("bind {bind}"))?;
    let local = sock.local_addr().std_context("local addr")?;
    println!("bound {local}");
    for addr in interface_addrs(local) {
        println!("interface {addr}");
    }
    println!("echoing (prefix {:?})", String::from_utf8_lossy(&prefix));

    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut out = prefix.clone();
    let mut count = 0u64;
    let mut printed_count = 0u64;
    let mut last_stats = Instant::now();
    loop {
        let (len, from) = match sock.recv_from(&mut buf).await {
            Ok(r) => r,
            // ICMP port unreachable from an earlier send shows up here on some platforms.
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(err) => return Err(err).std_context("recv"),
        };
        count += 1;
        let data = &buf[..len];
        if verbose || count <= PRINT_FIRST {
            println!("{from} {len} bytes: {}", printable(data));
        } else if last_stats.elapsed() >= STATS_INTERVAL {
            println!("echoed {count} datagrams (+{})", count - printed_count);
            printed_count = count;
            last_stats = Instant::now();
        }
        out.truncate(prefix.len());
        out.extend_from_slice(data);
        if let Err(err) = sock.send_to(&out, from).await {
            eprintln!("send to {from}: {err}");
        }
    }
}

async fn send(addr: &str, text: &str, timeout: Duration) -> Result<()> {
    let peer = tokio::net::lookup_host(addr)
        .await
        .std_context(format!("resolve {addr}"))?
        .next()
        .ok_or_else(|| anyerr!("no address for {addr}"))?;
    let bind: SocketAddr = match peer {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let sock = UdpSocket::bind(bind).await.std_context("bind")?;
    let start = Instant::now();
    sock.send_to(text.as_bytes(), peer)
        .await
        .std_context(format!("send to {peer}"))?;
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let (len, from) = tokio::time::timeout(timeout, sock.recv_from(&mut buf))
        .await
        .map_err(|_| anyerr!("no reply from {peer} within {timeout:?}"))?
        .std_context("recv")?;
    println!(
        "{from} {len} bytes in {:.2} ms: {}",
        start.elapsed().as_secs_f64() * 1000.0,
        printable(&buf[..len])
    );
    Ok(())
}

/// Local interface addresses usable with a socket bound to `local`, with its port.
fn interface_addrs(local: SocketAddr) -> Vec<SocketAddr> {
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let v6 = local.is_ipv6();
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces,
        Err(err) => {
            eprintln!("listing interfaces: {err}");
            return Vec::new();
        }
    };
    ifaces
        .into_iter()
        .map(|iface| iface.ip())
        // A v6 socket on `::` is usually dual-stack; skip link-local v6 (needs a scope id).
        .filter(|ip| match ip {
            IpAddr::V4(_) => true,
            IpAddr::V6(ip) => v6 && !ip.is_unicast_link_local(),
        })
        .map(|ip| SocketAddr::new(ip, local.port()))
        .collect()
}

/// UTF-8 text if valid and short, else a length-limited hex dump.
fn printable(data: &[u8]) -> String {
    const MAX: usize = 120;
    match std::str::from_utf8(data) {
        Ok(text) if !text.chars().any(|c| c.is_control()) => {
            let mut s: String = text.chars().take(MAX).collect();
            if text.chars().count() > MAX {
                s.push_str("...");
            }
            format!("{s:?}")
        }
        _ => {
            let mut s: String = data
                .iter()
                .take(MAX / 2)
                .map(|b| format!("{b:02x}"))
                .collect();
            if data.len() > MAX / 2 {
                s.push_str("...");
            }
            format!("hex {s}")
        }
    }
}
