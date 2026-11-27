//! Guest networking: pure-Rust `eth0` DHCPv4 + link/route setup via Linux
//! ioctls, `AF_PACKET`, and `NETLINK_ROUTE` (no BusyBox or `udhcpc` needed).
//!
//! The NAT keeps guests apart, so container VMs also
//! get `eth1` wired to `gravitond`'s L2 switch. Once DHCP finishes on `eth0`,
//! the subnet is routed over `eth1` (as its two halves, with `RTA_PREFSRC` set
//! to `eth0`'s IP), while the gateway stays pinned on `eth0`.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::net::Ipv4Addr;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
static STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
static ADDRESS: OnceLock<Option<String>> = OnceLock::new();

#[cfg(target_os = "linux")]
pub fn bring_up_links() {
    sys::bring_up_links();
}

#[cfg(target_os = "linux")]
pub fn is_no_nat_boot() -> bool {
    sys::no_nat_boot()
}

#[cfg(target_os = "linux")]
pub fn drop_default_route() -> Result<(), String> {
    sys::drop_default_route()
}

#[cfg(target_os = "linux")]
pub fn start() {
    sys::start();
}

#[cfg(target_os = "linux")]
pub fn wait(timeout: Duration) -> Option<String> {
    sys::wait(timeout)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DhcpReply {
    msg_type: u8,
    yiaddr: Ipv4Addr,
    subnet_mask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
}

const DHCP_DISCOVER: u8 = 1;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DHCP_OFFER: u8 = 2;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

fn build_dhcp_packet(
    msg_type: u8,
    xid: u32,
    mac: &[u8; 6],
    requested_ip: Option<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
) -> Vec<u8> {
    let mut dhcp = vec![0u8; 240];
    dhcp[0] = 1; // BOOTREQUEST
    dhcp[1] = 1; // Ethernet
    dhcp[2] = 6; // MAC len
    dhcp[4..8].copy_from_slice(&xid.to_be_bytes());
    dhcp[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // BROADCAST flag
    dhcp[28..34].copy_from_slice(mac);
    dhcp[236..240].copy_from_slice(&DHCP_MAGIC);

    dhcp.extend_from_slice(&[53, 1, msg_type]);
    if let Some(ip) = requested_ip {
        dhcp.push(50);
        dhcp.push(4);
        dhcp.extend_from_slice(&ip.octets());
    }
    if let Some(sid) = server_id {
        dhcp.push(54);
        dhcp.push(4);
        dhcp.extend_from_slice(&sid.octets());
    }
    dhcp.extend_from_slice(&[55, 3, 1, 3, 6]); // subnet mask, router, dns
    dhcp.push(255);
    while dhcp.len() < 300 {
        dhcp.push(0);
    }

    let udp_len = (8 + dhcp.len()) as u16;
    let total_len = 20 + udp_len;
    let mut pkt = Vec::with_capacity(total_len as usize);

    // IPv4 header
    pkt.extend_from_slice(&[0x45, 0x00]);
    pkt.extend_from_slice(&total_len.to_be_bytes());
    pkt.extend_from_slice(&(xid as u16).to_be_bytes());
    pkt.extend_from_slice(&[0x00, 0x00, 64, 17, 0x00, 0x00]);
    pkt.extend_from_slice(&[0, 0, 0, 0]);
    pkt.extend_from_slice(&[255, 255, 255, 255]);
    let ip_sum = ipv4_checksum(&pkt[..20]);
    pkt[10..12].copy_from_slice(&ip_sum.to_be_bytes());

    // UDP header
    let udp_start = pkt.len();
    pkt.extend_from_slice(&68u16.to_be_bytes());
    pkt.extend_from_slice(&67u16.to_be_bytes());
    pkt.extend_from_slice(&udp_len.to_be_bytes());
    pkt.extend_from_slice(&[0x00, 0x00]);
    pkt.extend_from_slice(&dhcp);
    let u_sum = udp_checksum([0, 0, 0, 0], [255, 255, 255, 255], &pkt[udp_start..]);
    pkt[udp_start + 6..udp_start + 8].copy_from_slice(&u_sum.to_be_bytes());
    pkt
}

fn parse_dhcp_packet(pkt: &[u8], expected_xid: u32, expected_mac: &[u8; 6]) -> Option<DhcpReply> {
    if pkt.len() < 28 || (pkt[0] >> 4) != 4 {
        return None;
    }
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    if ihl < 20 || pkt[9] != 17 || pkt.len() < ihl + 8 {
        return None;
    }
    let dst_port = u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]]);
    if dst_port != 68 {
        return None;
    }
    let udp_len = u16::from_be_bytes([pkt[ihl + 4], pkt[ihl + 5]]) as usize;
    if udp_len < 8 {
        return None;
    }
    let end = (ihl + udp_len).min(pkt.len());
    if end < ihl + 8 {
        return None;
    }
    let dhcp = &pkt[ihl + 8..end];
    if dhcp.len() < 240 || dhcp[0] != 2 {
        return None;
    }
    if dhcp[4..8] != expected_xid.to_be_bytes() || &dhcp[28..34] != expected_mac || dhcp[236..240] != DHCP_MAGIC {
        return None;
    }
    let yiaddr = Ipv4Addr::new(dhcp[16], dhcp[17], dhcp[18], dhcp[19]);
    let mut msg_type = None;
    let mut subnet_mask = None;
    let mut router = None;
    let mut dns = Vec::new();
    let mut server_id = None;

    let opts = &dhcp[240..];
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => i += 1,
            255 => break,
            code => {
                if i + 1 >= opts.len() {
                    break;
                }
                let len = opts[i + 1] as usize;
                if i + 2 + len > opts.len() {
                    break;
                }
                let val = &opts[i + 2..i + 2 + len];
                match code {
                    53 if len >= 1 => msg_type = Some(val[0]),
                    1 if len == 4 => subnet_mask = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
                    3 if len >= 4 => router = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
                    6 => {
                        for chunk in val.chunks_exact(4) {
                            dns.push(Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]));
                        }
                    }
                    54 if len == 4 => server_id = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
                    _ => {}
                }
                i += 2 + len;
            }
        }
    }

    Some(DhcpReply { msg_type: msg_type?, yiaddr, subnet_mask, router, dns, server_id })
}

fn ipv4_checksum(hdr: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for chunk in hdr.chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if hdr.len() % 2 == 1 {
        sum += u32::from(hdr[hdr.len() - 1]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn udp_checksum(src: [u8; 4], dst: [u8; 4], udp_segment: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for chunk in [src, dst] {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        sum += u32::from(u16::from_be_bytes([chunk[2], chunk[3]]));
    }
    sum += 17u32;
    sum += udp_segment.len() as u32;
    for chunk in udp_segment.chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if udp_segment.len() % 2 == 1 {
        sum += u32::from(udp_segment[udp_segment.len() - 1]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let csum = !(sum as u16);
    if csum == 0 { 0xffff } else { csum }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mask_to_prefix(mask: Ipv4Addr) -> Option<u32> {
    let bits = u32::from(mask);
    let ones = bits.leading_ones();
    (bits == if ones == 0 { 0 } else { u32::MAX << (32 - ones) }).then_some(ones)
}

/// The two halves of `address`'s subnet: `192.168.64.60`, 24 → `("192.168.64.0", 25), ("192.168.64.128", 25)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn split(address: Ipv4Addr, prefix: u32) -> Option<[(Ipv4Addr, u8); 2]> {
    if !(1..=30).contains(&prefix) {
        return None;
    }
    let mask = u32::MAX << (32 - prefix);
    let net = u32::from(address) & mask;
    let half = 1u32 << (31 - prefix);
    let new_prefix = (prefix + 1) as u8;
    Some([(Ipv4Addr::from(net), new_prefix), (Ipv4Addr::from(net + half), new_prefix)])
}

#[cfg(target_os = "linux")]
mod sys {
    use std::fs;
    use std::io;
    use std::net::Ipv4Addr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{
        ADDRESS, DHCP_ACK, DHCP_DISCOVER, DHCP_OFFER, DHCP_REQUEST, STARTED, build_dhcp_packet, mask_to_prefix,
        parse_dhcp_packet, split,
    };

    pub fn bring_up_links() {
        let _ = set_link_up("lo");
        if Path::new("/sys/class/net/eth0").exists() {
            let _ = set_link_up("eth0");
        }
        if Path::new("/sys/class/net/eth1").exists() {
            let _ = set_link_up("eth1");
        }
    }

    pub(super) fn no_nat_boot() -> bool {
        fs::read_to_string("/proc/cmdline")
            .map(|s| s.split_whitespace().any(|tok| tok == "graviton.no_nat=1"))
            .unwrap_or(false)
    }

    pub fn start() {
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        if no_nat_boot() || !Path::new("/sys/class/net/eth0").exists() {
            let _ = set_link_up("lo");
            if Path::new("/sys/class/net/eth1").exists() {
                let _ = set_link_up("eth1");
            }
            if !Path::new("/etc/resolv.conf").exists() {
                let _ = fs::write("/etc/resolv.conf", "nameserver 127.0.0.11\n");
            }
            let _ = ADDRESS.set(None);
            return;
        }
        thread::spawn(|| {
            let _ = ADDRESS.set(configure());
        });
    }

    pub fn wait(timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(address) = ADDRESS.get() {
                return address.clone();
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn configure() -> Option<String> {
        if let Err(err) = set_link_up("lo") {
            eprintln!("graviton-agent: link lo up: {err}");
        }
        if let Err(err) = set_link_up("eth0") {
            eprintln!("graviton-agent: link eth0 up: {err}");
            return None;
        }
        let sock = open_inet_dgram().ok()?;
        let ifindex = iface_index(sock.as_raw_fd(), "eth0").ok()?;
        let mac = iface_mac(sock.as_raw_fd(), "eth0").ok()?;
        let lease = dhcp_acquire(ifindex, &mac, Duration::from_secs(8))?;

        let ip = lease.yiaddr;
        let mask = lease.subnet_mask.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
        let prefix = mask_to_prefix(mask).unwrap_or(24);
        if let Err(err) = set_ipv4_addr(sock.as_raw_fd(), "eth0", ip, mask) {
            eprintln!("graviton-agent: eth0 addr {ip}/{prefix}: {err}");
            return None;
        }

        if let Some(gw) = lease.router {
            if let Err(err) = add_route_netlink(Ipv4Addr::UNSPECIFIED, 0, Some(gw), Some(ip), ifindex) {
                eprintln!("graviton-agent: default route via {gw}: {err}");
            }
        }

        let mut resolv = String::new();
        for server in &lease.dns {
            resolv.push_str(&format!("nameserver {server}\n"));
        }
        if resolv.is_empty() {
            if let Some(gw) = lease.router {
                resolv.push_str(&format!("nameserver {gw}\n"));
            }
        }
        let _ = fs::write("/etc/resolv.conf", resolv);
        let ip_str = ip.to_string();
        let _ = fs::write("/run/graviton-ip", format!("{ip_str}\n"));

        if Path::new("/sys/class/net/eth1").exists() {
            if let Err(err) = container_network(sock.as_raw_fd(), ip, prefix, lease.router, ifindex) {
                eprintln!("graviton-agent: container network: {err}");
            }
        }
        Some(ip_str)
    }

    fn container_network(
        sock: std::os::fd::RawFd,
        address: Ipv4Addr,
        prefix: u32,
        gateway: Option<Ipv4Addr>,
        eth0_idx: i32,
    ) -> Result<(), String> {
        let gw = gateway.ok_or("no default route")?;
        let halves = split(address, prefix).ok_or("subnet too small to split")?;
        set_link_up("eth1")?;
        let eth1_idx = iface_index(sock, "eth1")?;
        // Pin gateway host route (DNS, internet, Mac) on eth0.
        add_route_netlink(gw, 32, None, Some(address), eth0_idx)?;
        for (net, pfx) in halves {
            add_route_netlink(net, pfx, None, Some(address), eth1_idx)?;
        }
        Ok(())
    }

    fn dhcp_acquire(ifindex: i32, mac: &[u8; 6], total_timeout: Duration) -> Option<super::DhcpReply> {
        const ETH_P_IP: u16 = 0x0800;
        // SAFETY: AF_PACKET raw IPv4 DGRAM socket bound to `ifindex`.
        let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, i32::from(ETH_P_IP.to_be())) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is an owned socket descriptor.
        let pkt_sock = unsafe { OwnedFd::from_raw_fd(fd) };
        let rcv_timeout = libc::timeval { tv_sec: 0, tv_usec: 500_000 };
        // SAFETY: `rcv_timeout` is a valid timeval pointer.
        unsafe {
            libc::setsockopt(
                pkt_sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const rcv_timeout).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }

        // SAFETY: `sockaddr_ll` is POD.
        let mut bind_addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        bind_addr.sll_family = libc::AF_PACKET as u16;
        bind_addr.sll_protocol = ETH_P_IP.to_be();
        bind_addr.sll_ifindex = ifindex;
        // SAFETY: `bind_addr` is valid for `sockaddr_ll` size.
        if unsafe {
            libc::bind(
                pkt_sock.as_raw_fd(),
                (&raw const bind_addr).cast(),
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        } != 0
        {
            return None;
        }

        let mut bcast_addr = bind_addr;
        bcast_addr.sll_halen = 6;
        bcast_addr.sll_addr[..6].copy_from_slice(&[0xff; 6]);

        let mut xid = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0x4752_5631, |d| d.subsec_nanos())
            ^ (std::process::id() << 16);
        let deadline = Instant::now() + total_timeout;
        let mut rx = [0u8; 2048];

        while Instant::now() < deadline {
            xid = xid.wrapping_add(1);
            let discover = build_dhcp_packet(DHCP_DISCOVER, xid, mac, None, None);
            // SAFETY: valid buffer & destination `sockaddr_ll`.
            let sent = unsafe {
                libc::sendto(
                    pkt_sock.as_raw_fd(),
                    discover.as_ptr().cast(),
                    discover.len(),
                    0,
                    (&raw const bcast_addr).cast(),
                    std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if sent < 0 {
                return None;
            }

            let offer_deadline = (Instant::now() + Duration::from_millis(1200)).min(deadline);
            let mut offered = None;
            while Instant::now() < offer_deadline {
                // SAFETY: valid rx buffer on bound AF_PACKET socket.
                let n = unsafe { libc::recv(pkt_sock.as_raw_fd(), rx.as_mut_ptr().cast(), rx.len(), 0) };
                if n <= 0 {
                    continue;
                }
                if let Some(reply) = parse_dhcp_packet(&rx[..n as usize], xid, mac) {
                    if reply.msg_type == DHCP_OFFER && reply.yiaddr != Ipv4Addr::UNSPECIFIED {
                        offered = Some(reply);
                        break;
                    }
                }
            }
            let Some(offer) = offered else {
                continue;
            };

            let req = build_dhcp_packet(DHCP_REQUEST, xid, mac, Some(offer.yiaddr), offer.server_id);
            // SAFETY: valid buffer & destination `sockaddr_ll`.
            let _ = unsafe {
                libc::sendto(
                    pkt_sock.as_raw_fd(),
                    req.as_ptr().cast(),
                    req.len(),
                    0,
                    (&raw const bcast_addr).cast(),
                    std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };

            let ack_deadline = (Instant::now() + Duration::from_millis(1500)).min(deadline);
            while Instant::now() < ack_deadline {
                // SAFETY: valid rx buffer on bound AF_PACKET socket.
                let n = unsafe { libc::recv(pkt_sock.as_raw_fd(), rx.as_mut_ptr().cast(), rx.len(), 0) };
                if n <= 0 {
                    continue;
                }
                if let Some(ack) = parse_dhcp_packet(&rx[..n as usize], xid, mac) {
                    if ack.msg_type == DHCP_ACK && ack.yiaddr != Ipv4Addr::UNSPECIFIED {
                        return Some(ack);
                    }
                }
            }
        }
        None
    }

    fn open_inet_dgram() -> Result<OwnedFd, String> {
        // SAFETY: plain IPv4 datagram socket used for interface ioctls.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: fresh descriptor we own.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn make_ifreq(iface: &str) -> Result<libc::ifreq, String> {
        let bytes = iface.as_bytes();
        if bytes.is_empty() || bytes.len() >= libc::IFNAMSIZ {
            return Err(format!("bad interface `{iface}`"));
        }
        // SAFETY: `ifreq` is plain old data.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        for (dst, &b) in ifr.ifr_name.iter_mut().zip(bytes.iter()) {
            *dst = b as _;
        }
        Ok(ifr)
    }

    fn set_link_up(iface: &str) -> Result<(), String> {
        let sock = open_inet_dgram()?;
        let mut ifr = make_ifreq(iface)?;
        // SAFETY: `SIOCGIFFLAGS` fills `ifr_ifru.ifru_flags`.
        if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS as _, &raw mut ifr) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: reading and setting `ifru_flags` after `SIOCGIFFLAGS`.
        unsafe {
            ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            if libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as _, &raw const ifr) != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
        }
        Ok(())
    }

    fn iface_index(sock: std::os::fd::RawFd, iface: &str) -> Result<i32, String> {
        let mut ifr = make_ifreq(iface)?;
        // SAFETY: `SIOCGIFINDEX` populates `ifru_ifindex`.
        if unsafe { libc::ioctl(sock, libc::SIOCGIFINDEX as _, &raw mut ifr) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: reading `ifru_ifindex` populated by `SIOCGIFINDEX`.
        Ok(unsafe { ifr.ifr_ifru.ifru_ifindex })
    }

    fn iface_mac(sock: std::os::fd::RawFd, iface: &str) -> Result<[u8; 6], String> {
        let mut ifr = make_ifreq(iface)?;
        // SAFETY: `SIOCGIFHWADDR` populates `ifru_hwaddr`.
        if unsafe { libc::ioctl(sock, libc::SIOCGIFHWADDR as _, &raw mut ifr) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut mac = [0u8; 6];
        // SAFETY: `sa_data[0..6]` holds the Ethernet MAC address after `SIOCGIFHWADDR`.
        unsafe {
            for (dst, &src) in mac.iter_mut().zip(ifr.ifr_ifru.ifru_hwaddr.sa_data[..6].iter()) {
                *dst = src;
            }
        }
        Ok(mac)
    }

    fn sockaddr_in_from_ipv4(ip: Ipv4Addr) -> libc::sockaddr {
        // SAFETY: `sockaddr_in` fits within `sockaddr` (both 16 bytes) cleanly via `zeroed` + field writes.
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(ip.octets()) };
        // SAFETY: `sockaddr_in` and `sockaddr` have identical 16-byte size and alignment on Linux.
        unsafe { std::mem::transmute(sin) }
    }

    fn set_ipv4_addr(sock: std::os::fd::RawFd, iface: &str, ip: Ipv4Addr, mask: Ipv4Addr) -> Result<(), String> {
        let mut ifr = make_ifreq(iface)?;
        ifr.ifr_ifru.ifru_addr = sockaddr_in_from_ipv4(ip);
        // SAFETY: `SIOCSIFADDR` reads `ifru_addr`.
        if unsafe { libc::ioctl(sock, libc::SIOCSIFADDR as _, &raw const ifr) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut mask_ifr = make_ifreq(iface)?;
        mask_ifr.ifr_ifru.ifru_netmask = sockaddr_in_from_ipv4(mask);
        // SAFETY: `SIOCSIFNETMASK` reads `ifru_netmask`.
        if unsafe { libc::ioctl(sock, libc::SIOCSIFNETMASK as _, &raw const mask_ifr) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    pub fn drop_default_route() -> Result<(), String> {
        // SAFETY: netlink route socket.
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: fresh fd owned here.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };

        #[repr(C)]
        struct RtMsg {
            rtm_family: u8,
            rtm_dst_len: u8,
            rtm_src_len: u8,
            rtm_tos: u8,
            rtm_table: u8,
            rtm_protocol: u8,
            rtm_scope: u8,
            rtm_type: u8,
            rtm_flags: u32,
        }

        let mut msg = vec![0u8; std::mem::size_of::<libc::nlmsghdr>() + std::mem::size_of::<RtMsg>()];
        let hdr = libc::nlmsghdr {
            nlmsg_len: msg.len() as u32,
            nlmsg_type: libc::RTM_DELROUTE,
            nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
            nlmsg_seq: 2,
            nlmsg_pid: 0,
        };
        let rt = RtMsg {
            rtm_family: libc::AF_INET as u8,
            rtm_dst_len: 0,
            rtm_src_len: 0,
            rtm_tos: 0,
            rtm_table: libc::RT_TABLE_MAIN,
            rtm_protocol: libc::RTPROT_UNSPEC,
            rtm_scope: libc::RT_SCOPE_UNIVERSE,
            rtm_type: libc::RTN_UNICAST,
            rtm_flags: 0,
        };
        // SAFETY: copying POD headers into `msg`.
        unsafe {
            std::ptr::copy_nonoverlapping((&raw const hdr).cast::<u8>(), msg.as_mut_ptr(), std::mem::size_of::<libc::nlmsghdr>());
            std::ptr::copy_nonoverlapping(
                (&raw const rt).cast::<u8>(),
                msg.as_mut_ptr().add(std::mem::size_of::<libc::nlmsghdr>()),
                std::mem::size_of::<RtMsg>(),
            );
        }
        let sent = unsafe { libc::send(sock.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut resp = [0u8; 256];
        let n = unsafe { libc::recv(sock.as_raw_fd(), resp.as_mut_ptr().cast(), resp.len(), 0) };
        if n >= (std::mem::size_of::<libc::nlmsghdr>() + 4) as isize {
            let hdr_sz = std::mem::size_of::<libc::nlmsghdr>();
            let nlmsg_type = u16::from_ne_bytes([resp[4], resp[5]]);
            if nlmsg_type == libc::NLMSG_ERROR as u16 {
                let err = i32::from_ne_bytes([resp[hdr_sz], resp[hdr_sz + 1], resp[hdr_sz + 2], resp[hdr_sz + 3]]);
                if err != 0 && err != -libc::ESRCH {
                    return Err(io::Error::from_raw_os_error(-err).to_string());
                }
            }
        }
        Ok(())
    }

    fn add_route_netlink(
        dst: Ipv4Addr,
        prefix: u8,
        gateway: Option<Ipv4Addr>,
        prefsrc: Option<Ipv4Addr>,
        oif: i32,
    ) -> Result<(), String> {
        // SAFETY: netlink route socket.
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: fresh fd owned here.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };

        #[repr(C)]
        struct RtMsg {
            rtm_family: u8,
            rtm_dst_len: u8,
            rtm_src_len: u8,
            rtm_tos: u8,
            rtm_table: u8,
            rtm_protocol: u8,
            rtm_scope: u8,
            rtm_type: u8,
            rtm_flags: u32,
        }

        let mut msg = Vec::with_capacity(128);
        msg.resize(std::mem::size_of::<libc::nlmsghdr>() + std::mem::size_of::<RtMsg>(), 0);

        let push_attr = |buf: &mut Vec<u8>, rta_type: u16, data: &[u8]| {
            let rta_len = (4 + data.len()) as u16;
            buf.extend_from_slice(&rta_len.to_ne_bytes());
            buf.extend_from_slice(&rta_type.to_ne_bytes());
            buf.extend_from_slice(data);
            while buf.len() % 4 != 0 {
                buf.push(0);
            }
        };

        if prefix > 0 {
            push_attr(&mut msg, libc::RTA_DST, &dst.octets());
        }
        if let Some(gw) = gateway {
            push_attr(&mut msg, libc::RTA_GATEWAY, &gw.octets());
        }
        if let Some(src) = prefsrc {
            push_attr(&mut msg, libc::RTA_PREFSRC, &src.octets());
        }
        push_attr(&mut msg, libc::RTA_OIF, &(oif as u32).to_ne_bytes());

        let hdr = libc::nlmsghdr {
            nlmsg_len: msg.len() as u32,
            nlmsg_type: libc::RTM_NEWROUTE,
            nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_ACK | libc::NLM_F_CREATE | libc::NLM_F_REPLACE) as u16,
            nlmsg_seq: 1,
            nlmsg_pid: 0,
        };
        let rt = RtMsg {
            rtm_family: libc::AF_INET as u8,
            rtm_dst_len: prefix,
            rtm_src_len: 0,
            rtm_tos: 0,
            rtm_table: libc::RT_TABLE_MAIN,
            rtm_protocol: libc::RTPROT_BOOT,
            rtm_scope: if gateway.is_some() { libc::RT_SCOPE_UNIVERSE } else { libc::RT_SCOPE_LINK },
            rtm_type: libc::RTN_UNICAST,
            rtm_flags: 0,
        };
        // SAFETY: copying POD headers into the front of `msg`.
        unsafe {
            std::ptr::copy_nonoverlapping((&raw const hdr).cast::<u8>(), msg.as_mut_ptr(), std::mem::size_of::<libc::nlmsghdr>());
            std::ptr::copy_nonoverlapping(
                (&raw const rt).cast::<u8>(),
                msg.as_mut_ptr().add(std::mem::size_of::<libc::nlmsghdr>()),
                std::mem::size_of::<RtMsg>(),
            );
        }

        // SAFETY: `msg` is a valid netlink message buffer.
        let sent = unsafe { libc::send(sock.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut resp = [0u8; 256];
        // SAFETY: reading ACK on connected/unbound netlink socket.
        let n = unsafe { libc::recv(sock.as_raw_fd(), resp.as_mut_ptr().cast(), resp.len(), 0) };
        if n < (std::mem::size_of::<libc::nlmsghdr>() + 4) as isize {
            return Err("short netlink response".into());
        }
        let hdr_sz = std::mem::size_of::<libc::nlmsghdr>();
        let nlmsg_type = u16::from_ne_bytes([resp[4], resp[5]]);
        if nlmsg_type == libc::NLMSG_ERROR as u16 {
            let err = i32::from_ne_bytes([resp[hdr_sz], resp[hdr_sz + 1], resp[hdr_sz + 2], resp[hdr_sz + 3]]);
            if err != 0 && err != -libc::EEXIST {
                return Err(io::Error::from_raw_os_error(-err).to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_subnets() {
        assert_eq!(
            split(Ipv4Addr::new(192, 168, 64, 60), 24).unwrap(),
            [(Ipv4Addr::new(192, 168, 64, 0), 25), (Ipv4Addr::new(192, 168, 64, 128), 25)]
        );
        assert_eq!(
            split(Ipv4Addr::new(10, 1, 2, 3), 16).unwrap(),
            [(Ipv4Addr::new(10, 1, 0, 0), 17), (Ipv4Addr::new(10, 1, 128, 0), 17)]
        );
        assert!(split(Ipv4Addr::new(10, 0, 0, 1), 31).is_none());
    }

    #[test]
    fn mask_prefix_conversion() {
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 255, 0)), Some(24));
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 0, 0)), Some(16));
        assert_eq!(mask_to_prefix(Ipv4Addr::new(0, 0, 0, 0)), Some(0));
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 0, 255, 0)), None);
    }

    #[test]
    fn dhcp_packet_build_and_reply_parse_round_trip() {
        let mac = [0x02, 0x47, 0x72, 0x11, 0x22, 0x33];
        let xid = 0x1234_5678;
        let discover = build_dhcp_packet(DHCP_DISCOVER, xid, &mac, None, None);
        assert_eq!(discover[0] >> 4, 4);
        assert_eq!(discover[9], 17);
        assert_eq!(ipv4_checksum(&discover[..20]), 0);
        // Flip BOOTP op to BOOTREPLY and synthesize a server DHCPACK to verify parser.
        let mut reply = build_dhcp_packet(DHCP_ACK, xid, &mac, None, Some(Ipv4Addr::new(192, 168, 64, 1)));
        // Change UDP ports (src=67, dst=68) and fill yiaddr + subnet mask + router + dns.
        reply[20..22].copy_from_slice(&67u16.to_be_bytes());
        reply[22..24].copy_from_slice(&68u16.to_be_bytes());
        reply[28] = 2; // BOOTREPLY
        reply[28 + 16..28 + 20].copy_from_slice(&[192, 168, 64, 42]); // yiaddr
        // Patch options starting at 28 + 240.
        let opts = [
            53, 1, DHCP_ACK,
            1, 4, 255, 255, 255, 0,
            3, 4, 192, 168, 64, 1,
            6, 8, 192, 168, 64, 1, 1, 1, 1, 1,
            54, 4, 192, 168, 64, 1,
            255,
        ];
        reply[28 + 240..28 + 240 + opts.len()].copy_from_slice(&opts);

        let parsed = parse_dhcp_packet(&reply, xid, &mac).unwrap();
        assert_eq!(parsed.msg_type, DHCP_ACK);
        assert_eq!(parsed.yiaddr, Ipv4Addr::new(192, 168, 64, 42));
        assert_eq!(parsed.subnet_mask, Some(Ipv4Addr::new(255, 255, 255, 0)));
        assert_eq!(parsed.router, Some(Ipv4Addr::new(192, 168, 64, 1)));
        assert_eq!(parsed.dns, vec![Ipv4Addr::new(192, 168, 64, 1), Ipv4Addr::new(1, 1, 1, 1)]);
        assert_eq!(parsed.server_id, Some(Ipv4Addr::new(192, 168, 64, 1)));
    }
}
