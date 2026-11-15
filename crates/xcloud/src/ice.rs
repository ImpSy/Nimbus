//! ICE candidate helpers: parsing the server's candidates (including the
//! Teredo trick used for remote play) and gathering our host candidates.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::api::{IceCandidateJson, ServerDetails};

/// Default port the console's nano streaming service listens on.
pub const CONSOLE_DEFAULT_PORT: u16 = 9002;

/// Decode a Teredo (2001:0::/32) address into the client's public IPv4 and
/// mapped UDP port. Consoles behind NAT often advertise their Teredo address;
/// the embedded mapping is usually directly reachable over IPv4.
pub fn teredo_client(addr: Ipv6Addr) -> Option<(Ipv4Addr, u16)> {
    let s = addr.segments();
    if s[0] != 0x2001 || s[1] != 0x0000 {
        return None;
    }
    let port = s[5] ^ 0xffff;
    let hi = s[6] ^ 0xffff;
    let lo = s[7] ^ 0xffff;
    let ip = Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    Some((ip, port))
}

/// A parsed remote candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteCandidate {
    pub addr: SocketAddr,
    /// Higher is preferred.
    pub priority: u32,
}

/// Parse one SDP candidate line (`a=candidate:...` or `candidate:...`).
pub fn parse_candidate_line(line: &str) -> Option<RemoteCandidate> {
    let line = line.trim();
    let line = line.strip_prefix("a=").unwrap_or(line);
    let rest = line.strip_prefix("candidate:")?;
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let component: u32 = parts[1].parse().ok()?;
    if component != 1 || !parts[2].eq_ignore_ascii_case("udp") {
        return None;
    }
    let priority: u32 = parts[3].parse().ok()?;
    let ip: IpAddr = parts[4].parse().ok()?;
    let port: u16 = parts[5].parse().ok()?;
    Some(RemoteCandidate {
        addr: SocketAddr::new(ip, port),
        priority,
    })
}

/// Turn the server's candidate list into a de-duplicated list of socket
/// addresses to try, expanding Teredo addresses and adding the console's
/// directly-reported addresses (remote play).
pub fn remote_candidates(
    candidates: &[IceCandidateJson],
    server: Option<&ServerDetails>,
) -> Vec<RemoteCandidate> {
    let mut out: Vec<RemoteCandidate> = Vec::new();
    let mut push = |c: RemoteCandidate| {
        if !out.iter().any(|o| o.addr == c.addr) {
            out.push(c);
        }
    };

    for c in candidates {
        if c.candidate.contains("end-of-candidates") {
            continue;
        }
        let Some(rc) = parse_candidate_line(&c.candidate) else {
            tracing::debug!(candidate = %c.candidate, "ignoring unparsable candidate");
            continue;
        };
        if let IpAddr::V6(v6) = rc.addr.ip()
            && let Some((ip, port)) = teredo_client(v6)
        {
            tracing::info!(%ip, port, "expanded Teredo candidate");
            push(RemoteCandidate {
                addr: SocketAddr::new(ip.into(), port),
                priority: rc.priority.saturating_add(1),
            });
            push(RemoteCandidate {
                addr: SocketAddr::new(ip.into(), CONSOLE_DEFAULT_PORT),
                priority: rc.priority,
            });
        }
        push(rc);
    }

    if let Some(s) = server {
        let pairs = [
            (&s.ip_address, s.port),
            (&s.ipv4_address, s.ipv4_port),
            (&s.ipv6_address, s.ipv6_port),
        ];
        for (ip, port) in pairs {
            let Some(ip) = ip.as_deref().and_then(|ip| ip.parse::<IpAddr>().ok()) else {
                continue;
            };
            let ports: BTreeSet<u16> = port.into_iter().chain([CONSOLE_DEFAULT_PORT]).collect();
            for p in ports {
                push(RemoteCandidate {
                    addr: SocketAddr::new(ip, p),
                    priority: 1,
                });
            }
        }
    }

    out
}

fn is_usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            // Global unicast only (2000::/3); skip link-local / ULA / loopback.
            (first & 0xe000) == 0x2000
        }
    }
}

/// Local IP addresses usable as host candidates, IPv4 first.
pub fn local_addresses() -> Vec<IpAddr> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                let ip = iface.ip();
                if !is_usable(&ip) {
                    continue;
                }
                // Skip common virtual bridges that never route to the internet.
                let name = iface.name.as_str();
                if name.starts_with("docker") || name.starts_with("br-") || name.starts_with("veth")
                {
                    continue;
                }
                match ip {
                    IpAddr::V4(_) if !v4.contains(&ip) => v4.push(ip),
                    IpAddr::V6(_) if !v6.contains(&ip) => v6.push(ip),
                    _ => {}
                }
            }
        }
        Err(e) => tracing::warn!("failed to list interfaces: {e}"),
    }
    v4.truncate(4);
    v6.truncate(2);
    v4.extend(v6);
    v4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teredo_decoding() {
        // Example from RFC 4380: 2001:0:4136:e378:8000:63bf:3fff:fdd2
        // -> client 192.0.2.45, port 40000
        let a: Ipv6Addr = "2001:0:4136:e378:8000:63bf:3fff:fdd2".parse().unwrap();
        assert_eq!(teredo_client(a), Some((Ipv4Addr::new(192, 0, 2, 45), 40000)));
        let b: Ipv6Addr = "2603:1076:201:83::AB8:E9FE".parse().unwrap();
        assert_eq!(teredo_client(b), None);
    }

    #[test]
    fn parses_server_candidates() {
        let c = vec![
            IceCandidateJson {
                candidate: "a=candidate:1 1 UDP 100 198.51.100.10 1136 typ host ".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            },
            IceCandidateJson {
                candidate: "a=candidate:2 1 UDP 1 2001:0:4136:e378:8000:63bf:3fff:fdd2 9002 typ host ".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            },
            IceCandidateJson {
                candidate: "a=end-of-candidates".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            },
        ];
        let server = ServerDetails {
            ip_address: Some("192.168.1.20".into()),
            port: Some(1234),
            ..Default::default()
        };
        let r = remote_candidates(&c, Some(&server));
        let addrs: Vec<String> = r.iter().map(|c| c.addr.to_string()).collect();
        assert_eq!(
            addrs,
            vec![
                "198.51.100.10:1136",
                "192.0.2.45:40000",
                "192.0.2.45:9002",
                "[2001:0:4136:e378:8000:63bf:3fff:fdd2]:9002",
                "192.168.1.20:1234",
                "192.168.1.20:9002",
            ]
        );
    }
}

/// Minimal STUN (RFC 5389) binding client used to learn our public address
/// (server-reflexive candidate). Helps remote play when the console is behind
/// another NAT.
pub mod stun {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    const MAGIC: u32 = 0x2112_A442;

    pub const SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

    pub fn binding_request(tid: [u8; 12]) -> [u8; 20] {
        let mut b = [0u8; 20];
        b[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
        b[2..4].copy_from_slice(&0u16.to_be_bytes());
        b[4..8].copy_from_slice(&MAGIC.to_be_bytes());
        b[8..20].copy_from_slice(&tid);
        b
    }

    /// Parse a binding success response, returning the XOR-MAPPED-ADDRESS.
    pub fn parse_response(data: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
        if data.len() < 20 || data[0..2] != [0x01, 0x01] || data[4..8] != MAGIC.to_be_bytes() {
            return None;
        }
        if &data[8..20] != tid {
            return None;
        }
        let len = u16::from_be_bytes([data[2], data[3]]) as usize;
        let body = data.get(20..20 + len)?;
        let mut i = 0;
        while i + 4 <= body.len() {
            let kind = u16::from_be_bytes([body[i], body[i + 1]]);
            let alen = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
            let v = body.get(i + 4..i + 4 + alen)?;
            if kind == 0x0020 && v.len() >= 8 {
                let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC >> 16) as u16;
                let ip: IpAddr = match v[1] {
                    0x01 => {
                        let x = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC;
                        Ipv4Addr::from(x).into()
                    }
                    0x02 if v.len() >= 20 => {
                        let mut key = [0u8; 16];
                        key[..4].copy_from_slice(&MAGIC.to_be_bytes());
                        key[4..].copy_from_slice(tid);
                        let mut a = [0u8; 16];
                        for k in 0..16 {
                            a[k] = v[4 + k] ^ key[k];
                        }
                        Ipv6Addr::from(a).into()
                    }
                    _ => return None,
                };
                return Some(SocketAddr::new(ip, port));
            }
            i += 4 + alen.div_ceil(4) * 4;
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip_xor_mapped_address() {
            let tid = [7u8; 12];
            // Build a response for 203.0.113.5:54321.
            let ip = u32::from(Ipv4Addr::new(203, 0, 113, 5)) ^ MAGIC;
            let port = 54321u16 ^ (MAGIC >> 16) as u16;
            let mut attr = vec![0x00, 0x20, 0x00, 0x08, 0x00, 0x01];
            attr.extend_from_slice(&port.to_be_bytes());
            attr.extend_from_slice(&ip.to_be_bytes());
            let mut msg = vec![0x01, 0x01];
            msg.extend_from_slice(&(attr.len() as u16).to_be_bytes());
            msg.extend_from_slice(&MAGIC.to_be_bytes());
            msg.extend_from_slice(&tid);
            msg.extend_from_slice(&attr);
            assert_eq!(
                parse_response(&msg, &tid),
                Some("203.0.113.5:54321".parse().unwrap())
            );
            assert_eq!(binding_request(tid)[0..2], [0, 1]);
        }
    }
}
