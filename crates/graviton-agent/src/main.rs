//! `graviton-agent`: PID 1 inside every Graviton guest.
//!
//! Responsibilities (milestone 1-2):
//! 1. Mount `/proc`, `/sys`, `/dev`, and the container rootfs.
//! 2. Listen on vsock port [`graviton_proto::AGENT_VSOCK_PORT`].
//! 3. On `Exec`, create namespaces + cgroup, pivot_root into the rootfs and
//!    spawn the container process; stream stdout/stderr and the exit code back
//!    as [`graviton_proto::AgentEvent`]s.
//!
//! Build it for the guest with:
//! `cargo build -p graviton-agent --release --target aarch64-unknown-linux-musl`

#[cfg(target_os = "linux")]
fn main() {
    use graviton_proto::{AgentEvent, encode_line};
    use std::io::Write;

    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    // Until the vsock listener lands, announce readiness on the console.
    let _ = std::io::stdout().write_all(&encode_line(&AgentEvent::Ready { kernel }));
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("graviton-agent runs inside the Linux guest VM; build it with --target aarch64-unknown-linux-musl");
    std::process::exit(2);
}
