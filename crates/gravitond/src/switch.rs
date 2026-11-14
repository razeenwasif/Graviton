//! The container network: a learning Ethernet switch between containers.
//!
//! Virtualization.framework's NAT gives every VM an address on one subnet but
//! keeps guests from talking to each other (ARP between them never
//! answers). So each container VM also gets a second network device (`eth1`)
//! whose frames come here over a datagram socket pair. The switch learns
//! which port each MAC address is on, forwards unicast frames to it, and
//! floods broadcasts (ARP) and unknown destinations to every other port. In
//! the guest, the agent routes the NAT subnet over `eth1` (all but the
//! gateway), so containers reach each other at the addresses `grav ps` shows,
//! TCP and UDP alike, while the internet and the Mac stay on `eth0`.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a learned MAC address is trusted without hearing from it.
const MAC_TTL: Duration = Duration::from_secs(300);
/// Biggest Ethernet frame (1500-byte MTU plus header and VLAN tag), with room to spare.
const FRAME_MAX: usize = 1600;

#[derive(Default)]
struct Inner {
    /// Port id → our end of its socket pair.
    ports: HashMap<u64, Arc<OwnedFd>>,
    /// MAC → (port id, last seen).
    macs: HashMap<[u8; 6], (u64, Instant)>,
    next_id: u64,
}

#[derive(Default, Clone)]
pub struct Switch {
    inner: Arc<Mutex<Inner>>,
}

/// A port on the switch: give `vm_fd()` to the VM; dropping it unplugs the port.
pub struct Port {
    switch: Switch,
    id: u64,
    ours: Arc<OwnedFd>,
    vm: OwnedFd,
}

impl Port {
    /// The VM's end of the socket pair (keep the `Port` alive while the VM runs).
    pub fn vm_fd(&self) -> RawFd {
        self.vm.as_raw_fd()
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        let mut inner = self.switch.inner.lock().unwrap();
        inner.ports.remove(&self.id);
        inner.macs.retain(|_, (port, _)| *port != self.id);
        drop(inner);
        // Wakes the port's reader thread, which then exits.
        // SAFETY: shutdown(2) on a socket we own.
        unsafe { libc::shutdown(self.ours.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

impl Switch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Plug in a new port.
    pub fn attach(&self) -> io::Result<Port> {
        let mut fds = [0; 2];
        // SAFETY: socketpair(2) fills two descriptors.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socketpair returned two fresh descriptors that we now own.
        let (ours, vm) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&ours, &vm] {
            // Apple's guidance for VZFileHandleNetworkDeviceAttachment: big send buffers, bigger receive ones.
            set_buffer(fd, libc::SO_SNDBUF, 1 << 20);
            set_buffer(fd, libc::SO_RCVBUF, 4 << 20);
            // SAFETY: fcntl on a descriptor we own.
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let ours = Arc::new(ours);
        let id = {
            let mut inner = self.inner.lock().unwrap();
            inner.next_id += 1;
            let id = inner.next_id;
            inner.ports.insert(id, Arc::clone(&ours));
            id
        };
        let switch = self.clone();
        let reader = Arc::clone(&ours);
        std::thread::Builder::new().name(format!("switch-port-{id}")).spawn(move || switch.serve(id, &reader))?;
        Ok(Port { switch: self.clone(), id, ours, vm })
    }

    /// Read frames from one port and pass them on, until the port is unplugged.
    fn serve(&self, id: u64, fd: &OwnedFd) {
        let mut buf = [0u8; FRAME_MAX];
        loop {
            // SAFETY: recv(2) into a buffer we own.
            let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n <= 0 {
                if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            let frame = &buf[..n as usize];
            if frame.len() < 14 {
                continue;
            }
            for target in self.route(id, frame) {
                // A full receiver drops the frame, as a real switch would.
                // SAFETY: send(2) of a buffer we own on a descriptor the switch holds.
                unsafe { libc::send(target.as_raw_fd(), frame.as_ptr().cast(), frame.len(), libc::MSG_DONTWAIT) };
            }
        }
    }

    /// Learn the sender's MAC, then pick the ports a frame goes to.
    fn route(&self, from: u64, frame: &[u8]) -> Vec<Arc<OwnedFd>> {
        let dst: [u8; 6] = frame[0..6].try_into().unwrap();
        let src: [u8; 6] = frame[6..12].try_into().unwrap();
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        if src[0] & 1 == 0 {
            inner.macs.insert(src, (from, now));
        }
        let known = (dst[0] & 1 == 0)
            .then(|| inner.macs.get(&dst).filter(|(_, seen)| now.duration_since(*seen) < MAC_TTL).map(|(port, _)| *port))
            .flatten();
        match known {
            Some(port) if port == from => vec![],
            Some(port) => inner.ports.get(&port).cloned().into_iter().collect(),
            None => inner.ports.iter().filter(|(id, _)| **id != from).map(|(_, fd)| Arc::clone(fd)).collect(),
        }
    }
}

fn set_buffer(fd: &OwnedFd, option: libc::c_int, size: libc::c_int) {
    // SAFETY: setsockopt(2) with an int we own.
    unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, option, (&raw const size).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
        [&dst[..], &src[..], &[0x08, 0x00], payload].concat()
    }

    fn recv(fd: RawFd) -> Option<Vec<u8>> {
        let mut buf = [0u8; FRAME_MAX];
        for _ in 0..100 {
            // SAFETY: recv(2) into our buffer, non-blocking.
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
            if n > 0 {
                return Some(buf[..n as usize].to_vec());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn send(fd: RawFd, f: &[u8]) {
        // SAFETY: send(2) of our buffer.
        assert_eq!(unsafe { libc::send(fd, f.as_ptr().cast(), f.len(), 0) }, f.len() as isize);
    }

    #[test]
    fn floods_then_learns() {
        let sw = Switch::new();
        let (a, b, c) = (sw.attach().unwrap(), sw.attach().unwrap(), sw.attach().unwrap());
        let (ma, mb) = ([2, 0, 0, 0, 0, 0xa], [2, 0, 0, 0, 0, 0xb]);
        let bcast = [0xff; 6];

        // A broadcast (an ARP request) reaches everyone else.
        send(a.vm_fd(), &frame(bcast, ma, b"who has b"));
        assert_eq!(recv(b.vm_fd()).unwrap(), frame(bcast, ma, b"who has b"));
        assert!(recv(c.vm_fd()).is_some());
        assert!(recv(a.vm_fd()).is_none());

        // B answers A directly: only A gets it, and the switch learns B.
        send(b.vm_fd(), &frame(ma, mb, b"b is here"));
        assert_eq!(recv(a.vm_fd()).unwrap(), frame(ma, mb, b"b is here"));
        assert!(recv(c.vm_fd()).is_none());

        // A to B is now unicast.
        send(a.vm_fd(), &frame(mb, ma, b"hello b"));
        assert_eq!(recv(b.vm_fd()).unwrap(), frame(mb, ma, b"hello b"));
        assert!(recv(c.vm_fd()).is_none());

        // Unplugging B forgets it: frames for it flood again.
        drop(b);
        send(a.vm_fd(), &frame(mb, ma, b"still there?"));
        assert!(recv(c.vm_fd()).is_some());
    }
}
