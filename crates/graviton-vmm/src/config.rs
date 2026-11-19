use std::path::PathBuf;

use thiserror::Error;

/// Smallest memory a Graviton guest is allowed (kernel + agent + a process).
pub const MIN_MEMORY_MIB: u64 = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    /// No network device.
    None,
    /// NAT through the host (default).
    Nat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDirectory {
    pub host_path: PathBuf,
    /// virtiofs tag the agent mounts.
    pub tag: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    pub cpus: u32,
    pub memory_mib: u64,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// A disk image attached read-write as the guest's `/dev/vda`, where the
    /// agent keeps containers' files; empty for none.
    pub rootfs: PathBuf,
    pub cmdline: String,
    pub network: NetworkMode,
    pub shares: Vec<SharedDirectory>,
    /// Where to write the guest console (hvc0). Defaults to a temp file.
    pub console_log: Option<PathBuf>,
    /// A connected datagram socket for a second network device (`eth1`),
    /// carrying raw Ethernet frames: containers reach each other through it,
    /// since Virtualization.framework's NAT keeps guests apart. The caller
    /// keeps it open while the VM runs.
    pub switch_port: Option<std::os::fd::RawFd>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("cpus must be at least 1 (got {0})")]
    Cpus(u32),
    #[error("memory must be at least {MIN_MEMORY_MIB} MiB (got {0})")]
    Memory(u64),
    #[error("duplicate virtiofs tag `{0}`")]
    DuplicateTag(String),
    #[error("virtiofs tag `{0}` must be 1-36 characters of [A-Za-z0-9_-]")]
    BadTag(String),
}

impl VmConfig {
    /// Defaults for a single-container VM.
    pub fn new(kernel: impl Into<PathBuf>, initrd: impl Into<PathBuf>, rootfs: impl Into<PathBuf>) -> Self {
        Self {
            cpus: 2,
            memory_mib: 1024,
            kernel: kernel.into(),
            initrd: initrd.into(),
            rootfs: rootfs.into(),
            cmdline: "console=hvc0 quiet panic=-1".into(),
            network: NetworkMode::Nat,
            shares: Vec::new(),
            console_log: None,
            switch_port: None,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.cpus == 0 {
            return Err(ConfigError::Cpus(self.cpus));
        }
        if self.memory_mib < MIN_MEMORY_MIB {
            return Err(ConfigError::Memory(self.memory_mib));
        }
        let mut seen = std::collections::HashSet::new();
        for share in &self.shares {
            let tag = &share.tag;
            if tag.is_empty() || tag.len() > 36 || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(ConfigError::BadTag(tag.clone()));
            }
            if !seen.insert(tag) {
                return Err(ConfigError::DuplicateTag(tag.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VmConfig {
        VmConfig::new("vmlinux", "initrd.img", "rootfs.ext4")
    }

    #[test]
    fn defaults_are_valid() {
        assert_eq!(cfg().validate(), Ok(()));
    }

    #[test]
    fn rejects_bad_resources_and_tags() {
        let mut c = cfg();
        c.cpus = 0;
        assert_eq!(c.validate(), Err(ConfigError::Cpus(0)));

        let mut c = cfg();
        c.memory_mib = 64;
        assert_eq!(c.validate(), Err(ConfigError::Memory(64)));

        let mut c = cfg();
        let share = SharedDirectory { host_path: "/tmp".into(), tag: "app".into(), read_only: false };
        c.shares = vec![share.clone(), share];
        assert_eq!(c.validate(), Err(ConfigError::DuplicateTag("app".into())));

        let mut c = cfg();
        c.shares = vec![SharedDirectory { host_path: "/tmp".into(), tag: "bad tag".into(), read_only: true }];
        assert!(matches!(c.validate(), Err(ConfigError::BadTag(_))));
    }
}
