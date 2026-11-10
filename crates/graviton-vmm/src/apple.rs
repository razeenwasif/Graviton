//! Virtualization.framework backend.
//!
//! | VmConfig        | Virtualization.framework                                          |
//! |-----------------|-------------------------------------------------------------------|
//! | kernel / initrd | `VZLinuxBootLoader` (`initialRamdiskURL`, `commandLine`)          |
//! | cpus / memory   | `VZVirtualMachineConfiguration.CPUCount` / `memorySize`           |
//! | console         | `VZVirtioConsoleDeviceSerialPortConfiguration` → a log file       |
//! | agent channel   | `VZVirtioSocketDeviceConfiguration` (vsock)                       |
//! | network (NAT)   | `VZVirtioNetworkDeviceConfiguration` + `VZNATNetworkDeviceAttachment` |
//! | shared dirs     | `VZVirtioFileSystemDeviceConfiguration` (virtiofs)                |
//! | rootfs          | `VZVirtioBlockDeviceConfiguration` (milestone 2)                  |
//!
//! The calling process must be signed with the
//! `com.apple.security.virtualization` entitlement (see `scripts/dev-gravitond.sh`).
//!
//! Threading: a `VZVirtualMachine` may only be touched on the dispatch queue
//! it was created with, and its completion handlers run there too. Every call
//! below hops onto that queue with `exec_sync` and waits for completion
//! handlers on a channel *outside* the queue.

use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZBootLoader, VZDirectorySharingDeviceConfiguration, VZEntropyDeviceConfiguration, VZFileSerialPortAttachment,
    VZGenericPlatformConfiguration, VZLinuxBootLoader, VZNATNetworkDeviceAttachment, VZNetworkDeviceAttachment,
    VZNetworkDeviceConfiguration, VZPlatformConfiguration, VZSerialPortAttachment, VZSerialPortConfiguration,
    VZSharedDirectory, VZSingleDirectoryShare, VZSocketDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioEntropyDeviceConfiguration, VZVirtioFileSystemDeviceConfiguration, VZVirtioNetworkDeviceConfiguration,
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration, VZVirtualMachine,
    VZVirtualMachineConfiguration, VZVirtualMachineState,
};

use crate::{Hypervisor, NetworkMode, VmConfig, VmHandle, VmState, VmmError};

const START_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct AppleHypervisor;

impl AppleHypervisor {
    pub fn new() -> Self {
        Self
    }

    /// Whether this Mac can run VMs at all (Apple silicon or VT-x, not nested without support).
    pub fn is_supported() -> bool {
        // SAFETY: class method with no arguments.
        unsafe { VZVirtualMachine::isSupported() }
    }

    /// Boot and return the concrete handle (gives access to the console log path).
    pub fn boot_vm(&self, config: &VmConfig) -> Result<AppleVm, VmmError> {
        config.validate()?;
        if !Self::is_supported() {
            return Err(VmmError::Unavailable("Virtualization.framework reports this Mac can't run VMs".into()));
        }
        for (what, path) in [("kernel", &config.kernel), ("initrd", &config.initrd)] {
            if !path.is_file() {
                return Err(VmmError::Framework(format!("{what} not found at {} (run scripts/build-guest.sh)", path.display())));
            }
        }

        let console_log = config.console_log.clone().unwrap_or_else(|| std::env::temp_dir().join("graviton-console.log"));
        // SAFETY: building a configuration object graph on this thread; nothing is shared yet.
        let configuration = unsafe { build_configuration(config, &console_log)? };

        let queue = DispatchQueue::new("dev.graviton.vm", None);
        // SAFETY: the VM is bound to `queue`; all later access goes through `on_queue`.
        let vm = unsafe { VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &configuration, &queue) };
        let vm = AppleVm { vm: Sendable(vm), queue, console_log };

        let (tx, rx) = mpsc::channel();
        let done = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(error_text(err));
        });
        let done = Sendable(done);
        vm.on_queue(|machine| unsafe { machine.startWithCompletionHandler(done.get()) });
        match rx.recv_timeout(START_TIMEOUT) {
            Ok(None) => Ok(vm),
            Ok(Some(message)) => Err(VmmError::Framework(format!("starting VM: {message}"))),
            Err(_) => Err(VmmError::Timeout("waiting for the VM to start")),
        }
    }
}

impl Hypervisor for AppleHypervisor {
    fn name(&self) -> &'static str {
        "Virtualization.framework"
    }

    fn boot(&self, config: &VmConfig) -> Result<Box<dyn VmHandle>, VmmError> {
        Ok(Box::new(self.boot_vm(config)?))
    }
}

/// Wrapper for Objective-C objects we only ever touch on the VM's queue.
struct Sendable<T>(T);
// SAFETY: the contents are only used inside `exec_sync` on the VM's serial queue
// (or, for blocks, handed to the framework, which invokes them on that queue).
unsafe impl<T> Send for Sendable<T> {}
// SAFETY: as above; shared references never touch the object off-queue.
unsafe impl<T> Sync for Sendable<T> {}

impl<T> Sendable<T> {
    /// Borrow through a method so closures capture the wrapper, not the (non-Send) field.
    fn get(&self) -> &T {
        &self.0
    }
}

pub struct AppleVm {
    vm: Sendable<Retained<VZVirtualMachine>>,
    queue: DispatchRetained<DispatchQueue>,
    console_log: PathBuf,
}

impl AppleVm {
    /// Where the guest's hvc0 console is written.
    pub fn console_log(&self) -> &Path {
        &self.console_log
    }

    /// Run `f` on the VM's queue and return its result.
    fn on_queue<R: Send>(&self, f: impl FnOnce(&VZVirtualMachine) -> R + Send) -> R {
        let mut out = None;
        let vm = &self.vm;
        self.queue.exec_sync(|| out = Some(f(vm.get())));
        out.expect("exec_sync runs the closure before returning")
    }

    fn raw_state(&self) -> VZVirtualMachineState {
        // SAFETY: called on the VM's queue.
        self.on_queue(|vm| unsafe { vm.state() })
    }

    fn connect_once(&self, port: u32) -> Result<OwnedFd, String> {
        let (tx, rx) = mpsc::channel::<Result<OwnedFd, String>>();
        let done = RcBlock::new(move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
            let result = if let Some(message) = error_text(err) {
                Err(message)
            } else if conn.is_null() {
                Err("no connection".into())
            } else {
                // SAFETY: the framework keeps `conn` alive for the block's duration. We dup
                // its descriptor so the stream outlives the connection object.
                let fd = unsafe { (*conn).fileDescriptor() };
                unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned().map_err(|e| e.to_string())
            };
            let _ = tx.send(result);
        });
        let done = Sendable(done);
        let started = self.on_queue(|vm| {
            // SAFETY: on the VM's queue; socketDevices is non-empty because we configured one.
            let devices = unsafe { vm.socketDevices() };
            let Some(device) = devices.firstObject() else { return false };
            let Ok(device) = device.downcast::<VZVirtioSocketDevice>() else { return false };
            unsafe { device.connectToPort_completionHandler(port, done.get()) };
            true
        });
        if !started {
            return Err("VM has no vsock device".into());
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| Err("vsock connect timed out".into()))
    }

    fn stop_with(&self, hard: bool) -> Result<(), VmmError> {
        if hard {
            let (tx, rx) = mpsc::channel();
            let done = Sendable(RcBlock::new(move |err: *mut NSError| {
                let _ = tx.send(error_text(err));
            }));
            // SAFETY: on the VM's queue.
            self.on_queue(|vm| unsafe { vm.stopWithCompletionHandler(done.get()) });
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(None) => Ok(()),
                Ok(Some(message)) => Err(VmmError::Framework(format!("stopping VM: {message}"))),
                Err(_) => Err(VmmError::Timeout("waiting for the VM to stop")),
            }
        } else {
            // SAFETY: on the VM's queue.
            self.on_queue(|vm| unsafe { vm.requestStopWithError() })
                .map_err(|e| VmmError::Framework(format!("requesting stop: {}", e.localizedDescription())))
        }
    }
}

impl VmHandle for AppleVm {
    fn state(&self) -> VmState {
        match self.raw_state() {
            VZVirtualMachineState::Stopped => VmState::Stopped,
            VZVirtualMachineState::Running => VmState::Running,
            VZVirtualMachineState::Paused | VZVirtualMachineState::Pausing | VZVirtualMachineState::Saving => VmState::Paused,
            VZVirtualMachineState::Starting | VZVirtualMachineState::Resuming | VZVirtualMachineState::Restoring => VmState::Starting,
            VZVirtualMachineState::Stopping => VmState::Stopping,
            _ => VmState::Error,
        }
    }

    /// Connect to a guest port, retrying while the guest boots and starts listening.
    fn connect(&self, port: u32) -> Result<OwnedFd, VmmError> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.connect_once(port) {
                Ok(fd) => return Ok(fd),
                Err(message) => {
                    if self.state() != VmState::Running {
                        return Err(VmmError::Framework(format!("VM is no longer running ({message})")));
                    }
                    if Instant::now() >= deadline {
                        return Err(VmmError::Framework(format!("connecting to vsock port {port}: {message}")));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn request_stop(&self) -> Result<(), VmmError> {
        self.stop_with(false)
    }

    fn kill(&self) -> Result<(), VmmError> {
        self.stop_with(true)
    }

    fn wait_stopped(&self, timeout: Duration) -> Result<(), VmmError> {
        let deadline = Instant::now() + timeout;
        while matches!(self.state(), VmState::Running | VmState::Stopping | VmState::Starting) {
            if Instant::now() >= deadline {
                return Err(VmmError::Timeout("waiting for the guest to power off"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

impl Drop for AppleVm {
    fn drop(&mut self) {
        if matches!(self.state(), VmState::Running | VmState::Paused | VmState::Starting) {
            let _ = self.kill();
        }
    }
}

/// Build the `VZVirtualMachineConfiguration` for `config`.
///
/// # Safety
/// Must be called before the configuration is shared with a `VZVirtualMachine`.
unsafe fn build_configuration(config: &VmConfig, console_log: &Path) -> Result<Retained<VZVirtualMachineConfiguration>, VmmError> {
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(&config.kernel));
        boot.setInitialRamdiskURL(Some(&file_url(&config.initrd)));
        boot.setCommandLine(&NSString::from_str(&config.cmdline));

        let c = VZVirtualMachineConfiguration::new();
        let boot: &VZBootLoader = &boot;
        c.setBootLoader(Some(boot));
        c.setCPUCount(config.cpus as usize);
        c.setMemorySize(config.memory_mib * 1024 * 1024);
        let platform = VZGenericPlatformConfiguration::new();
        let platform: &VZPlatformConfiguration = &platform;
        c.setPlatform(platform);

        // Console (hvc0) → log file.
        let attachment = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &file_url(console_log),
            false,
        )
        .map_err(|e| VmmError::Framework(format!("console log {}: {}", console_log.display(), e.localizedDescription())))?;
        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let attachment: &VZSerialPortAttachment = &attachment;
        console.setAttachment(Some(attachment));
        c.setSerialPorts(&NSArray::<VZSerialPortConfiguration>::from_retained_slice(&[console.into_super()]));

        c.setEntropyDevices(&NSArray::<VZEntropyDeviceConfiguration>::from_retained_slice(&[
            VZVirtioEntropyDeviceConfiguration::new().into_super(),
        ]));
        c.setSocketDevices(&NSArray::<VZSocketDeviceConfiguration>::from_retained_slice(&[
            VZVirtioSocketDeviceConfiguration::new().into_super(),
        ]));

        if config.network == NetworkMode::Nat {
            let net = VZVirtioNetworkDeviceConfiguration::new();
            let nat = VZNATNetworkDeviceAttachment::new();
            let nat: &VZNetworkDeviceAttachment = &nat;
            net.setAttachment(Some(nat));
            c.setNetworkDevices(&NSArray::<VZNetworkDeviceConfiguration>::from_retained_slice(&[net.into_super()]));
        }

        let shares: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = config
            .shares
            .iter()
            .map(|s| {
                let dir = VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &file_url(&s.host_path), s.read_only);
                let share = VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
                let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &NSString::from_str(&s.tag),
                );
                fs.setShare(Some(&share.into_super()));
                fs.into_super()
            })
            .collect();
        if !shares.is_empty() {
            c.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
        }

        c.validateWithError()
            .map_err(|e| VmmError::Framework(format!("invalid VM configuration: {}", e.localizedDescription())))?;
        Ok(c)
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn error_text(err: *mut NSError) -> Option<String> {
    if err.is_null() {
        None
    } else {
        // SAFETY: the framework passes a valid NSError (or nil) to completion handlers.
        Some(unsafe { &*err }.localizedDescription().to_string())
    }
}
