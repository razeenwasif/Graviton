//! Minimal RFC 1035 DNS message parsing, local container lookup, and reply
//! building for the guest resolver.
//!
//! Pure functions on wire bytes, so everything runs and tests on macOS as well
//! as inside the Linux guest (`graviton-agent`). Unknown external names return
//! `None` from [`answer`], telling the guest resolver to forward the raw query
//! untouched to the NAT's upstream DNS server.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::HostEntry;

/// Short TTL for container records: IPs come from NAT DHCP and can change across restarts.
pub const CONTAINER_TTL: u32 = 5;

/// Optional local domain suffix (`redis.graviton` resolves alongside `redis`).
pub const LOCAL_DOMAIN: &str = "graviton";

/// Standard DNS record types we answer locally.
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_ANY: u16 = 255;

const CLASS_IN: u16 = 1;
const CLASS_ANY: u16 = 255;

const RCODE_NOERROR: u16 = 0;
const RCODE_SERVFAIL: u16 = 2;
const RCODE_NXDOMAIN: u16 = 3;

/// Live container name/id ↔ IPv4 table used by the guest DNS responder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostTable {
    /// Lowercased container name or id → IPv4 address.
    by_name: HashMap<String, Ipv4Addr>,
    /// IPv4 address → canonical container name (for reverse PTR lookups).
    by_ip: HashMap<Ipv4Addr, String>,
}

impl HostTable {
    pub fn from_entries(entries: &[HostEntry]) -> Self {
        let mut by_name = HashMap::new();
        let mut by_ip = HashMap::new();
        // Insert IDs first so an explicit container name always wins if they ever collide.
        for e in entries {
            let Ok(ip) = e.ip.parse::<Ipv4Addr>() else { continue };
            if !e.id.is_empty() {
                by_name.insert(e.id.to_ascii_lowercase(), ip);
            }
        }
        for e in entries {
            let Ok(ip) = e.ip.parse::<Ipv4Addr>() else { continue };
            if !e.name.is_empty() {
                by_name.insert(e.name.to_ascii_lowercase(), ip);
                by_ip.entry(ip).or_insert_with(|| e.name.clone());
            }
        }
        Self { by_name, by_ip }
    }

    pub fn lookup_name(&self, qname_lower: &str) -> Lookup {
        if let Some(&ip) = self.by_name.get(qname_lower) {
            return Lookup::Found(ip);
        }
        let suffix = format!(".{LOCAL_DOMAIN}");
        if let Some(base) = qname_lower.strip_suffix(&suffix) {
            return match self.by_name.get(base) {
                Some(&ip) => Lookup::Found(ip),
                None => Lookup::NxDomain,
            };
        }
        if qname_lower == LOCAL_DOMAIN {
            return Lookup::NxDomain;
        }
        Lookup::Forward
    }

    pub fn lookup_ip(&self, ip: Ipv4Addr) -> Option<&str> {
        self.by_ip.get(&ip).map(String::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    Found(Ipv4Addr),
    NxDomain,
    Forward,
}

struct Question {
    /// Lowercased dot-joined name without trailing dot (empty string for root).
    name: String,
    qtype: u16,
    qclass: u16,
    /// Byte offset in the original packet immediately after the Question section.
    end: usize,
}

/// Answer a raw DNS query packet locally if it targets a container name, a
/// `*.graviton` name, or a container's reverse-DNS IP. Returns `None` when the
/// query should be forwarded verbatim to the upstream NAT DNS server.
pub fn answer(pkt: &[u8], table: &HostTable) -> Option<Vec<u8>> {
    let q = parse_query(pkt)?;
    if q.qclass != CLASS_IN && q.qclass != CLASS_ANY {
        return None;
    }

    // Reverse lookup: d.c.b.a.in-addr.arpa
    if let Some(ip) = parse_reverse_ipv4(&q.name) {
        let name = table.lookup_ip(ip)?;
        if q.qtype == TYPE_PTR || q.qtype == TYPE_ANY {
            if let Some(rdata) = encode_name(name) {
                return Some(build_reply(pkt, q.end, RCODE_NOERROR, true, Some((TYPE_PTR, &rdata))));
            }
        }
        return Some(build_reply(pkt, q.end, RCODE_NOERROR, true, None));
    }

    match table.lookup_name(&q.name) {
        Lookup::Found(ip) => {
            if q.qtype == TYPE_A || q.qtype == TYPE_ANY {
                let octets = ip.octets();
                Some(build_reply(pkt, q.end, RCODE_NOERROR, true, Some((TYPE_A, &octets))))
            } else {
                // Known container name, non-A query (e.g. AAAA): return NODATA so
                // dual-stack resolvers (musl/glibc/Go) immediately use the A record
                // instead of querying upstream and getting NXDOMAIN.
                Some(build_reply(pkt, q.end, RCODE_NOERROR, true, None))
            }
        }
        Lookup::NxDomain => Some(build_reply(pkt, q.end, RCODE_NXDOMAIN, true, None)),
        Lookup::Forward => None,
    }
}

/// Build a `SERVFAIL` response for `pkt` when no upstream DNS server answered.
pub fn servfail(pkt: &[u8]) -> Option<Vec<u8>> {
    if pkt.len() < 12 {
        return None;
    }
    let req_flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if (req_flags & 0x8000) != 0 {
        return None;
    }
    if let Some(q) = parse_query(pkt) {
        return Some(build_reply(pkt, q.end, RCODE_SERVFAIL, false, None));
    }
    let flags = 0x8080 | (req_flags & 0x0100) | RCODE_SERVFAIL;
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&pkt[0..2]);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&[0; 8]);
    Some(out)
}

/// Extract upstream DNS server addresses (`ip:53`) from `/etc/resolv.conf`,
/// ignoring comments, malformed lines, and `skip` (the local resolver itself).
pub fn parse_resolv_conf(contents: &str, skip: Option<IpAddr>) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.split(['#', ';']).next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        let Some(raw) = parts.next() else { continue };
        let Ok(ip) = raw.parse::<IpAddr>() else { continue };
        if skip == Some(ip) {
            continue;
        }
        let addr = SocketAddr::new(ip, 53);
        if !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

fn parse_query(pkt: &[u8]) -> Option<Question> {
    if pkt.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    // Only handle standard queries (QR = 0, OPCODE = 0) with exactly one question.
    if (flags & 0x8000) != 0 || (flags & 0x7800) != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]);
    if qdcount != 1 {
        return None;
    }

    let mut pos = 12usize;
    let mut name = String::new();
    loop {
        let len = *pkt.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        // Reject compression pointers (>= 0xC0) or invalid labels (> 63) in QNAME.
        if len > 63 || pos + len > pkt.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        for &b in &pkt[pos..pos + len] {
            if !b.is_ascii() {
                return None;
            }
            name.push((b as char).to_ascii_lowercase());
        }
        if name.len() > 255 {
            return None;
        }
        pos += len;
    }

    if pos + 4 > pkt.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([pkt[pos], pkt[pos + 1]]);
    let qclass = u16::from_be_bytes([pkt[pos + 2], pkt[pos + 3]]);
    Some(Question { name, qtype, qclass, end: pos + 4 })
}

fn build_reply(pkt: &[u8], question_end: usize, rcode: u16, authoritative: bool, record: Option<(u16, &[u8])>) -> Vec<u8> {
    let req_flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    let aa = if authoritative { 0x0400 } else { 0 };
    // QR=1, AA=aa, TC=0, RD=copied, RA=1, RCODE=rcode.
    let flags: u16 = 0x8080 | aa | (req_flags & 0x0100) | (rcode & 0x000F);
    let ancount: u16 = if record.is_some() { 1 } else { 0 };

    let extra = record.map_or(0, |(_, rdata)| 12 + rdata.len());
    let mut out = Vec::with_capacity(question_end + extra);
    out.extend_from_slice(&pkt[0..2]); // ID
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&ancount.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&pkt[12..question_end]); // Question echoed verbatim (preserves case)

    if let Some((rtype, rdata)) = record {
        out.extend_from_slice(&[0xC0, 0x0C]); // NAME: compression pointer to QNAME at offset 12
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&CONTAINER_TTL.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
    }
    out
}

fn parse_reverse_ipv4(qname_lower: &str) -> Option<Ipv4Addr> {
    let prefix = qname_lower.strip_suffix(".in-addr.arpa")?;
    let mut parts = prefix.split('.');
    let d: u8 = parts.next()?.parse().ok()?;
    let c: u8 = parts.next()?.parse().ok()?;
    let b: u8 = parts.next()?.parse().ok()?;
    let a: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Ipv4Addr::new(a, b, c, d))
}

fn encode_name(name: &str) -> Option<Vec<u8>> {
    let trimmed = name.trim_end_matches('.');
    if trimmed.is_empty() || trimmed.len() > 253 {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() + 2);
    for label in trimmed.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return None;
        }
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out.push(0);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(id: u16, name: &str, qtype: u16, edns: bool) -> Vec<u8> {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&id.to_be_bytes());
        pkt.extend_from_slice(&0x0100u16.to_be_bytes()); // standard query, RD=1
        pkt.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
        pkt.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT=0
        pkt.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
        pkt.extend_from_slice(&(u16::from(edns)).to_be_bytes()); // ARCOUNT
        for label in name.trim_end_matches('.').split('.').filter(|s| !s.is_empty()) {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
        pkt.push(0);
        pkt.extend_from_slice(&qtype.to_be_bytes());
        pkt.extend_from_slice(&CLASS_IN.to_be_bytes());
        if edns {
            // Minimal EDNS0 OPT record (root name 0x00, type 41, UDP payload 4096, ext RCODE 0, RDLEN 0)
            pkt.extend_from_slice(&[0x00, 0x00, 41, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
        }
        pkt
    }

    fn sample_table() -> HostTable {
        HostTable::from_entries(&[
            HostEntry { name: "redis".into(), id: "a1b2c3d4e5f6".into(), ip: "192.168.64.5".into() },
            HostEntry { name: "Web-Api".into(), id: "112233445566".into(), ip: "192.168.64.9".into() },
        ])
    }

    #[test]
    fn resolves_a_by_name_id_and_local_domain() {
        let table = sample_table();
        for name in ["redis", "REDIS", "redis.graviton", "a1b2c3d4e5f6", "a1b2c3d4e5f6.graviton"] {
            let req = query(0x1234, name, TYPE_A, true);
            let rep = answer(&req, &table).unwrap_or_else(|| panic!("expected answer for {name}"));
            assert_eq!(&rep[0..2], &[0x12, 0x34]);
            let flags = u16::from_be_bytes([rep[2], rep[3]]);
            assert_eq!(flags, 0x8580); // QR=1, AA=1, RD=1, RA=1, NOERROR
            assert_eq!(u16::from_be_bytes([rep[6], rep[7]]), 1); // ANCOUNT=1
            assert_eq!(u16::from_be_bytes([rep[10], rep[11]]), 0); // ARCOUNT=0
            // RDATA is the last 4 bytes.
            assert_eq!(&rep[rep.len() - 4..], &[192, 168, 64, 5]);
        }
        // Mixed-case QNAME is echoed verbatim (DNS 0x20 preservation).
        let req = query(0x4321, "Web-API", TYPE_A, false);
        let rep = answer(&req, &table).unwrap();
        assert_eq!(&rep[12..12 + 9], b"\x07Web-API\0");
        assert_eq!(&rep[rep.len() - 4..], &[192, 168, 64, 9]);
    }

    #[test]
    fn aaaa_for_known_container_is_nodata_not_forward() {
        let table = sample_table();
        let req = query(0x0007, "redis", 28, false); // AAAA
        let rep = answer(&req, &table).unwrap();
        let flags = u16::from_be_bytes([rep[2], rep[3]]);
        assert_eq!(flags & 0x000F, RCODE_NOERROR);
        assert_eq!(u16::from_be_bytes([rep[6], rep[7]]), 0); // ANCOUNT=0 (NODATA)
    }

    #[test]
    fn unknown_external_forwards_and_unknown_graviton_is_nxdomain() {
        let table = sample_table();
        assert!(answer(&query(1, "example.com", TYPE_A, false), &table).is_none());
        assert!(answer(&query(2, "unknown", TYPE_A, false), &table).is_none());

        let rep = answer(&query(3, "missing.graviton", TYPE_A, false), &table).unwrap();
        let flags = u16::from_be_bytes([rep[2], rep[3]]);
        assert_eq!(flags & 0x000F, RCODE_NXDOMAIN);
        assert_eq!(u16::from_be_bytes([rep[6], rep[7]]), 0);
    }

    #[test]
    fn reverse_ptr_lookup_for_container_ip() {
        let table = sample_table();
        let req = query(0x5555, "5.64.168.192.in-addr.arpa", TYPE_PTR, false);
        let rep = answer(&req, &table).unwrap();
        assert_eq!(u16::from_be_bytes([rep[6], rep[7]]), 1);
        // RDATA: \x05redis\x00
        assert_eq!(&rep[rep.len() - 7..], b"\x05redis\0");

        // Unknown IP forwards upstream.
        assert!(answer(&query(0x5556, "1.64.168.192.in-addr.arpa", TYPE_PTR, false), &table).is_none());
    }

    #[test]
    fn malformed_packets_never_panic() {
        let table = sample_table();
        assert!(answer(&[], &table).is_none());
        assert!(answer(&[0; 11], &table).is_none());
        // Truncated label length
        let bad = [0, 1, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 10, b'a'];
        assert!(answer(&bad, &table).is_none());
        // Compression pointer inside QNAME
        let ptr = [0, 1, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 0x0C, 0, 1, 0, 1];
        assert!(answer(&ptr, &table).is_none());
    }

    #[test]
    fn servfail_preserves_id_and_question() {
        let req = query(0xABCD, "example.com", TYPE_A, true);
        let rep = servfail(&req).unwrap();
        assert_eq!(&rep[0..2], &[0xAB, 0xCD]);
        let flags = u16::from_be_bytes([rep[2], rep[3]]);
        assert_eq!(flags & 0x000F, RCODE_SERVFAIL);
        assert_eq!(flags & 0x8000, 0x8000);
        assert_eq!(u16::from_be_bytes([rep[4], rep[5]]), 1);
        assert_eq!(u16::from_be_bytes([rep[6], rep[7]]), 0);
    }

    #[test]
    fn parses_resolv_conf() {
        let conf = "# Generated by udhcpc\nnameserver 127.0.0.11\nnameserver 192.168.64.1 # nat\nnameserver 192.168.64.1\nnameserver 1.1.1.1\nnameserver bogus\n";
        let skip = Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 11)));
        let servers = parse_resolv_conf(conf, skip);
        assert_eq!(
            servers,
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 64, 1)), 53),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
            ]
        );
    }
}
