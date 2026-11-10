//! Virtualization.framework backend (milestone 1).
//!
//! Planned mapping, using the `objc2-virtualization` bindings:
//!
//! | VmConfig            | Virtualization.framework                                   |
//! |---------------------|------------------------------------------------------------|
//! | kernel / initrd     | `VZLinuxBootLoader` (`initialRamdiskURL`, `commandLine`)   |
//! | cpus / memory       | `VZVirtualMachineConfiguration.cpuCount` / `memorySize`    |
//! | rootfs              | `VZVirtioBlockDeviceConfiguration` + disk image attachment  |
//! | shared dirs         | `VZVirtioFileSystemDeviceConfiguration` (virtiofs)         |
//! | network (NAT)       | `VZVirtioNetworkDeviceConfiguration` + `VZNATNetworkDeviceAttachment` |
//! | agent channel       | `VZVirtioSocketDeviceConfiguration` (vsock)                |
//! | console             | `VZVirtioConsoleDeviceSerialPortConfiguration`             |
//!
//! The process that calls into Virtualization.framework (gravitond) must be
//! signed with the `com.apple.security.virtualization` entitlement, and the
//! `VZVirtualMachine` must be driven from the dispatch queue it was created on.

use crate::{Hypervisor, VmConfig, VmHandle, VmmError};

#[derive(Debug, Default)]
pub struct AppleHypervisor;

impl AppleHypervisor {
    pub fn new() -> Self {
        Self
    }
}

impl Hypervisor for AppleHypervisor {
    fn name(&self) -> &'static str {
        "Virtualization.framework"
    }

    fn boot(&self, config: &VmConfig) -> Result<Box<dyn VmHandle>, VmmError> {
        config.validate()?;
        Err(VmmError::NotImplemented("Virtualization.framework boot (milestone 1)"))
    }
}
