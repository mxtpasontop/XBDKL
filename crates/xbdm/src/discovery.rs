//! Console discovery over UDP port 730 (the "name answering" protocol).
//!
//! Request:  `[0x03, 0x00]` asks every console to announce itself,
//!           `[0x01, len, name...]` asks one named console.
//! Reply:    `[0x02, len, name...]`, sent from the console's address.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::{Client, XBDM_PORT};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Console {
    pub name: String,
    pub ip: IpAddr,
}

/// Broadcasts a discovery request and collects replies until `timeout` elapses.
pub fn discover(timeout: Duration) -> Result<Vec<Console>> {
    let mut consoles = query(&[0x03, 0x00], timeout, false)?;
    // Some xbdm builds answer the broadcast with an empty name; ask them directly.
    for console in consoles.iter_mut().filter(|c| c.name.is_empty()) {
        if let Ok(name) = Client::connect((console.ip, XBDM_PORT)).and_then(|mut c| c.debug_name()) {
            console.name = name;
        }
    }
    Ok(consoles)
}

/// Looks up one console by debug name. Returns `None` if nothing answers in time.
pub fn resolve(name: &str, timeout: Duration) -> Result<Option<Console>> {
    let bytes = name.as_bytes();
    let len = u8::try_from(bytes.len())
        .map_err(|_| Error::InvalidArgument("console name is too long".into()))?;
    let mut request = vec![0x01, len];
    request.extend_from_slice(bytes);
    Ok(query(&request, timeout, true)?
        .into_iter()
        .find(|c| c.name.eq_ignore_ascii_case(name)))
}

fn query(request: &[u8], timeout: Duration, stop_at_first: bool) -> Result<Vec<Console>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.send_to(request, (Ipv4Addr::BROADCAST, XBDM_PORT))?;

    let deadline = Instant::now() + timeout;
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        socket.set_read_timeout(Some(remaining))?;
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e.into()),
        };
        if let Some(console) = parse_reply(&buf[..n], from)
            && seen.insert(console.ip)
        {
            found.push(console);
            if stop_at_first {
                break;
            }
        }
    }
    Ok(found)
}

fn parse_reply(packet: &[u8], from: SocketAddr) -> Option<Console> {
    let [0x02, len, rest @ ..] = packet else { return None };
    let name = rest.get(..*len as usize)?;
    Some(Console { name: String::from_utf8_lossy(name).into_owned(), ip: from.ip() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_parsing() {
        let from: SocketAddr = "192.168.1.50:730".parse().unwrap();
        let c = parse_reply(b"\x02\x04Jtag", from).unwrap();
        assert_eq!(c, Console { name: "Jtag".into(), ip: from.ip() });
        assert!(parse_reply(b"\x02\x09Jtag", from).is_none());
        assert!(parse_reply(b"\x03\x00", from).is_none());
    }
}
