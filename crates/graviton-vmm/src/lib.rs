//! Lightweight Linux VMs for Graviton.
//!
//! Each container runs in its own minimal VM: a small Linux kernel, an
//! initramfs whose `/init` is `graviton-agent`, and (from milestone 2) the
//! container's root filesystem attached as a virtio block device or shared
//! over virtiofs.
//!
//! [`VmConfig`] describes a VM and is platform-independent. A [`Hypervisor`]
//! boots it; on macOS that is [`apple::AppleHypervisor`], backed by
//! Virtualization.framework.

mod config;

#[cfg(target_os = "macos")]
pub mod apple;

use std::os::fd::OwnedFd;
use std::time::Duration;

pub use config::{ConfigError, NetworkMode, SharedDirectory, VmConfig};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VmmError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Framework(String),
    #[error("hypervisor unavailable: {0}")]
    Unavailable(String),
    #[error("timed out {0}")]
    Timeout(&'static str),
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    Error,
}

/// A running (or stopped) VM.
pub trait VmHandle: Send {
    fn state(&self) -> VmState;
    /// Open a vsock stream to `port` inside the guest.
    fn connect(&self, port: u32) -> Result<OwnedFd, VmmError>;
    /// Ask the guest to shut down cleanly.
    fn request_stop(&self) -> Result<(), VmmError>;
    /// Hard stop.
    fn kill(&self) -> Result<(), VmmError>;
    /// Block until the VM has stopped (or `timeout` passes).
    fn wait_stopped(&self, timeout: Duration) -> Result<(), VmmError>;
}

/// Something that can boot a [`VmConfig`].
pub trait Hypervisor: Send + Sync {
    fn name(&self) -> &'static str;
    fn boot(&self, config: &VmConfig) -> Result<Box<dyn VmHandle>, VmmError>;
}
