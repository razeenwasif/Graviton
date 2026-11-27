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
//! | rootfs (a disk) | `VZVirtioBlockDeviceConfiguration` + a disk image, read-write     |
//!
//! The calling process must be signed with the
//! `com.apple.security.virtualization` entitlement (see `scripts/dev-gravitond.sh`).
//!
//! Threading: a `VZVirtualMachine` may only be touched on the dispatch queue
//! it was created with, and its completion handlers run there too. Every call
//! below hops onto that queue with `exec_sync` and waits for completion
//! handlers on a channel *outside* the queue.

use std::ffi::c_void;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSError, NSFileHandle, NSNumber, NSString, NSUnderlyingErrorKey, NSURL};
use objc2_virtualization::{
    VZBootLoader, VZDirectorySharingDeviceConfiguration, VZDiskImageCachingMode, VZDiskImageStorageDeviceAttachment,
    VZDiskImageSynchronizationMode, VZEntropyDeviceConfiguration, VZFileHandleNetworkDeviceAttachment, VZFileSerialPortAttachment,
    VZGenericMachineIdentifier, VZGenericPlatformConfiguration, VZLinuxBootLoader, VZLinuxRosettaAvailability,
    VZLinuxRosettaDirectoryShare, VZMACAddress, VZMemoryBalloonDeviceConfiguration, VZMultipleDirectoryShare,
    VZNATNetworkDeviceAttachment, VZNetworkDeviceAttachment, VZNetworkDeviceConfiguration, VZPlatformConfiguration,
    VZSerialPortAttachment, VZSerialPortConfiguration, VZSharedDirectory, VZSingleDirectoryShare, VZSocketDeviceConfiguration,
    VZStorageDeviceAttachment, VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioEntropyDeviceConfiguration, VZVirtioFileSystemDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration,
    VZVirtioTraditionalMemoryBalloonDevice, VZVirtioTraditionalMemoryBalloonDeviceConfiguration, VZVirtualMachine,
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

    /// Whether Apple's Rosetta for Linux binaries is installed on this Mac.
    pub fn rosetta_available() -> bool {
        // SAFETY: class method with no arguments.
        unsafe { VZLinuxRosettaDirectoryShare::availability() == VZLinuxRosettaAvailability::Installed }
    }

    /// Minimum VM memory size accepted by Virtualization.framework on this host.
    pub fn minimum_allowed_memory_bytes() -> u64 {
        // SAFETY: class method with no arguments.
        unsafe { VZVirtualMachineConfiguration::minimumAllowedMemorySize() }
    }

    /// Generate a fresh serialisable machine identifier for VM snapshot save/restore.
    pub fn new_machine_id() -> Vec<u8> {
        // SAFETY: creating a brand new VZGenericMachineIdentifier and reading its bytes.
        unsafe { VZGenericMachineIdentifier::new().dataRepresentation().to_vec() }
    }

    /// Whether the macOS console login session is active and unlocked so SEP-backed
    /// `.vzsave` decryption (`kSecAttrAccessibleWhenUnlocked`) can succeed.
    pub fn session_unlocked() -> bool {
        // SAFETY: CoreGraphics returns an owned CFDictionary (or NULL) that we release below.
        let dict = unsafe { CGSessionCopyCurrentDictionary() };
        if dict.is_null() {
            return false;
        }
        let screen_locked = cf_dict_bool(dict, "CGSSessionScreenIsLocked");
        let on_console = cf_dict_bool(dict, "kCGSSessionOnConsoleKey");
        let login_done = cf_dict_bool(dict, "kCGSessionLoginDoneKey");
        // SAFETY: `dict` is a non-null +1 CF object returned by `CGSessionCopyCurrentDictionary`.
        unsafe { CFRelease(dict) };
        screen_locked != Some(true) && on_console != Some(false) && login_done == Some(true)
    }

    /// Boot and return the concrete handle (gives access to the console log path).
    pub fn boot_vm(&self, config: &VmConfig) -> Result<AppleVm, VmmError> {
        self.boot_vm_inner(config, false)
    }

    /// Boot after validating that Virtualization.framework supports saving and
    /// restoring this exact VM configuration (`validateSaveRestoreSupport`).
    pub fn boot_vm_saveable(&self, config: &VmConfig) -> Result<AppleVm, VmmError> {
        self.boot_vm_inner(config, true)
    }

    fn boot_vm_inner(&self, config: &VmConfig, require_save_restore: bool) -> Result<AppleVm, VmmError> {
        let vm = self.prepare_vm(config, require_save_restore)?;
        let (tx, rx) = mpsc::channel();
        let done = Sendable(RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(error_text(err));
        }));
        vm.on_queue(|machine| unsafe { machine.startWithCompletionHandler(done.get()) });
        match rx.recv_timeout(START_TIMEOUT) {
            Ok(None) => Ok(vm),
            Ok(Some(message)) => Err(VmmError::Framework(format!("starting VM: {message}"))),
            Err(_) => Err(VmmError::Timeout("waiting for the VM to start")),
        }
    }

    /// Restore a VM from a saved `.vzsave` state file and resume execution.
    pub fn restore_vm(&self, config: &VmConfig, state_path: &Path) -> Result<AppleVm, VmmError> {
        if !state_path.is_file() {
            return Err(VmmError::Framework(format!("snapshot not found at {}", state_path.display())));
        }
        let vm = self.prepare_vm(config, true)?;
        let state_url = Sendable(file_url(state_path));
        let (tx, rx) = mpsc::channel();
        let done = Sendable(RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(restore_error_info(err));
        }));
        vm.on_queue(|machine| unsafe { machine.restoreMachineStateFromURL_completionHandler(state_url.get(), done.get()) });
        match rx.recv_timeout(START_TIMEOUT) {
            Ok(None) => {}
            Ok(Some((message, true))) => return Err(VmmError::SnapshotLocked(format!("restoring VM snapshot: {message}"))),
            Ok(Some((message, false))) => return Err(VmmError::Framework(format!("restoring VM snapshot: {message}"))),
            Err(_) => return Err(VmmError::Timeout("waiting for the VM snapshot to restore")),
        }
        vm.resume()?;
        Ok(vm)
    }

    fn prepare_vm(&self, config: &VmConfig, require_save_restore: bool) -> Result<AppleVm, VmmError> {
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
        let configuration = unsafe { build_configuration(config, &console_log, require_save_restore)? };

        let queue = DispatchQueue::new("dev.graviton.vm", None);
        // SAFETY: the VM is bound to `queue`; all later access goes through `on_queue`.
        let vm = unsafe { VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &configuration, &queue) };
        Ok(AppleVm { vm: Sendable(vm), queue, console_log })
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

    /// Pause a running VM (required before `save_state`).
    pub fn pause(&self) -> Result<(), VmmError> {
        let (tx, rx) = mpsc::channel();
        let done = Sendable(RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(error_text(err));
        }));
        self.on_queue(|vm| unsafe { vm.pauseWithCompletionHandler(done.get()) });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(None) => Ok(()),
            Ok(Some(message)) => Err(VmmError::Framework(format!("pausing VM: {message}"))),
            Err(_) => Err(VmmError::Timeout("waiting for the VM to pause")),
        }
    }

    /// Resume a paused (or freshly restored) VM.
    pub fn resume(&self) -> Result<(), VmmError> {
        let (tx, rx) = mpsc::channel();
        let done = Sendable(RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(error_text(err));
        }));
        self.on_queue(|vm| unsafe { vm.resumeWithCompletionHandler(done.get()) });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(None) => Ok(()),
            Ok(Some(message)) => Err(VmmError::Framework(format!("resuming VM: {message}"))),
            Err(_) => Err(VmmError::Timeout("waiting for the VM to resume")),
        }
    }

    /// Save a paused VM's state to `path` (host-encrypted `.vzsave`).
    pub fn save_state(&self, path: &Path) -> Result<(), VmmError> {
        let _ = std::fs::remove_file(path);
        let url = Sendable(file_url(path));
        let (tx, rx) = mpsc::channel();
        let done = Sendable(RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(error_text(err));
        }));
        self.on_queue(|vm| unsafe { vm.saveMachineStateToURL_completionHandler(url.get(), done.get()) });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(None) => Ok(()),
            Ok(Some(message)) => Err(VmmError::Framework(format!("saving VM state: {message}"))),
            Err(_) => Err(VmmError::Timeout("waiting for the VM state to save")),
        }
    }

    /// Ask the guest's `virtio-balloon` driver to adjust physical RAM to `bytes`.
    pub fn set_balloon_target_bytes(&self, bytes: u64) {
        self.on_queue(|vm| unsafe {
            let devices = vm.memoryBalloonDevices();
            if let Some(dev) = devices.firstObject() {
                if let Ok(balloon) = dev.downcast::<VZVirtioTraditionalMemoryBalloonDevice>() {
                    balloon.setTargetVirtualMachineMemorySize(bytes);
                }
            }
        });
    }

    /// Current target physical RAM (bytes) configured on the VM's `virtio-balloon`.
    pub fn balloon_target_bytes(&self) -> Option<u64> {
        self.on_queue(|vm| unsafe {
            let devices = vm.memoryBalloonDevices();
            devices
                .firstObject()
                .and_then(|dev| dev.downcast::<VZVirtioTraditionalMemoryBalloonDevice>().ok())
                .map(|balloon| balloon.targetVirtualMachineMemorySize())
        })
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
        let start = Instant::now();
        let deadline = start + Duration::from_secs(20);
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
                    let delay = if start.elapsed() < Duration::from_secs(2) { 10 } else { 50 };
                    std::thread::sleep(Duration::from_millis(delay));
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
unsafe fn build_configuration(
    config: &VmConfig,
    console_log: &Path,
    require_save_restore: bool,
) -> Result<Retained<VZVirtualMachineConfiguration>, VmmError> {
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(&config.kernel));
        boot.setInitialRamdiskURL(Some(&file_url(&config.initrd)));
        let mut cmdline = if config.cmdline.split_whitespace().any(|tok| tok.starts_with("nr_cpus=")) {
            config.cmdline.clone()
        } else if config.cmdline.is_empty() {
            format!("nr_cpus={}", config.cpus)
        } else {
            format!("{} nr_cpus={}", config.cmdline, config.cpus)
        };
        if config.network == NetworkMode::None && !cmdline.split_whitespace().any(|tok| tok == "graviton.no_nat=1") {
            cmdline.push_str(" graviton.no_nat=1");
        }
        boot.setCommandLine(&NSString::from_str(&cmdline));

        let c = VZVirtualMachineConfiguration::new();
        let boot: &VZBootLoader = &boot;
        c.setBootLoader(Some(boot));
        c.setCPUCount(config.cpus as usize);
        c.setMemorySize(config.memory_mib * 1024 * 1024);
        let platform = VZGenericPlatformConfiguration::new();
        if let Some(id) = &config.identity {
            let data = NSData::with_bytes(&id.machine_id);
            let mid = VZGenericMachineIdentifier::initWithDataRepresentation(VZGenericMachineIdentifier::alloc(), &data)
                .ok_or_else(|| VmmError::Framework("invalid saved machine identifier".into()))?;
            platform.setMachineIdentifier(&mid);
        }
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
        if config.balloon {
            c.setMemoryBalloonDevices(&NSArray::<VZMemoryBalloonDeviceConfiguration>::from_retained_slice(&[
                VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new().into_super(),
            ]));
        }

        let mut nics: Vec<Retained<VZNetworkDeviceConfiguration>> = Vec::new();
        if config.network == NetworkMode::Nat {
            // eth0: NAT, for the internet and the Mac.
            let net = VZVirtioNetworkDeviceConfiguration::new();
            if let Some(mac_str) = config.identity.as_ref().and_then(|i| i.nat_mac.as_deref()) {
                let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(mac_str))
                    .ok_or_else(|| VmmError::Framework(format!("invalid MAC address `{mac_str}`")))?;
                net.setMACAddress(&mac);
            }
            let nat = VZNATNetworkDeviceAttachment::new();
            let nat: &VZNetworkDeviceAttachment = &nat;
            net.setAttachment(Some(nat));
            nics.push(net.into_super());
        }
        if let Some(fd) = config.switch_port {
            // eth1: raw frames to gravitond's switch, for other containers.
            let handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), fd, false);
            let attachment = VZFileHandleNetworkDeviceAttachment::initWithFileHandle(VZFileHandleNetworkDeviceAttachment::alloc(), &handle);
            let net = VZVirtioNetworkDeviceConfiguration::new();
            if let Some(mac_str) = config.identity.as_ref().and_then(|i| i.switch_mac.as_deref()) {
                let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(mac_str))
                    .ok_or_else(|| VmmError::Framework(format!("invalid MAC address `{mac_str}`")))?;
                net.setMACAddress(&mac);
            }
            let attachment: &VZNetworkDeviceAttachment = &attachment;
            net.setAttachment(Some(attachment));
            nics.push(net.into_super());
        }
        if !nics.is_empty() {
            c.setNetworkDevices(&NSArray::<VZNetworkDeviceConfiguration>::from_retained_slice(&nics));
        }

        let mut shares: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        if let Some(tag) = &config.multi_share_tag {
            let keys: Vec<Retained<NSString>> = config.shares.iter().map(|s| NSString::from_str(&s.tag)).collect();
            let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
            let dirs: Vec<Retained<VZSharedDirectory>> = config
                .shares
                .iter()
                .map(|s| VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &file_url(&s.host_path), s.read_only))
                .collect();
            let dict = NSDictionary::from_retained_objects(&key_refs, &dirs);
            let share = VZMultipleDirectoryShare::initWithDirectories(VZMultipleDirectoryShare::alloc(), &dict);
            let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str(tag),
            );
            fs.setShare(Some(&share.into_super()));
            shares.push(fs.into_super());
        } else {
            for s in &config.shares {
                let dir = VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &file_url(&s.host_path), s.read_only);
                let share = VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
                let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &NSString::from_str(&s.tag),
                );
                fs.setShare(Some(&share.into_super()));
                shares.push(fs.into_super());
            }
        }
        if config.rosetta {
            let rosetta = VZLinuxRosettaDirectoryShare::initWithError(VZLinuxRosettaDirectoryShare::alloc()).map_err(|e| {
                VmmError::Unavailable(format!(
                    "Rosetta for Linux isn't available ({}; run `softwareupdate --install-rosetta`)",
                    e.localizedDescription()
                ))
            })?;
            let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str("rosetta"),
            );
            fs.setShare(Some(&rosetta.into_super()));
            shares.push(fs.into_super());
        }
        if !shares.is_empty() {
            c.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
        }

        // `/dev/vda`: read-write container scratch disk (uncached on host to avoid double caching).
        // `/dev/vdb`: optional read-only EROFS image disk shared across containers (`lowerdir`).
        let mut storage: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
        if !config.rootfs.as_os_str().is_empty() {
            let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &file_url(&config.rootfs),
                false,
                VZDiskImageCachingMode::Uncached,
                VZDiskImageSynchronizationMode::None,
            )
            .map_err(|e| VmmError::Framework(format!("disk {}: {}", config.rootfs.display(), e.localizedDescription())))?;
            let disk: &VZStorageDeviceAttachment = &disk;
            let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), disk);
            storage.push(block.into_super());
        }
        if let Some(img) = &config.image_disk {
            let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &file_url(img),
                true,
                VZDiskImageCachingMode::Cached,
                VZDiskImageSynchronizationMode::None,
            )
            .map_err(|e| VmmError::Framework(format!("image disk {}: {}", img.display(), e.localizedDescription())))?;
            let disk: &VZStorageDeviceAttachment = &disk;
            let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), disk);
            storage.push(block.into_super());
        }
        if !storage.is_empty() {
            c.setStorageDevices(&NSArray::<VZStorageDeviceConfiguration>::from_retained_slice(&storage));
        }

        c.validateWithError()
            .map_err(|e| VmmError::Framework(format!("invalid VM configuration: {}", e.localizedDescription())))?;
        if require_save_restore {
            c.validateSaveRestoreSupportWithError()
                .map_err(|e| VmmError::Framework(format!("VM configuration can't be saved/restored: {}", e.localizedDescription())))?;
        }
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

fn is_locked_restore_error(domain: &str, code: isize, description: &str) -> bool {
    (domain == "NSOSStatusErrorDomain" && code == -25308)
        || code == -536_870_174
        || description.to_ascii_lowercase().contains("permission denied")
}

fn error_chain_locked(err: &NSError, depth: usize) -> bool {
    if depth >= 4 {
        return false;
    }
    let domain = err.domain().to_string();
    let code = err.code();
    let desc = err.localizedDescription().to_string();
    if is_locked_restore_error(&domain, code, &desc) {
        return true;
    }
    let info = err.userInfo();
    let aks_key = NSString::from_str("AKSError");
    if let Some(aks_obj) = info.objectForKey(&aks_key) {
        if let Ok(num) = aks_obj.downcast::<NSNumber>() {
            if num.integerValue() == -536_870_174 {
                return true;
            }
        }
    }
    // SAFETY: `NSUnderlyingErrorKey` is a static Foundation constant key.
    let underlying_key = unsafe { NSUnderlyingErrorKey };
    if let Some(next_obj) = info.objectForKey(underlying_key) {
        if let Ok(next_err) = next_obj.downcast::<NSError>() {
            return error_chain_locked(&next_err, depth + 1);
        }
    }
    false
}

fn restore_error_info(err: *mut NSError) -> Option<(String, bool)> {
    if err.is_null() {
        None
    } else {
        // SAFETY: the framework passes a valid NSError (or nil) to completion handlers.
        let err_ref = unsafe { &*err };
        Some((err_ref.localizedDescription().to_string(), error_chain_locked(err_ref, 0)))
    }
}

type CFTypeID = usize;
type CFIndex = isize;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: bool,
    ) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFGetTypeID(cf: *const c_void) -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFBooleanGetValue(boolean: *const c_void) -> bool;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFNumberGetValue(number: *const c_void, the_type: CFIndex, value_ptr: *mut c_void) -> bool;
    fn CFRelease(cf: *const c_void);
}

fn cf_dict_bool(dict: *const c_void, key: &str) -> Option<bool> {
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_SINT32_TYPE: CFIndex = 3;
    // SAFETY: `dict` is a valid CFDictionaryRef for the duration of this call and `cf_key` is released before returning.
    unsafe {
        let cf_key = CFStringCreateWithBytes(
            std::ptr::null(),
            key.as_ptr(),
            key.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
            false,
        );
        if cf_key.is_null() {
            return None;
        }
        let val = CFDictionaryGetValue(dict, cf_key);
        CFRelease(cf_key);
        if val.is_null() {
            return None;
        }
        let tid = CFGetTypeID(val);
        if tid == CFBooleanGetTypeID() {
            Some(CFBooleanGetValue(val))
        } else if tid == CFNumberGetTypeID() {
            let mut n: i32 = 0;
            if CFNumberGetValue(val, K_CF_NUMBER_SINT32_TYPE, (&raw mut n).cast()) {
                Some(n != 0)
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppleHypervisor, is_locked_restore_error};

    #[test]
    fn session_unlocked_probe_does_not_crash() {
        let _ = AppleHypervisor::session_unlocked();
    }

    #[test]
    fn classifies_locked_restore_errors() {
        assert!(is_locked_restore_error("NSOSStatusErrorDomain", -25308, "sec error"));
        assert!(is_locked_restore_error("AKSError", -536_870_174, "aks error"));
        assert!(is_locked_restore_error(
            "VZErrorDomain",
            12,
            "The virtual machine failed to restore with error “permission denied”."
        ));
        assert!(!is_locked_restore_error(
            "VZErrorDomain",
            12,
            "The virtual machine configuration is incompatible with the saved state."
        ));
    }
}
