//! Finding a logger on the LAN by its serial.
//!
//! The logger answers a plaintext hello broadcast on its own UDP port with one line —
//! `ip,mac,serial`. Only the serial is read, and only to tell our logger from a neighbour's: the
//! MAC is parsed past and never stored, because a MAC is a home-network detail and those do not
//! enter this repository.
//!
//! Three ports and two hellos are tried because the logger's firmware is not consistent about
//! which it answers on, and a broadcast is cheap. Which combination a given unit replies to is a
//! question only the real hardware settles.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout_at};

/// UDP ports a logger has been seen listening on for the discovery hello.
pub const PORTS: [u16; 3] = [48899, 58899, 8899];

/// The discovery requests. Read-only: each only asks the logger to describe itself.
const HELLOS: [&[u8]; 2] = [b"WIFIKIT-214028-READ", b"Link_Status"];

/// Every logger on the segment hears these; no router forwards them.
#[must_use]
pub fn broadcast_targets() -> Vec<SocketAddr> {
    PORTS
        .iter()
        .map(|&port| SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port))
        .collect()
}

/// Say hello at every `target`; the logger carrying `serial` is wherever its reply came from.
/// `None` if it has not answered within `limit`.
///
/// Every hello goes out before any reply is waited for, so `limit` bounds the whole search rather
/// than each port in turn.
///
/// # Errors
///
/// If the socket cannot be opened or configured for broadcast. A send that fails is not fatal —
/// one closed port must not call off the search — and neither is a failed receive: probing a port
/// nothing listens on draws an ICMP unreachable, which surfaces here as a receive error on a
/// socket that is otherwise fine.
pub async fn find(
    serial: u32,
    targets: &[SocketAddr],
    limit: Duration,
) -> io::Result<Option<IpAddr>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sock.set_broadcast(true)?;
    let mut sent = 0_usize;
    for target in targets {
        for hello in HELLOS {
            if sock.send_to(hello, target).await.is_ok() {
                sent += 1;
            }
        }
    }
    if sent == 0 {
        return Ok(None);
    }
    let deadline = Instant::now() + limit;
    let mut buf = [0u8; 256];
    while let Ok(recv) = timeout_at(deadline, sock.recv_from(&mut buf)).await {
        let Ok((n, from)) = recv else {
            continue; // an unreachable port, not the end of the search
        };
        if buf.get(..n).and_then(serial_of) == Some(serial) {
            return Ok(Some(from.ip()));
        }
    }
    Ok(None)
}

/// The serial from a logger's `ip,mac,serial` reply, serial in decimal.
fn serial_of(reply: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(reply).ok()?;
    text.trim().split(',').nth(2)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never waited out: every test here gets the reply it waits for.
    const AMPLE: Duration = Duration::from_secs(5);

    /// Where a hello is answered with each `(from, body)` in turn, each sent from its own socket
    /// on the loopback address `from`.
    async fn loggers(replies: &[(&str, &'static str)]) -> SocketAddr {
        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let at = listener.local_addr().expect("addr");
        let mut senders = Vec::new();
        for &(from, body) in replies {
            let sock = UdpSocket::bind((from, 0)).await.expect("bind");
            senders.push((sock, body));
        }
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((n, asker)) = listener.recv_from(&mut buf).await {
                if !HELLOS.contains(&buf.get(..n).unwrap_or_default()) {
                    continue;
                }
                for (sock, body) in &senders {
                    sock.send_to(body.as_bytes(), asker).await.ok();
                }
            }
        });
        at
    }

    #[tokio::test]
    async fn our_logger_is_found_where_it_answered_from() {
        // The body names another address; the source is the one that answered.
        let at = loggers(&[("127.0.0.1", "192.0.2.10,ACDE48001122,3735928559")]).await;
        let found = find(0xDEAD_BEEF, &[at], AMPLE).await.expect("sends");
        assert_eq!(found, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[tokio::test]
    async fn replies_that_are_not_ours_are_passed_over() {
        // A reply that is not a logger's, then a neighbour's logger, then ours.
        let at = loggers(&[
            ("127.0.0.1", "hello"),
            ("127.0.0.1", "192.0.2.10,ACDE48001122,1"),
            ("127.0.0.2", "192.0.2.10,ACDE48001122,3735928559"),
        ])
        .await;
        let found = find(0xDEAD_BEEF, &[at], AMPLE).await.expect("sends");
        assert_eq!(found, Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))));
    }

    #[tokio::test]
    async fn a_port_nothing_listens_on_does_not_call_off_the_search() {
        // The first target draws an ICMP unreachable. Abandoning the search on it would mean one
        // dead port hid a logger answering on another.
        let dead = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let closed = dead.local_addr().expect("addr");
        drop(dead);
        let at = loggers(&[("127.0.0.1", "192.0.2.10,ACDE48001122,3735928559")]).await;
        let found = find(0xDEAD_BEEF, &[closed, at], AMPLE)
            .await
            .expect("sends");
        assert_eq!(found, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[tokio::test]
    async fn a_logger_that_never_answers_gives_up_rather_than_waiting_out_the_slot() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let at = silent.local_addr().expect("addr");
        let found = find(0xDEAD_BEEF, &[at], Duration::from_millis(50))
            .await
            .expect("sends");
        assert_eq!(found, None);
    }

    /// A logger that answers only `want` and ignores the other hello.
    async fn picky_logger(want: &'static [u8], body: &'static str) -> SocketAddr {
        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let at = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((n, asker)) = listener.recv_from(&mut buf).await {
                if buf.get(..n) == Some(want) {
                    listener.send_to(body.as_bytes(), asker).await.ok();
                }
            }
        });
        at
    }

    #[tokio::test]
    async fn a_logger_answering_only_the_second_hello_is_still_found() {
        // Which hello a unit replies to is firmware-dependent, so both go out. A logger deaf to
        // the first must still be found, which is the only thing that proves the second is sent.
        let at = picky_logger(b"Link_Status", "192.0.2.10,ACDE48001122,3735928559").await;
        let found = find(0xDEAD_BEEF, &[at], AMPLE).await.expect("sends");
        assert_eq!(found, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn a_target_is_made_for_every_port() {
        let ports: Vec<u16> = broadcast_targets().iter().map(SocketAddr::port).collect();
        assert_eq!(ports, PORTS);
    }
}
