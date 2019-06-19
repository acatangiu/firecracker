// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Virtual Machine Monitor that leverages the Linux Kernel-based Virtual Machine (KVM),
//! and other virtualization features to run a single lightweight micro-virtual
//! machine (microVM).
#![deny(missing_docs)]
extern crate chrono;
extern crate epoll;
extern crate futures;
extern crate kvm_bindings;
extern crate libc;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate time;
extern crate timerfd;

extern crate arch;
#[cfg(target_arch = "x86_64")]
extern crate cpuid;
extern crate devices;
extern crate fc_util;
extern crate kernel;
extern crate kvm;
#[macro_use]
extern crate logger;
extern crate core;
extern crate memory_model;
extern crate net_util;
extern crate rate_limiter;
extern crate seccomp;
extern crate sys_util;

/// Syscalls allowed through the seccomp filter.
pub mod default_syscalls;
mod device_manager;
/// Signal handling utilities.
pub mod signal_handler;
mod snapshot;
/// Wrappers over structures used to configure the VMM.
pub mod vmm_config;
mod vstate;

use futures::sync::oneshot;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::fs::{metadata, File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::result;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Barrier, RwLock};
use std::thread;
use std::time::Duration;

use timerfd::{ClockId, SetTimeFlags, TimerFd, TimerState};

use arch::DeviceType;
use device_manager::legacy::LegacyDeviceManager;
#[cfg(target_arch = "aarch64")]
use device_manager::mmio::MMIODeviceInfo;
use device_manager::mmio::MMIODeviceManager;
use devices::legacy::I8042DeviceError;
use devices::virtio;
#[cfg(feature = "vsock")]
use devices::virtio::vhost::{handle::VHOST_EVENTS_COUNT, TYPE_VSOCK};
use devices::virtio::{EpollConfigConstructor, MmioDevice, MmioDeviceState, MmioDeviceStateError};
use devices::virtio::{BLOCK_EVENTS_COUNT, TYPE_BLOCK};
use devices::virtio::{NET_EVENTS_COUNT, TYPE_NET};
use devices::{DeviceEventT, EpollHandler};
use fc_util::now_cputime_us;
use kernel::cmdline as kernel_cmdline;
use kernel::loader as kernel_loader;
use kvm::*;
use logger::error::LoggerError;
use logger::{AppInfo, Level, LogOption, Metric, LOGGER, METRICS};
use memory_model::{FileMemoryDesc, GuestAddress, GuestMemory, GuestMemoryError};
use net_util::TapError;
#[cfg(target_arch = "aarch64")]
use serde_json::Value;
#[cfg(target_arch = "x86_64")]
use snapshot::*;
use sys_util::{EventFd, Terminal};
use vmm_config::boot_source::{BootSourceConfig, BootSourceConfigError};
use vmm_config::drive::{BlockDeviceConfig, BlockDeviceConfigs, DriveError};
use vmm_config::instance_info::{
    InstanceInfo, InstanceState, PauseMicrovmError, ResumeMicrovmError, StartMicrovmError,
    StateError,
};
use vmm_config::logger::{LoggerConfig, LoggerConfigError, LoggerLevel};
use vmm_config::machine_config::{VmConfig, VmConfigError};
use vmm_config::net::{
    NetworkInterfaceConfig, NetworkInterfaceConfigs, NetworkInterfaceError,
    NetworkInterfaceUpdateConfig,
};
#[cfg(feature = "vsock")]
use vmm_config::vsock::{VsockDeviceConfig, VsockDeviceConfigs, VsockError};
#[cfg(target_arch = "x86_64")]
use vstate::VcpuState;
use vstate::{Vcpu, VcpuEvent, VcpuHandle, VcpuResponse, Vm};

/// Default guest kernel command line:
/// - `reboot=k` shut down the guest on reboot, instead of well... rebooting;
/// - `panic=1` on panic, reboot after 1 second;
/// - `pci=off` do not scan for PCI devices (save boot time);
/// - `nomodules` disable loadable kernel module support;
/// - `8250.nr_uarts=0` disable 8250 serial interface;
/// - `i8042.noaux` do not probe the i8042 controller for an attached mouse (save boot time);
/// - `i8042.nomux` do not probe i8042 for a multiplexing controller (save boot time);
/// - `i8042.nopnp` do not use ACPIPnP to discover KBD/AUX controllers (save boot time);
/// - `i8042.dumbkbd` do not attempt to control kbd state via the i8042 (save boot time).
const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=1 pci=off nomodules 8250.nr_uarts=0 \
                                      i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd";
const WRITE_METRICS_PERIOD_SECONDS: u64 = 60;

/// Success exit code.
pub const FC_EXIT_CODE_OK: u8 = 0;
/// Generic error exit code.
pub const FC_EXIT_CODE_GENERIC_ERROR: u8 = 1;
/// Generic exit code for an error considered not possible to occur if the program logic is sound.
pub const FC_EXIT_CODE_UNEXPECTED_ERROR: u8 = 2;
/// Firecracker was shut down after intercepting a restricted system call.
pub const FC_EXIT_CODE_BAD_SYSCALL: u8 = 148;
/// Firecracker was shut down after intercepting `SIGBUS`.
pub const FC_EXIT_CODE_SIGBUS: u8 = 149;
/// Firecracker was shut down after intercepting `SIGSEGV`.
pub const FC_EXIT_CODE_SIGSEGV: u8 = 150;
/// Firecracker failed to resume from a snapshot.
#[cfg(target_arch = "x86_64")]
pub const FC_EXIT_CODE_RESUME_ERROR: u8 = 151;

/// Errors associated with the VMM internal logic. These errors cannot be generated by direct user
/// input, but can result from bad configuration of the host (for example if Firecracker doesn't
/// have permissions to open the KVM fd).
pub enum Error {
    /// Cannot receive message from the API.
    ApiChannel,
    /// Legacy devices work with Event file descriptors and the creation can fail because
    /// of resource exhaustion.
    CreateLegacyDevice(device_manager::legacy::Error),
    /// An operation on the epoll instance failed due to resource exhaustion or bad configuration.
    EpollFd(io::Error),
    /// Cannot read from an Event file descriptor.
    EventFd(io::Error),
    /// An event arrived for a device, but the dispatcher can't find the event (epoll) handler.
    DeviceEventHandlerNotFound,
    /// An epoll handler can't be downcasted to the desired type.
    DeviceEventHandlerInvalidDowncast,
    /// Cannot open /dev/kvm. Either the host does not have KVM or Firecracker does not have
    /// permission to open the file descriptor.
    Kvm(io::Error),
    /// The host kernel reports an invalid KVM API version.
    KvmApiVersion(i32),
    /// Cannot initialize the KVM context due to missing capabilities.
    KvmCap(kvm::Cap),
    /// Epoll wait failed.
    Poll(io::Error),
    /// Write to the serial console failed.
    Serial(io::Error),
    /// Cannot create Timer file descriptor.
    TimerFd(io::Error),
    /// Cannot open the VM file descriptor.
    Vm(vstate::Error),
}

// Implementing Debug as these errors are mostly used in panics & expects.
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            ApiChannel => write!(f, "ApiChannel: error receiving data from the API server"),
            CreateLegacyDevice(e) => write!(f, "Error creating legacy device: {:?}", e),
            EpollFd(e) => write!(f, "Epoll fd error: {}", e.to_string()),
            EventFd(e) => write!(f, "Event fd error: {}", e.to_string()),
            DeviceEventHandlerNotFound => write!(
                f,
                "Device event handler not found. This might point to a guest device driver issue."
            ),
            DeviceEventHandlerInvalidDowncast => write!(
                f,
                "Device event handler couldn't be downcasted to expected type."
            ),
            Kvm(os_err) => write!(f, "Cannot open /dev/kvm. Error: {}", os_err.to_string()),
            KvmApiVersion(ver) => write!(f, "Bad KVM API version: {}", ver),
            KvmCap(cap) => write!(f, "Missing KVM capability: {:?}", cap),
            Poll(e) => write!(f, "Epoll wait failed: {}", e.to_string()),
            Serial(e) => write!(f, "Error writing to the serial console: {:?}", e),
            TimerFd(e) => write!(f, "Error creating timer fd: {}", e.to_string()),
            Vm(e) => write!(f, "Error opening VM fd: {:?}", e),
        }
    }
}

/// Types of errors associated with vmm actions.
#[derive(Clone, Debug)]
pub enum ErrorKind {
    /// User Errors describe bad configuration (user input).
    User,
    /// Internal Errors are unrelated to the user and usually refer to logical errors
    /// or bad management of resources (memory, file descriptors & others).
    Internal,
}

impl PartialEq for ErrorKind {
    fn eq(&self, other: &ErrorKind) -> bool {
        match (self, other) {
            (&ErrorKind::User, &ErrorKind::User) => true,
            (&ErrorKind::Internal, &ErrorKind::Internal) => true,
            _ => false,
        }
    }
}

/// Wrapper for all errors associated with VMM actions.
#[derive(Debug)]
pub enum VmmActionError {
    /// The action `ConfigureBootSource` failed either because of bad user input (`ErrorKind::User`)
    /// or an internal error (`ErrorKind::Internal`).
    BootSource(ErrorKind, BootSourceConfigError),
    /// One of the actions `InsertBlockDevice`, `RescanBlockDevice` or `UpdateBlockDevicePath`
    /// failed either because of bad user input (`ErrorKind::User`) or an
    /// internal error (`ErrorKind::Internal`).
    DriveConfig(ErrorKind, DriveError),
    /// The action `ConfigureLogger` failed either because of bad user input (`ErrorKind::User`) or
    /// an internal error (`ErrorKind::Internal`).
    Logger(ErrorKind, LoggerConfigError),
    /// One of the actions `GetVmConfiguration` or `SetVmConfiguration` failed either because of bad
    /// input (`ErrorKind::User`) or an internal error (`ErrorKind::Internal`).
    MachineConfig(ErrorKind, VmConfigError),
    /// The action `InsertNetworkDevice` failed either because of bad user input (`ErrorKind::User`)
    /// or an internal error (`ErrorKind::Internal`).
    NetworkConfig(ErrorKind, NetworkInterfaceError),
    /// The action `ResumeFromSnapshot` failed either because of bad user input (`ErrorKind::User`) or
    /// an internal error (`ErrorKind::Internal`).
    PauseMicrovm(ErrorKind, PauseMicrovmError),
    /// The action `ResumeFromSnapshot` failed either because of bad user input (`ErrorKind::User`) or
    /// an internal error (`ErrorKind::Internal`).
    ResumeMicrovm(ErrorKind, ResumeMicrovmError),
    /// The action `StartMicroVm` failed either because of bad user input (`ErrorKind::User`) or
    /// an internal error (`ErrorKind::Internal`).
    StartMicrovm(ErrorKind, StartMicrovmError),
    /// The action `SendCtrlAltDel` failed. Details are provided by the device-specific error
    /// `I8042DeviceError`.
    SendCtrlAltDel(ErrorKind, I8042DeviceError),
    #[cfg(feature = "vsock")]
    /// The action `insert_vsock_device` failed either because of bad user input (`ErrorKind::User`)
    /// or an internal error (`ErrorKind::Internal`).
    VsockConfig(ErrorKind, VsockError),
}

// It's convenient to turn DriveErrors into VmmActionErrors directly.
impl std::convert::From<DriveError> for VmmActionError {
    fn from(e: DriveError) -> Self {
        let kind = match e {
            // User errors.
            DriveError::CannotOpenBlockDevice
            | DriveError::InvalidBlockDeviceID
            | DriveError::InvalidBlockDevicePath
            | DriveError::BlockDevicePathAlreadyExists
            | DriveError::EpollHandlerNotFound
            | DriveError::BlockDeviceUpdateFailed
            | DriveError::OperationNotAllowedPreBoot
            | DriveError::UpdateNotAllowedPostBoot
            | DriveError::RootBlockDeviceAlreadyAdded => ErrorKind::User,
        };
        VmmActionError::DriveConfig(kind, e)
    }
}

// It's convenient to turn VmConfigErrors into VmmActionErrors directly.
impl std::convert::From<VmConfigError> for VmmActionError {
    fn from(e: VmConfigError) -> Self {
        VmmActionError::MachineConfig(
            match e {
                // User errors.
                VmConfigError::InvalidVcpuCount
                | VmConfigError::InvalidMemorySize
                | VmConfigError::UpdateNotAllowedPostBoot => ErrorKind::User,
            },
            e,
        )
    }
}

// It's convenient to turn NetworkInterfaceErrors into VmmActionErrors directly.
impl std::convert::From<NetworkInterfaceError> for VmmActionError {
    fn from(e: NetworkInterfaceError) -> Self {
        let kind = match e {
            // User errors.
            NetworkInterfaceError::GuestMacAddressInUse(_)
            | NetworkInterfaceError::HostDeviceNameInUse(_)
            | NetworkInterfaceError::DeviceIdNotFound
            | NetworkInterfaceError::UpdateNotAllowedPostBoot => ErrorKind::User,
            // Internal errors.
            NetworkInterfaceError::EpollHandlerNotFound(_)
            | NetworkInterfaceError::RateLimiterUpdateFailed(_) => ErrorKind::Internal,
            NetworkInterfaceError::OpenTap(ref te) => match te {
                // User errors.
                TapError::OpenTun(_) | TapError::CreateTap(_) | TapError::InvalidIfname => {
                    ErrorKind::User
                }
                // Internal errors.
                TapError::IoctlError(_) | TapError::NetUtil(_) => ErrorKind::Internal,
            },
        };
        VmmActionError::NetworkConfig(kind, e)
    }
}

impl std::convert::From<PauseMicrovmError> for VmmActionError {
    fn from(e: PauseMicrovmError) -> Self {
        use self::PauseMicrovmError::*;
        use self::StateError::*;
        let kind = match e {
            MicroVMInvalidState(ref err) => match err {
                MicroVMAlreadyRunning | MicroVMIsNotRunning => ErrorKind::User,
                VcpusInvalidState => ErrorKind::Internal,
            },
            #[cfg(target_arch = "x86_64")]
            OpenSnapshotFile(_) => ErrorKind::User,
            VcpuPause => ErrorKind::User,
            InvalidSnapshot
            | SaveMmioDeviceState(_)
            | SaveVmState(_)
            | SaveVcpuState(_)
            | StopVcpus(_)
            | SyncMemory(_)
            | SignalVcpu(_) => ErrorKind::Internal,
            #[cfg(target_arch = "x86_64")]
            SerializeVcpu(_) | SyncHeader(_) => ErrorKind::Internal,
        };
        VmmActionError::PauseMicrovm(kind, e)
    }
}

// It's convenient to turn ResumeMicrovmError into VmmActionErrors directly.
impl std::convert::From<ResumeMicrovmError> for VmmActionError {
    fn from(e: ResumeMicrovmError) -> Self {
        use self::ResumeMicrovmError::*;
        use self::StateError::*;
        let kind = match e {
            MicroVMInvalidState(ref err) => match err {
                MicroVMAlreadyRunning | MicroVMIsNotRunning => ErrorKind::User,
                VcpusInvalidState => ErrorKind::Internal,
            },
            #[cfg(target_arch = "x86_64")]
            OpenSnapshotFile(_) => ErrorKind::User,
            VcpuResume => ErrorKind::User,
            #[cfg(target_arch = "x86_64")]
            DeserializeVcpu(_) => ErrorKind::Internal,
            RestoreVmState(_) | RestoreVcpuState | SignalVcpu(_) | StartMicroVm(_) => {
                ErrorKind::Internal
            }
        };
        VmmActionError::ResumeMicrovm(kind, e)
    }
}

// It's convenient to turn StartMicrovmErrors into VmmActionErrors directly.
impl std::convert::From<StartMicrovmError> for VmmActionError {
    fn from(e: StartMicrovmError) -> Self {
        use self::StateError::*;
        let kind = match e {
            // User errors.
            #[cfg(feature = "vsock")]
            StartMicrovmError::CreateVsockDevice(_) => ErrorKind::User,
            StartMicrovmError::CreateBlockDevice(_)
            | StartMicrovmError::CreateNetDevice(_)
            | StartMicrovmError::KernelCmdline(_)
            | StartMicrovmError::KernelLoader(_)
            | StartMicrovmError::MissingKernelConfig
            | StartMicrovmError::NetDeviceNotConfigured
            | StartMicrovmError::OpenBlockDevice(_)
            | StartMicrovmError::VcpusNotConfigured => ErrorKind::User,
            // Internal errors.
            #[cfg(feature = "vsock")]
            StartMicrovmError::RegisterVsockDevice(_) => ErrorKind::Internal,
            #[cfg(target_arch = "x86_64")]
            StartMicrovmError::SnapshotBackingFile(_) => ErrorKind::Internal,
            StartMicrovmError::ConfigureSystem(_)
            | StartMicrovmError::ConfigureVm(_)
            | StartMicrovmError::CreateRateLimiter(_)
            | StartMicrovmError::DeviceManager
            | StartMicrovmError::EventFd
            | StartMicrovmError::GuestMemory(_)
            | StartMicrovmError::LegacyIOBus(_)
            | StartMicrovmError::RegisterBlockDevice(_)
            | StartMicrovmError::RegisterEvent
            | StartMicrovmError::RegisterMMIODevice(_)
            | StartMicrovmError::RegisterNetDevice(_)
            | StartMicrovmError::SeccompFilters(_)
            | StartMicrovmError::SignalVcpu(_)
            | StartMicrovmError::Vcpu(_)
            | StartMicrovmError::VcpuConfigure(_)
            | StartMicrovmError::VcpusAlreadyPresent
            | StartMicrovmError::VcpuSpawn(_) => ErrorKind::Internal,
            // The only user `LoadCommandline` error is `CommandLineOverflow`.
            StartMicrovmError::LoadCommandline(ref cle) => match cle {
                kernel::cmdline::Error::CommandLineOverflow => ErrorKind::User,
                _ => ErrorKind::Internal,
            },
            StartMicrovmError::MicroVMInvalidState(ref err) => match err {
                MicroVMAlreadyRunning | MicroVMIsNotRunning => ErrorKind::User,
                VcpusInvalidState => ErrorKind::Internal,
            },
        };
        VmmActionError::StartMicrovm(kind, e)
    }
}

impl VmmActionError {
    /// Returns the error type.
    pub fn kind(&self) -> &ErrorKind {
        use self::VmmActionError::*;

        match *self {
            BootSource(ref kind, _) => kind,
            DriveConfig(ref kind, _) => kind,
            Logger(ref kind, _) => kind,
            MachineConfig(ref kind, _) => kind,
            NetworkConfig(ref kind, _) => kind,
            PauseMicrovm(ref kind, _) => kind,
            ResumeMicrovm(ref kind, _) => kind,
            StartMicrovm(ref kind, _) => kind,
            SendCtrlAltDel(ref kind, _) => kind,
            #[cfg(feature = "vsock")]
            VsockConfig(ref kind, _) => kind,
        }
    }
}

impl Display for VmmActionError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::VmmActionError::*;

        match *self {
            BootSource(_, ref err) => write!(f, "{}", err.to_string()),
            DriveConfig(_, ref err) => write!(f, "{}", err.to_string()),
            Logger(_, ref err) => write!(f, "{}", err.to_string()),
            MachineConfig(_, ref err) => write!(f, "{}", err.to_string()),
            NetworkConfig(_, ref err) => write!(f, "{}", err.to_string()),
            PauseMicrovm(_, ref err) => write!(f, "{}", err.to_string()),
            ResumeMicrovm(_, ref err) => write!(f, "{}", err.to_string()),
            StartMicrovm(_, ref err) => write!(f, "{}", err.to_string()),
            SendCtrlAltDel(_, ref err) => write!(f, "{}", err.to_string()),
            #[cfg(feature = "vsock")]
            VsockConfig(_, ref err) => write!(f, "{}", err.to_string()),
        }
    }
}

/// This enum represents the public interface of the VMM. Each action contains various
/// bits of information (ids, paths, etc.), together with an OutcomeSender, which is always present.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum VmmAction {
    /// Configure the boot source of the microVM using as input the `ConfigureBootSource`. This
    /// action can only be called before the microVM has booted. The response is sent using the
    /// `OutcomeSender`.
    ConfigureBootSource(BootSourceConfig, OutcomeSender),
    /// Configure the logger using as input the `LoggerConfig`. This action can only be called
    /// before the microVM has booted. The response is sent using the `OutcomeSender`.
    ConfigureLogger(LoggerConfig, OutcomeSender),
    /// Get the configuration of the microVM. The action response is sent using the `OutcomeSender`.
    GetVmConfiguration(OutcomeSender),
    /// Flush the metrics. This action can only be called after the logger has been configured.
    /// The response is sent using the `OutcomeSender`.
    FlushMetrics(OutcomeSender),
    /// Add a new block device or update one that already exists using the `BlockDeviceConfig` as
    /// input. This action can only be called before the microVM has booted. The response
    /// is sent using the `OutcomeSender`.
    InsertBlockDevice(BlockDeviceConfig, OutcomeSender),
    /// Add a new network interface config or update one that already exists using the
    /// `NetworkInterfaceConfig` as input. This action can only be called before the microVM has
    /// booted. The response is sent using the `OutcomeSender`.
    InsertNetworkDevice(NetworkInterfaceConfig, OutcomeSender),
    #[cfg(feature = "vsock")]
    /// Add a new vsock device or update one that already exists using the
    /// `VsockDeviceConfig` as input. This action can only be called before the microVM has
    /// booted. The response is sent using the `OutcomeSender`.
    InsertVsockDevice(VsockDeviceConfig, OutcomeSender),
    /// Pause the microVM, save its state to the snapshot file and end this Firecracker process.
    #[cfg(target_arch = "x86_64")]
    PauseToSnapshot(OutcomeSender),
    /// Pause the microVM VCPUs, effectively pausing the guest.
    PauseVCPUs(OutcomeSender),
    /// Update the size of an existing block device specified by an ID. The ID is the first data
    /// associated with this enum variant. This action can only be called after the microVM is
    /// started. The response is sent using the `OutcomeSender`.
    RescanBlockDevice(String, OutcomeSender),
    /// Load the microVM state from the snapshot file and resume its operation.
    #[cfg(target_arch = "x86_64")]
    ResumeFromSnapshot(String, OutcomeSender),
    /// Resume the microVM VCPUs, thus resuming a paused guest.
    ResumeVCPUs(OutcomeSender),
    /// Set the microVM configuration (memory & vcpu) using `VmConfig` as input. This
    /// action can only be called before the microVM has booted. The action
    /// response is sent using the `OutcomeSender`.
    SetVmConfiguration(VmConfig, OutcomeSender),
    /// Launch the microVM. This action can only be called before the microVM has booted.
    /// The first argument represents an optional file path for the snapshot. If `Some`, the
    /// microVM will be snapshottable, and the snapshot will be placed at the specified location.
    /// If `None`, the microVM will not be snapshottable.
    /// The response is sent using the `OutcomeSender`.
    StartMicroVm(Option<String>, OutcomeSender),
    /// Send CTRL+ALT+DEL to the microVM, using the i8042 keyboard function. If an AT-keyboard
    /// driver is listening on the guest end, this can be used to shut down the microVM gracefully.
    SendCtrlAltDel(OutcomeSender),
    /// Update the path of an existing block device. The data associated with this variant
    /// represents the `drive_id` and the `path_on_host`. The response is sent using
    /// the `OutcomeSender`.
    UpdateBlockDevicePath(String, String, OutcomeSender),
    /// Update a network interface, after microVM start. Currently, the only updatable properties
    /// are the RX and TX rate limiters.
    UpdateNetworkInterface(NetworkInterfaceUpdateConfig, OutcomeSender),
}

/// The enum represents the response sent by the VMM in case of success. The response is either
/// empty, when no data needs to be sent, or an internal VMM structure.
#[derive(Debug)]
pub enum VmmData {
    /// No data is sent on the channel.
    Empty,
    /// The microVM configuration represented by `VmConfig`.
    MachineConfiguration(VmConfig),
}

/// Data type used to communicate between the API and the VMM.
pub type VmmRequestOutcome = std::result::Result<VmmData, VmmActionError>;
/// One shot channel used to send a request.
pub type OutcomeSender = oneshot::Sender<VmmRequestOutcome>;
/// One shot channel used to receive a response.
pub type OutcomeReceiver = oneshot::Receiver<VmmRequestOutcome>;

type Result<T> = std::result::Result<T, Error>;

/// Holds a micro-second resolution timestamp with both the real time and cpu time.
#[derive(Clone, Default)]
pub struct TimestampUs {
    /// Real time in microseconds.
    pub time_us: u64,
    /// Cpu time in microseconds.
    pub cputime_us: u64,
}

#[inline]
/// Gets the wallclock timestamp as microseconds.
fn get_time_us() -> u64 {
    (chrono::Utc::now().timestamp_nanos() / 1000) as u64
}

/// Describes a KVM context that gets attached to the micro vm instance.
/// It gives access to the functionality of the KVM wrapper as long as every required
/// KVM capability is present on the host.
pub struct KvmContext {
    kvm: Kvm,
    max_memslots: usize,
}

impl KvmContext {
    fn new() -> Result<Self> {
        fn check_cap(kvm: &Kvm, cap: Cap) -> std::result::Result<(), Error> {
            if !kvm.check_extension(cap) {
                return Err(Error::KvmCap(cap));
            }
            Ok(())
        }

        let kvm = Kvm::new().map_err(Error::Kvm)?;

        if kvm.get_api_version() != kvm::KVM_API_VERSION as i32 {
            return Err(Error::KvmApiVersion(kvm.get_api_version()));
        }

        check_cap(&kvm, Cap::Irqchip)?;
        check_cap(&kvm, Cap::Ioeventfd)?;
        check_cap(&kvm, Cap::Irqfd)?;
        check_cap(&kvm, Cap::ImmediateExit)?;
        #[cfg(target_arch = "x86_64")]
        check_cap(&kvm, Cap::SetTssAddr)?;
        check_cap(&kvm, Cap::UserMemory)?;
        check_cap(&kvm, Cap::MsrFeatures)?;
        #[cfg(target_arch = "x86_64")]
        check_cap(&kvm, Cap::VcpuEvents)?;
        #[cfg(target_arch = "x86_64")]
        check_cap(&kvm, Cap::Debugregs)?;
        #[cfg(target_arch = "x86_64")]
        check_cap(&kvm, Cap::Xsave)?;
        #[cfg(target_arch = "x86_64")]
        check_cap(&kvm, Cap::Xcrs)?;

        #[cfg(target_arch = "aarch64")]
        check_cap(&kvm, Cap::ArmPsci02)?;

        let max_memslots = kvm.get_nr_memslots();
        Ok(KvmContext { kvm, max_memslots })
    }

    fn fd(&self) -> &Kvm {
        &self.kvm
    }

    /// Get the maximum number of memory slots reported by this KVM context.
    pub fn max_memslots(&self) -> usize {
        self.max_memslots
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum EpollDispatch {
    Exit,
    Stdin,
    DeviceHandler(usize, DeviceEventT),
    VmmActionRequest,
    WriteMetrics,
}

struct MaybeHandler {
    handler: Option<Box<EpollHandler>>,
    receiver: Receiver<Box<EpollHandler>>,
}

impl MaybeHandler {
    fn new(receiver: Receiver<Box<EpollHandler>>) -> Self {
        MaybeHandler {
            handler: None,
            receiver,
        }
    }
}

struct EpollEvent<T: AsRawFd> {
    fd: T,
}

// Handles epoll related business.
// A glaring shortcoming of the current design is the liberal passing around of raw_fds,
// and duping of file descriptors. This issue will be solved when we also implement device removal.
struct EpollContext {
    epoll_raw_fd: RawFd,
    stdin_index: u64,
    // FIXME: find a different design as this does not scale. This Vec can only grow.
    dispatch_table: Vec<Option<EpollDispatch>>,
    device_handlers: Vec<MaybeHandler>,
    device_id_to_handler_id: HashMap<(u32, String), usize>,
}

impl EpollContext {
    fn new() -> Result<Self> {
        let epoll_raw_fd = epoll::create(true).map_err(Error::EpollFd)?;

        // Initial capacity needs to be large enough to hold:
        // * 1 exit event
        // * 1 stdin event
        // * 2 queue events for virtio block
        // * 4 for virtio net
        // The total is 8 elements; allowing spare capacity to avoid reallocations.
        let mut dispatch_table = Vec::with_capacity(20);
        let stdin_index = dispatch_table.len() as u64;
        dispatch_table.push(None);
        Ok(EpollContext {
            epoll_raw_fd,
            stdin_index,
            dispatch_table,
            device_handlers: Vec::with_capacity(6),
            device_id_to_handler_id: HashMap::new(),
        })
    }

    fn enable_stdin_event(&mut self) -> Result<()> {
        if let Err(e) = epoll::ctl(
            self.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            libc::STDIN_FILENO,
            epoll::Event::new(epoll::Events::EPOLLIN, self.stdin_index),
        ) {
            // TODO: We just log this message, and immediately return Ok, instead of returning the
            // actual error because this operation always fails with EPERM when adding a fd which
            // has been redirected to /dev/null via dup2 (this may happen inside the jailer).
            // Find a better solution to this (and think about the state of the serial device
            // while we're at it). This also led to commenting out parts of the
            // enable_disable_stdin_test() unit test function.
            warn!("Could not add stdin event to epoll. {:?}", e);
            return Ok(());
        }

        self.dispatch_table[self.stdin_index as usize] = Some(EpollDispatch::Stdin);

        Ok(())
    }

    fn disable_stdin_event(&mut self) -> Result<()> {
        // Ignore failure to remove from epoll. The only reason for failure is
        // that stdin has closed or changed in which case we won't get
        // any more events on the original event_fd anyway.
        let _ = epoll::ctl(
            self.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_DEL,
            libc::STDIN_FILENO,
            epoll::Event::new(epoll::Events::EPOLLIN, self.stdin_index),
        )
        .map_err(Error::EpollFd);
        self.dispatch_table[self.stdin_index as usize] = None;

        Ok(())
    }

    fn add_event<T>(&mut self, fd: T, token: EpollDispatch) -> Result<EpollEvent<T>>
    where
        T: AsRawFd,
    {
        let dispatch_index = self.dispatch_table.len() as u64;
        epoll::ctl(
            self.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, dispatch_index),
        )
        .map_err(Error::EpollFd)?;
        self.dispatch_table.push(Some(token));

        Ok(EpollEvent { fd })
    }

    fn allocate_tokens(&mut self, count: usize) -> (u64, Sender<Box<EpollHandler>>) {
        let dispatch_base = self.dispatch_table.len() as u64;
        let device_idx = self.device_handlers.len();
        let (sender, receiver) = channel();

        for x in 0..count {
            self.dispatch_table.push(Some(EpollDispatch::DeviceHandler(
                device_idx,
                x as DeviceEventT,
            )));
        }

        self.device_handlers.push(MaybeHandler::new(receiver));

        (dispatch_base, sender)
    }

    fn allocate_virtio_tokens<T: EpollConfigConstructor>(
        &mut self,
        type_id: u32,
        device_id: &str,
        count: usize,
    ) -> T {
        let (dispatch_base, sender) = self.allocate_tokens(count);
        self.device_id_to_handler_id.insert(
            (type_id, device_id.to_string()),
            self.device_handlers.len() - 1,
        );
        T::new(dispatch_base, self.epoll_raw_fd, sender)
    }

    fn get_device_handler_by_handler_id(&mut self, id: usize) -> Result<&mut EpollHandler> {
        let maybe = &mut self.device_handlers[id];
        match maybe.handler {
            Some(ref mut v) => Ok(v.as_mut()),
            None => {
                // This should only be called in response to an epoll trigger.
                // Moreover, this branch of the match should only be active on the first call
                // (the first epoll event for this device), therefore the channel is guaranteed
                // to contain a message for the first epoll event since both epoll event
                // registration and channel send() happen in the device activate() function.
                let received = maybe
                    .receiver
                    .try_recv()
                    .map_err(|_| Error::DeviceEventHandlerNotFound)?;
                Ok(maybe.handler.get_or_insert(received).as_mut())
            }
        }
    }

    fn get_generic_device_handler_by_device_id(
        &mut self,
        type_id: u32,
        device_id: &str,
    ) -> Result<&mut dyn EpollHandler> {
        let handler_id = *self
            .device_id_to_handler_id
            .get(&(type_id, device_id.to_string()))
            .ok_or(Error::DeviceEventHandlerNotFound)?;
        let device_handler = self.get_device_handler_by_handler_id(handler_id)?;
        Ok(&mut *device_handler)
    }

    fn get_device_handler_by_device_id<T: EpollHandler + 'static>(
        &mut self,
        type_id: u32,
        device_id: &str,
    ) -> Result<&mut T> {
        let device_handler = self.get_generic_device_handler_by_device_id(type_id, device_id)?;
        match device_handler.as_mut_any().downcast_mut::<T>() {
            Some(res) => Ok(res),
            None => Err(Error::DeviceEventHandlerInvalidDowncast),
        }
    }
}

impl Drop for EpollContext {
    fn drop(&mut self) {
        let rc = unsafe { libc::close(self.epoll_raw_fd) };
        if rc != 0 {
            warn!("Cannot close epoll.");
        }
    }
}

struct KernelConfig {
    cmdline: kernel_cmdline::Cmdline,
    kernel_file: File,
    #[cfg(target_arch = "x86_64")]
    cmdline_addr: GuestAddress,
}

struct Vmm {
    kvm: KvmContext,

    vm_config: VmConfig,
    shared_info: Arc<RwLock<InstanceInfo>>,

    // Guest VM core resources.
    guest_memory: Option<GuestMemory>,
    kernel_config: Option<KernelConfig>,
    vcpus_handles: Vec<VcpuHandle>,
    exit_evt: Option<EpollEvent<EventFd>>,
    vm: Vm,

    // Guest VM devices.
    mmio_device_manager: Option<MMIODeviceManager>,
    legacy_device_manager: LegacyDeviceManager,

    // Device configurations.
    // If there is a Root Block Device, this should be added as the first element of the list.
    // This is necessary because we want the root to always be mounted on /dev/vda.
    block_device_configs: BlockDeviceConfigs,
    network_interface_configs: NetworkInterfaceConfigs,
    #[cfg(feature = "vsock")]
    vsock_device_configs: VsockDeviceConfigs,

    epoll_context: EpollContext,

    // API resources.
    api_event: EpollEvent<EventFd>,
    from_api: Receiver<Box<VmmAction>>,

    write_metrics_event: EpollEvent<TimerFd>,

    // The level of seccomp filtering used. Seccomp filters are loaded before executing guest code.
    seccomp_level: u32,

    #[cfg(target_arch = "x86_64")]
    snapshot_image: Option<SnapshotImage>,
}

impl Vmm {
    fn new(
        api_shared_info: Arc<RwLock<InstanceInfo>>,
        api_event_fd: EventFd,
        from_api: Receiver<Box<VmmAction>>,
        seccomp_level: u32,
    ) -> Result<Self> {
        let mut epoll_context = EpollContext::new()?;
        // If this fails, it's fatal; using expect() to crash.
        let api_event = epoll_context
            .add_event(api_event_fd, EpollDispatch::VmmActionRequest)
            .expect("Cannot add API eventfd to epoll.");

        let write_metrics_event = epoll_context
            .add_event(
                // non-blocking & close on exec
                TimerFd::new_custom(ClockId::Monotonic, true, true).map_err(Error::TimerFd)?,
                EpollDispatch::WriteMetrics,
            )
            .expect("Cannot add write metrics TimerFd to epoll.");

        let block_device_configs = BlockDeviceConfigs::new();
        let kvm = KvmContext::new()?;
        let vm = Vm::new(kvm.fd()).map_err(Error::Vm)?;

        Ok(Vmm {
            kvm,
            vm_config: VmConfig::default(),
            shared_info: api_shared_info,
            guest_memory: None,
            kernel_config: None,
            vcpus_handles: vec![],
            exit_evt: None,
            vm,
            mmio_device_manager: None,
            legacy_device_manager: LegacyDeviceManager::new().map_err(Error::CreateLegacyDevice)?,
            block_device_configs,
            network_interface_configs: NetworkInterfaceConfigs::new(),
            #[cfg(feature = "vsock")]
            vsock_device_configs: VsockDeviceConfigs::new(),
            epoll_context,
            api_event,
            from_api,
            write_metrics_event,
            seccomp_level,

            #[cfg(target_arch = "x86_64")]
            snapshot_image: None,
        })
    }

    fn update_drive_handler(
        &mut self,
        drive_id: &str,
        disk_image: File,
    ) -> result::Result<(), DriveError> {
        let handler = self
            .epoll_context
            .get_device_handler_by_device_id::<virtio::BlockEpollHandler>(TYPE_BLOCK, drive_id)
            .map_err(|_| DriveError::EpollHandlerNotFound)?;

        handler
            .update_disk_image(disk_image)
            .map_err(|_| DriveError::BlockDeviceUpdateFailed)
    }

    // Attaches all block devices from the BlockDevicesConfig.
    fn attach_block_devices(&mut self) -> std::result::Result<(), StartMicrovmError> {
        // We rely on check_health function for making sure kernel_config is not None.
        let kernel_config = self
            .kernel_config
            .as_mut()
            .ok_or(StartMicrovmError::MissingKernelConfig)?;

        if self.block_device_configs.has_root_block_device() {
            // If no PARTUUID was specified for the root device, try with the /dev/vda.
            if !self.block_device_configs.has_partuuid_root() {
                kernel_config
                    .cmdline
                    .insert_str("root=/dev/vda")
                    .map_err(|e| StartMicrovmError::KernelCmdline(e.to_string()))?;

                if self.block_device_configs.has_read_only_root() {
                    kernel_config
                        .cmdline
                        .insert_str("ro")
                        .map_err(|e| StartMicrovmError::KernelCmdline(e.to_string()))?;
                } else {
                    kernel_config
                        .cmdline
                        .insert_str("rw")
                        .map_err(|e| StartMicrovmError::KernelCmdline(e.to_string()))?;
                }
            }
        }

        let epoll_context = &mut self.epoll_context;
        // `unwrap` is suitable for this context since this should be called only after the
        // device manager has been initialized.
        let device_manager = self.mmio_device_manager.as_mut().unwrap();

        for drive_config in self.block_device_configs.config_list.iter_mut() {
            // Add the block device from file.
            let block_file = OpenOptions::new()
                .read(true)
                .write(!drive_config.is_read_only)
                .open(&drive_config.path_on_host)
                .map_err(StartMicrovmError::OpenBlockDevice)?;

            if drive_config.is_root_device && drive_config.get_partuuid().is_some() {
                kernel_config
                    .cmdline
                    .insert_str(format!(
                        "root=PARTUUID={}",
                        //The unwrap is safe as we are firstly checking that partuuid is_some().
                        drive_config.get_partuuid().unwrap()
                    ))
                    .map_err(|e| StartMicrovmError::KernelCmdline(e.to_string()))?;
                if drive_config.is_read_only {
                    kernel_config
                        .cmdline
                        .insert_str("ro")
                        .map_err(|e| StartMicrovmError::KernelCmdline(e.to_string()))?;
                } else {
                    kernel_config
                        .cmdline
                        .insert_str("rw")
                        .map_err(|e| StartMicrovmError::KernelCmdline(e.to_string()))?;
                }
            }

            let epoll_config = epoll_context.allocate_virtio_tokens(
                TYPE_BLOCK,
                &drive_config.drive_id,
                BLOCK_EVENTS_COUNT,
            );
            let rate_limiter = match drive_config.rate_limiter {
                Some(rlim_cfg) => Some(
                    rlim_cfg
                        .into_rate_limiter()
                        .map_err(StartMicrovmError::CreateRateLimiter)?,
                ),
                None => None,
            };

            let block_box = Box::new(
                devices::virtio::Block::new(
                    block_file,
                    drive_config.is_read_only,
                    epoll_config,
                    rate_limiter,
                )
                .map_err(StartMicrovmError::CreateBlockDevice)?,
            );
            device_manager
                .register_virtio_device(
                    self.vm.get_fd(),
                    block_box,
                    &mut kernel_config.cmdline,
                    TYPE_BLOCK,
                    &drive_config.drive_id,
                )
                .map_err(StartMicrovmError::RegisterBlockDevice)?;
        }

        Ok(())
    }

    fn attach_net_devices(&mut self) -> std::result::Result<(), StartMicrovmError> {
        // We rely on check_health function for making sure kernel_config is not None.
        let kernel_config = self
            .kernel_config
            .as_mut()
            .ok_or(StartMicrovmError::MissingKernelConfig)?;

        // `unwrap` is suitable for this context since this should be called only after the
        // device manager has been initialized.
        let device_manager = self.mmio_device_manager.as_mut().unwrap();

        for cfg in self.network_interface_configs.iter_mut() {
            let epoll_config = self.epoll_context.allocate_virtio_tokens(
                TYPE_NET,
                &cfg.iface_id,
                NET_EVENTS_COUNT,
            );

            let allow_mmds_requests = cfg.allow_mmds_requests();
            let rx_rate_limiter = match cfg.rx_rate_limiter {
                Some(rlim) => Some(
                    rlim.into_rate_limiter()
                        .map_err(StartMicrovmError::CreateRateLimiter)?,
                ),
                None => None,
            };
            let tx_rate_limiter = match cfg.tx_rate_limiter {
                Some(rlim) => Some(
                    rlim.into_rate_limiter()
                        .map_err(StartMicrovmError::CreateRateLimiter)?,
                ),
                None => None,
            };

            if let Some(tap) = cfg.take_tap() {
                let net_box = Box::new(
                    devices::virtio::Net::new_with_tap(
                        tap,
                        cfg.guest_mac(),
                        epoll_config,
                        rx_rate_limiter,
                        tx_rate_limiter,
                        allow_mmds_requests,
                    )
                    .map_err(StartMicrovmError::CreateNetDevice)?,
                );

                device_manager
                    .register_virtio_device(
                        self.vm.get_fd(),
                        net_box,
                        &mut kernel_config.cmdline,
                        TYPE_NET,
                        &cfg.iface_id,
                    )
                    .map_err(StartMicrovmError::RegisterNetDevice)?;
            } else {
                return Err(StartMicrovmError::NetDeviceNotConfigured)?;
            }
        }
        Ok(())
    }

    #[cfg(feature = "vsock")]
    fn attach_vsock_devices(
        &mut self,
        guest_mem: &GuestMemory,
    ) -> std::result::Result<(), StartMicrovmError> {
        let kernel_config = self
            .kernel_config
            .as_mut()
            .ok_or(StartMicrovmError::MissingKernelConfig)?;
        // `unwrap` is suitable for this context since this should be called only after the
        // device manager has been initialized.
        let device_manager = self.mmio_device_manager.as_mut().unwrap();

        for cfg in self.vsock_device_configs.iter() {
            let epoll_config =
                self.epoll_context
                    .allocate_virtio_tokens(TYPE_VSOCK, &cfg.id, VHOST_EVENTS_COUNT);

            let vsock_box = Box::new(
                devices::virtio::Vsock::new(u64::from(cfg.guest_cid), guest_mem, epoll_config)
                    .map_err(StartMicrovmError::CreateVsockDevice)?,
            );
            device_manager
                .register_virtio_device(
                    self.vm.get_fd(),
                    vsock_box,
                    &mut kernel_config.cmdline,
                    TYPE_VSOCK,
                    &cfg.id,
                )
                .map_err(StartMicrovmError::RegisterVsockDevice)?;
        }
        Ok(())
    }

    fn configure_kernel(&mut self, kernel_config: KernelConfig) {
        self.kernel_config = Some(kernel_config);
    }

    fn flush_metrics(&mut self) -> VmmRequestOutcome {
        if let Err(e) = self.write_metrics() {
            if let LoggerError::NeverInitialized(s) = e {
                return Err(VmmActionError::Logger(
                    ErrorKind::User,
                    LoggerConfigError::FlushMetrics(s),
                ));
            } else {
                return Err(VmmActionError::Logger(
                    ErrorKind::Internal,
                    LoggerConfigError::FlushMetrics(e.to_string()),
                ));
            }
        }
        Ok(VmmData::Empty)
    }

    #[cfg(target_arch = "x86_64")]
    fn log_dirty_pages(&mut self) {
        // If we're logging dirty pages, post the metrics on how many dirty pages there are.
        if LOGGER.flags() | LogOption::LogDirtyPages as usize > 0 {
            METRICS.memory.dirty_pages.add(self.get_dirty_page_count());
        }
    }

    fn write_metrics(&mut self) -> result::Result<(), LoggerError> {
        // The dirty pages are only available on x86_64.
        #[cfg(target_arch = "x86_64")]
        self.log_dirty_pages();
        LOGGER.log_metrics()
    }

    fn init_guest_memory(&mut self) -> std::result::Result<(), StartMicrovmError> {
        let mem_size = self
            .vm_config
            .mem_size_mib
            .ok_or(StartMicrovmError::GuestMemory(
                memory_model::GuestMemoryError::MemoryNotInitialized,
            ))?
            << 20;
        let arch_mem_regions = arch::arch_memory_regions(mem_size);

        #[cfg(target_arch = "aarch64")]
        let guest_memory = GuestMemory::new_anon_from_tuples(&arch_mem_regions)
            .map_err(StartMicrovmError::GuestMemory)?;
        #[cfg(target_arch = "x86_64")]
        let guest_memory = match self.snapshot_image.as_ref() {
            Some(image) => {
                let mut ranges = Vec::<FileMemoryDesc>::with_capacity(arch_mem_regions.len());
                let snapshot_fd = image.as_raw_fd();
                let mut region_offset = image.memory_offset();
                let shared_mapping = image.is_shared_mapping();
                for (gpa, size) in arch_mem_regions {
                    ranges.push(FileMemoryDesc {
                        gpa,
                        size,
                        fd: snapshot_fd,
                        offset: region_offset,
                        shared: shared_mapping,
                    });
                    region_offset += size;
                }
                GuestMemory::new_file_backed(&ranges).map_err(StartMicrovmError::GuestMemory)?
            }
            None => {
                warn!("No snapshot file found, defaulting to using anonymous memory.");
                GuestMemory::new_anon_from_tuples(&arch_mem_regions)
                    .map_err(StartMicrovmError::GuestMemory)?
            }
        };

        self.guest_memory = Some(guest_memory);
        self.vm
            .memory_init(
                self.guest_memory
                    .clone()
                    .ok_or(StartMicrovmError::GuestMemory(
                        memory_model::GuestMemoryError::MemoryNotInitialized,
                    ))?,
                &self.kvm,
            )
            .map_err(StartMicrovmError::ConfigureVm)?;
        Ok(())
    }

    fn check_health(&self) -> std::result::Result<(), StartMicrovmError> {
        if self.kernel_config.is_none() {
            return Err(StartMicrovmError::MissingKernelConfig)?;
        }
        Ok(())
    }

    fn init_mmio_device_manager(&mut self) -> std::result::Result<(), StartMicrovmError> {
        if self.mmio_device_manager.is_some() {
            return Ok(());
        }

        let guest_mem = self
            .guest_memory
            .clone()
            .ok_or(StartMicrovmError::GuestMemory(
                memory_model::GuestMemoryError::MemoryNotInitialized,
            ))?;

        // Instantiate the MMIO device manager.
        // 'mmio_base' address has to be an address which is protected by the kernel
        // and is architectural specific.
        let device_manager = MMIODeviceManager::new(
            guest_mem.clone(),
            &mut (arch::get_reserved_mem_addr() as u64),
            (arch::IRQ_BASE, arch::IRQ_MAX),
        );
        self.mmio_device_manager = Some(device_manager);

        Ok(())
    }

    fn attach_virtio_devices(&mut self) -> std::result::Result<(), StartMicrovmError> {
        self.init_mmio_device_manager()?;

        self.attach_block_devices()?;
        self.attach_net_devices()?;
        #[cfg(feature = "vsock")]
        {
            let guest_mem = self
                .guest_memory
                .clone()
                .ok_or(StartMicrovmError::GuestMemory(
                    memory_model::GuestMemoryError::MemoryNotInitialized,
                ))?;
            self.attach_vsock_devices(&guest_mem)?;
        }

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn get_mmio_device_info(&self) -> Option<&HashMap<(DeviceType, String), MMIODeviceInfo>> {
        if let Some(ref device_manager) = self.mmio_device_manager {
            Some(device_manager.get_device_info())
        } else {
            None
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn setup_interrupt_controller(&mut self) -> std::result::Result<(), StartMicrovmError> {
        self.vm
            .setup_irqchip()
            .map_err(StartMicrovmError::ConfigureVm)
    }

    #[cfg(target_arch = "aarch64")]
    fn setup_interrupt_controller(&mut self) -> std::result::Result<(), StartMicrovmError> {
        let vcpu_count = self
            .vm_config
            .vcpu_count
            .ok_or(StartMicrovmError::VcpusNotConfigured)?;
        self.vm
            .setup_irqchip(vcpu_count)
            .map_err(StartMicrovmError::ConfigureVm)
    }

    #[cfg(target_arch = "x86_64")]
    fn attach_legacy_devices(&mut self) -> std::result::Result<(), StartMicrovmError> {
        self.legacy_device_manager
            .register_devices()
            .map_err(StartMicrovmError::LegacyIOBus)?;

        self.vm
            .get_fd()
            .register_irqfd(&self.legacy_device_manager.com_evt_1_3, 4)
            .map_err(|e| {
                StartMicrovmError::LegacyIOBus(device_manager::legacy::Error::EventFd(e))
            })?;
        self.vm
            .get_fd()
            .register_irqfd(&self.legacy_device_manager.com_evt_2_4, 3)
            .map_err(|e| {
                StartMicrovmError::LegacyIOBus(device_manager::legacy::Error::EventFd(e))
            })?;
        self.vm
            .get_fd()
            .register_irqfd(&self.legacy_device_manager.kbd_evt, 1)
            .map_err(|e| StartMicrovmError::LegacyIOBus(device_manager::legacy::Error::EventFd(e)))
    }

    #[cfg(target_arch = "aarch64")]
    fn attach_legacy_devices(&mut self) -> std::result::Result<(), StartMicrovmError> {
        self.init_mmio_device_manager()?;
        // `unwrap` is suitable for this context since this should be called only after the
        // device manager has been initialized.
        let device_manager = self.mmio_device_manager.as_mut().unwrap();

        // We rely on check_health function for making sure kernel_config is not None.
        let kernel_config = self
            .kernel_config
            .as_mut()
            .ok_or(StartMicrovmError::MissingKernelConfig)?;

        if kernel_config.cmdline.as_str().contains("console=") {
            device_manager
                .register_mmio_serial(self.vm.get_fd(), &mut kernel_config.cmdline)
                .map_err(StartMicrovmError::RegisterMMIODevice)?;
        }
        device_manager
            .register_mmio_rtc(self.vm.get_fd())
            .map_err(StartMicrovmError::RegisterMMIODevice)?;
        Ok(())
    }

    // On aarch64, the vCPUs need to be created (i.e call KVM_CREATE_VCPU) and configured before
    // setting up the IRQ chip because the `KVM_CREATE_VCPU` ioctl will return error if the IRQCHIP
    // was already initialized.
    // Search for `kvm_arch_vcpu_create` in arch/arm/kvm/arm.c.
    fn create_vcpus(
        &mut self,
        request_ts: TimestampUs,
    ) -> std::result::Result<(), StartMicrovmError> {
        let vcpu_count = self
            .vm_config
            .vcpu_count
            .ok_or(StartMicrovmError::VcpusNotConfigured)?;

        if !self.vcpus_handles.is_empty() {
            Err(StartMicrovmError::VcpusAlreadyPresent)?;
        }

        self.vcpus_handles.reserve(vcpu_count as usize);

        for cpu_id in 0..vcpu_count {
            let io_bus = self.legacy_device_manager.io_bus.clone();

            // If the lock is poisoned, it's OK to panic.
            let vcpu_exit_evt = self
                .legacy_device_manager
                .i8042
                .lock()
                .expect("Failed to start VCPUs due to poisoned i8042 lock")
                .get_reset_evt_clone()
                .map_err(|_| StartMicrovmError::EventFd)?;

            let vcpu_handle =
                VcpuHandle::new(cpu_id, &self.vm, io_bus, vcpu_exit_evt, request_ts.clone())
                    .map_err(StartMicrovmError::Vcpu)?;

            self.vcpus_handles.push(vcpu_handle);
        }
        Ok(())
    }

    fn configure_vcpus_for_boot(
        &mut self,
        entry_addr: GuestAddress,
    ) -> std::result::Result<(), StartMicrovmError> {
        for handle in self.vcpus_handles.iter_mut() {
            handle
                .configure_vcpu(&self.vm_config, entry_addr, &self.vm)
                .map_err(StartMicrovmError::VcpuConfigure)?;
        }
        Ok(())
    }

    /// Creates vcpu threads and runs the vcpu main loop which starts off 'Paused'.
    fn start_vcpus(&mut self) -> std::result::Result<(), StartMicrovmError> {
        Vcpu::register_vcpu_kick_signal_handler();
        for handle in self.vcpus_handles.iter_mut() {
            handle
                .start_vcpu(
                    self.seccomp_level,
                    self.mmio_device_manager
                        .as_ref()
                        .map(|devmgr| devmgr.bus.clone()),
                )
                .map_err(StartMicrovmError::VcpuSpawn)?
        }
        Ok(())
    }

    fn load_kernel(&mut self) -> std::result::Result<GuestAddress, StartMicrovmError> {
        // This is the easy way out of consuming the value of the kernel_cmdline.
        let kernel_config = self
            .kernel_config
            .as_mut()
            .ok_or(StartMicrovmError::MissingKernelConfig)?;

        let vm_memory = self.vm.get_memory().ok_or(StartMicrovmError::GuestMemory(
            memory_model::GuestMemoryError::MemoryNotInitialized,
        ))?;
        let entry_addr = kernel_loader::load_kernel(
            vm_memory,
            &mut kernel_config.kernel_file,
            arch::get_kernel_start(),
        )
        .map_err(StartMicrovmError::KernelLoader)?;

        // This is x86_64 specific since on aarch64 the commandline will be specified through the FDT.
        #[cfg(target_arch = "x86_64")]
        kernel_loader::load_cmdline(
            vm_memory,
            kernel_config.cmdline_addr,
            &kernel_config
                .cmdline
                .as_cstring()
                .map_err(StartMicrovmError::LoadCommandline)?,
        )
        .map_err(StartMicrovmError::LoadCommandline)?;

        Ok(entry_addr)
    }

    fn configure_system(&self) -> std::result::Result<(), StartMicrovmError> {
        let kernel_config = self
            .kernel_config
            .as_ref()
            .ok_or(StartMicrovmError::MissingKernelConfig)?;

        let vm_memory = self.vm.get_memory().ok_or(StartMicrovmError::GuestMemory(
            memory_model::GuestMemoryError::MemoryNotInitialized,
        ))?;
        // The vcpu_count has a default value. We shouldn't have gotten to this point without
        // having set the vcpu count.
        let vcpu_count = self
            .vm_config
            .vcpu_count
            .ok_or(StartMicrovmError::VcpusNotConfigured)?;
        #[cfg(target_arch = "x86_64")]
        arch::x86_64::configure_system(
            vm_memory,
            kernel_config.cmdline_addr,
            kernel_config.cmdline.len() + 1,
            vcpu_count,
        )
        .map_err(StartMicrovmError::ConfigureSystem)?;

        #[cfg(target_arch = "aarch64")]
        {
            arch::aarch64::configure_system(
                vm_memory,
                &kernel_config
                    .cmdline
                    .as_cstring()
                    .map_err(StartMicrovmError::LoadCommandline)?,
                vcpu_count,
                self.get_mmio_device_info(),
            )
            .map_err(StartMicrovmError::ConfigureSystem)?;
        }
        Ok(())
    }

    fn register_events(&mut self) -> std::result::Result<(), StartMicrovmError> {
        // If the lock is poisoned, it's OK to panic.
        let event_fd = self
            .legacy_device_manager
            .i8042
            .lock()
            .expect("Failed to register events on the event fd due to poisoned lock")
            .get_reset_evt_clone()
            .map_err(|_| StartMicrovmError::EventFd)?;
        let exit_epoll_evt = self
            .epoll_context
            .add_event(event_fd, EpollDispatch::Exit)
            .map_err(|_| StartMicrovmError::RegisterEvent)?;
        self.exit_evt = Some(exit_epoll_evt);

        self.epoll_context
            .enable_stdin_event()
            .map_err(|_| StartMicrovmError::RegisterEvent)?;

        Ok(())
    }

    // Creates the snapshot file that will later be populated.
    #[cfg(target_arch = "x86_64")]
    fn create_snapshot_file(
        &mut self,
        snapshot_path: String,
    ) -> std::result::Result<(), StartMicrovmError> {
        let nmsrs = self.vm.supported_msrs().as_original_struct().nmsrs;
        let ncpuids = self.vm.supported_cpuid().as_original_struct().nent;
        let image: SnapshotImage =
            SnapshotImage::create_new(snapshot_path, self.vm_config.clone(), nmsrs, ncpuids)
                .map_err(StartMicrovmError::SnapshotBackingFile)?;
        self.snapshot_image = Some(image);
        Ok(())
    }

    fn start_microvm(&mut self, snapshot_path: Option<String>) -> VmmRequestOutcome {
        info!("VMM received instance start command");
        if self.is_instance_initialized() {
            Err(StartMicrovmError::from(StateError::MicroVMAlreadyRunning))?;
        }
        let request_ts = TimestampUs {
            time_us: get_time_us(),
            cputime_us: now_cputime_us(),
        };

        self.check_health()?;
        // Use expect() to crash if the other thread poisoned this lock.
        self.shared_info
            .write()
            .expect("Failed to start microVM because shared info couldn't be written due to poisoned lock")
            .state = InstanceState::Starting;

        #[cfg(target_arch = "x86_64")]
        {
            if let Some(snap_path) = snapshot_path {
                self.create_snapshot_file(snap_path)?;
            }
        }

        self.init_guest_memory()?;

        // For x86_64 we need to create the interrupt controller before calling `KVM_CREATE_VCPUS`
        // while on aarch64 we need to do it the other way around.
        #[cfg(target_arch = "x86_64")]
        {
            self.setup_interrupt_controller()?;
            self.attach_virtio_devices()?;
            self.attach_legacy_devices()?;

            let entry_addr = self.load_kernel()?;
            self.create_vcpus(request_ts)?;
            self.configure_vcpus_for_boot(entry_addr)?;
        }

        #[cfg(target_arch = "aarch64")]
        {
            let entry_addr = self.load_kernel()?;
            self.create_vcpus(request_ts)?;
            self.configure_vcpus_for_boot(entry_addr)?;

            self.setup_interrupt_controller()?;
            self.attach_virtio_devices()?;
            self.attach_legacy_devices()?;
        }

        self.configure_system()?;

        self.register_events()?;

        // Will create vcpu threads and run their main loop. Initial vcpu state is 'Paused'.
        self.start_vcpus()?;

        // Load seccomp filters for the VMM thread.
        // Execution panics if filters cannot be loaded, use --seccomp-level=0 if skipping filters
        // altogether is the desired behaviour.
        default_syscalls::set_seccomp_level(self.seccomp_level)
            .map_err(StartMicrovmError::SeccompFilters)?;

        // Send the 'resume' command so that vcpus actually start running.
        self.resume_vcpus()?;

        // Use expect() to crash if the other thread poisoned this lock.
        self.shared_info
            .write()
            .expect("Failed to start microVM because shared info couldn't be written due to poisoned lock")
            .state = InstanceState::Running;

        // Arm the log write timer.
        // TODO: the timer does not stop on InstanceStop.
        let timer_state = TimerState::Periodic {
            current: Duration::from_secs(WRITE_METRICS_PERIOD_SECONDS),
            interval: Duration::from_secs(WRITE_METRICS_PERIOD_SECONDS),
        };
        self.write_metrics_event
            .fd
            .set_state(timer_state, SetTimeFlags::Default);

        // Log the metrics straight away to check the process startup time.
        if LOGGER.log_metrics().is_err() {
            METRICS.logger.missed_metrics_count.inc();
        }

        Ok(VmmData::Empty)
    }

    fn send_ctrl_alt_del(&mut self) -> VmmRequestOutcome {
        self.legacy_device_manager
            .i8042
            .lock()
            .expect("i8042 lock was poisoned")
            .trigger_ctrl_alt_del()
            .map_err(|e| VmmActionError::SendCtrlAltDel(ErrorKind::Internal, e))?;
        Ok(VmmData::Empty)
    }

    /// Waits for all vCPUs to exit and terminates the Firecracker process.
    fn stop(&mut self, exit_code: i32) {
        info!("Vmm is stopping.");

        if let Err(e) = self.epoll_context.disable_stdin_event() {
            warn!("Cannot disable the STDIN event. {:?}", e);
        }

        if let Err(e) = self
            .legacy_device_manager
            .stdin_handle
            .lock()
            .set_canon_mode()
        {
            warn!("Cannot set canonical mode for the terminal. {:?}", e);
        }

        // Log the metrics before exiting.
        if let Err(e) = LOGGER.log_metrics() {
            error!("Failed to log metrics while stopping: {}", e);
        }

        // Exit from Firecracker using the provided exit code. Safe because we're terminating
        // the process anyway.
        unsafe {
            libc::_exit(exit_code);
        }
    }

    fn instance_state(&self) -> InstanceState {
        // Use expect() to crash if the other thread poisoned this lock.
        let shared_info = self.shared_info.read().expect(
            "Failed to determine if instance is initialized because \
             shared info couldn't be read due to poisoned lock",
        );
        shared_info.state.clone()
    }

    fn is_instance_initialized(&self) -> bool {
        match self.instance_state() {
            InstanceState::Uninitialized => false,
            _ => true,
        }
    }

    #[allow(dead_code)]
    fn is_instance_running(&self) -> bool {
        match self.instance_state() {
            InstanceState::Running => true,
            _ => false,
        }
    }

    #[allow(clippy::unused_label)]
    fn run_control(&mut self) -> Result<()> {
        const EPOLL_EVENTS_LEN: usize = 100;

        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        let epoll_raw_fd = self.epoll_context.epoll_raw_fd;

        // TODO: try handling of errors/failures without breaking this main loop.
        'poll: loop {
            let num_events = epoll::wait(epoll_raw_fd, -1, &mut events[..]).map_err(Error::Poll)?;

            for event in events.iter().take(num_events) {
                let dispatch_idx = event.data as usize;

                if let Some(dispatch_type) = self.epoll_context.dispatch_table[dispatch_idx] {
                    match dispatch_type {
                        EpollDispatch::Exit => {
                            match self.exit_evt {
                                Some(ref ev) => {
                                    ev.fd.read().map_err(Error::EventFd)?;
                                }
                                None => warn!("leftover exit-evt in epollcontext!"),
                            }
                            thread::sleep(Duration::from_millis(100));
                            self.stop(i32::from(FC_EXIT_CODE_OK));
                        }
                        EpollDispatch::Stdin => {
                            let mut out = [0u8; 64];
                            let stdin_lock = self.legacy_device_manager.stdin_handle.lock();
                            match stdin_lock.read_raw(&mut out[..]) {
                                Ok(0) => {
                                    // Zero-length read indicates EOF. Remove from pollables.
                                    self.epoll_context.disable_stdin_event()?;
                                }
                                Err(e) => {
                                    error!("error while reading stdin: {}", e);
                                    self.epoll_context.disable_stdin_event()?;
                                }
                                Ok(count) => {
                                    // Use expect() to panic if another thread panicked
                                    // while holding the lock.
                                    self.legacy_device_manager
                                        .stdio_serial
                                        .lock()
                                        .expect(
                                            "Failed to process stdin event due to poisoned lock",
                                        )
                                        .queue_input_bytes(&out[..count])
                                        .map_err(Error::Serial)?;
                                }
                            }
                        }
                        EpollDispatch::DeviceHandler(device_idx, device_token) => {
                            METRICS.vmm.device_events.inc();
                            match self
                                .epoll_context
                                .get_device_handler_by_handler_id(device_idx)
                            {
                                Ok(handler) => match handler.handle_event(device_token) {
                                    Err(devices::Error::PayloadExpected) => panic!(
                                        "Received update disk image event with empty payload."
                                    ),
                                    Err(devices::Error::UnknownEvent { device, event }) => {
                                        panic!("Unknown event: {:?} {:?}", device, event)
                                    }
                                    _ => (),
                                },
                                Err(e) => {
                                    warn!("invalid handler for device {}: {:?}", device_idx, e)
                                }
                            }
                        }
                        EpollDispatch::VmmActionRequest => {
                            self.api_event.fd.read().map_err(Error::EventFd)?;
                            self.run_vmm_action().unwrap_or_else(|_| {
                                warn!("got spurious notification from api thread");
                            });
                        }
                        EpollDispatch::WriteMetrics => {
                            self.write_metrics_event.fd.read();
                            // Please note that, since LOGGER has no output file configured yet, it will write to
                            // stdout, so logging will interfere with console output.
                            if let Err(e) = self.write_metrics() {
                                error!("Failed to log metrics: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    // Count the number of pages dirtied since the last call to this function.
    // Because this is used for metrics, it swallows most errors and simply doesn't count dirty
    // pages if the KVM operation fails.
    #[cfg(target_arch = "x86_64")]
    fn get_dirty_page_count(&mut self) -> usize {
        if let Some(ref mem) = self.guest_memory {
            let dirty_pages = mem.map_and_fold(
                0,
                |(slot, memory_region)| {
                    let bitmap = self
                        .vm
                        .get_fd()
                        .get_dirty_log(slot as u32, memory_region.size());
                    match bitmap {
                        Ok(v) => v
                            .iter()
                            .fold(0, |init, page| init + page.count_ones() as usize),
                        Err(_) => 0,
                    }
                },
                |dirty_pages, region_dirty_pages| dirty_pages + region_dirty_pages,
            );
            return dirty_pages;
        }
        0
    }

    fn configure_boot_source(
        &mut self,
        kernel_image_path: String,
        kernel_cmdline: Option<String>,
    ) -> VmmRequestOutcome {
        if self.is_instance_initialized() {
            return Err(VmmActionError::BootSource(
                ErrorKind::User,
                BootSourceConfigError::UpdateNotAllowedPostBoot,
            ));
        }

        let kernel_file = File::open(kernel_image_path).map_err(|_| {
            VmmActionError::BootSource(ErrorKind::User, BootSourceConfigError::InvalidKernelPath)
        })?;
        let mut cmdline = kernel_cmdline::Cmdline::new(arch::CMDLINE_MAX_SIZE);
        cmdline
            .insert_str(kernel_cmdline.unwrap_or_else(|| String::from(DEFAULT_KERNEL_CMDLINE)))
            .map_err(|_| {
                VmmActionError::BootSource(
                    ErrorKind::User,
                    BootSourceConfigError::InvalidKernelCommandLine,
                )
            })?;

        let kernel_config = KernelConfig {
            kernel_file,
            cmdline,
            #[cfg(target_arch = "x86_64")]
            cmdline_addr: GuestAddress(arch::x86_64::layout::CMDLINE_START),
        };
        self.configure_kernel(kernel_config);

        Ok(VmmData::Empty)
    }

    fn set_vm_configuration(&mut self, machine_config: VmConfig) -> VmmRequestOutcome {
        if self.is_instance_initialized() {
            Err(VmConfigError::UpdateNotAllowedPostBoot)?;
        }

        if let Some(vcpu_count_value) = machine_config.vcpu_count {
            // Check that the vcpu_count value is >=1.
            if vcpu_count_value == 0 {
                Err(VmConfigError::InvalidVcpuCount)?;
            }
        }

        if let Some(mem_size_mib_value) = machine_config.mem_size_mib {
            // TODO: add other memory checks
            if mem_size_mib_value == 0 {
                Err(VmConfigError::InvalidMemorySize)?;
            }
        }

        let ht_enabled = match machine_config.ht_enabled {
            Some(value) => value,
            None => self.vm_config.ht_enabled.unwrap(),
        };

        let vcpu_count_value = match machine_config.vcpu_count {
            Some(value) => value,
            None => self.vm_config.vcpu_count.unwrap(),
        };

        // If hyperthreading is enabled or is to be enabled in this call
        // only allow vcpu count to be 1 or even.
        if ht_enabled && vcpu_count_value > 1 && vcpu_count_value % 2 == 1 {
            Err(VmConfigError::InvalidVcpuCount)?;
        }

        // Update all the fields that have a new value.
        self.vm_config.vcpu_count = Some(vcpu_count_value);
        self.vm_config.ht_enabled = Some(ht_enabled);

        if machine_config.mem_size_mib.is_some() {
            self.vm_config.mem_size_mib = machine_config.mem_size_mib;
        }

        if machine_config.cpu_template.is_some() {
            self.vm_config.cpu_template = machine_config.cpu_template;
        }

        Ok(VmmData::Empty)
    }

    fn insert_net_device(&mut self, body: NetworkInterfaceConfig) -> VmmRequestOutcome {
        if self.is_instance_initialized() {
            Err(NetworkInterfaceError::UpdateNotAllowedPostBoot)?;
        }
        self.network_interface_configs
            .insert(body)
            .map(|_| VmmData::Empty)
            .map_err(|e| VmmActionError::NetworkConfig(ErrorKind::User, e))
    }

    fn update_net_device(&mut self, new_cfg: NetworkInterfaceUpdateConfig) -> VmmRequestOutcome {
        if !self.is_instance_initialized() {
            // VM not started yet, so we only need to update the device configs, not the actual
            // live device.
            let old_cfg = self
                .network_interface_configs
                .iter_mut()
                .find(|&&mut ref c| c.iface_id == new_cfg.iface_id)
                .ok_or(NetworkInterfaceError::DeviceIdNotFound)?;

            // Check if we need to update the RX rate limiter.
            if let Some(new_rlim_cfg) = new_cfg.rx_rate_limiter {
                if let Some(ref mut old_rlim_cfg) = old_cfg.rx_rate_limiter {
                    // We already have an RX rate limiter set, so we'll update it.
                    old_rlim_cfg.update(&new_rlim_cfg);
                } else {
                    // No old RX rate limiter; create one now.
                    old_cfg.rx_rate_limiter = Some(new_rlim_cfg);
                }
            }

            // Check if we need to update the TX rate limiter.
            if let Some(new_rlim_cfg) = new_cfg.tx_rate_limiter {
                if let Some(ref mut old_rlim_cfg) = old_cfg.tx_rate_limiter {
                    // We already have a TX rate limiter set, so we'll update it.
                    old_rlim_cfg.update(&new_rlim_cfg);
                } else {
                    // No old TX rate limiter; create one now.
                    old_cfg.tx_rate_limiter = Some(new_rlim_cfg);
                }
            }

            return Ok(VmmData::Empty);
        }

        // If we got to here, the VM is running. We need to update the live device.
        //

        let handler = self
            .epoll_context
            .get_device_handler_by_device_id::<virtio::NetEpollHandler>(TYPE_NET, &new_cfg.iface_id)
            .map_err(NetworkInterfaceError::EpollHandlerNotFound)?;

        handler.patch_rate_limiters(
            new_cfg
                .rx_rate_limiter
                .map(|rl| rl.bandwidth.map(|b| b.into_token_bucket()))
                .unwrap_or(None),
            new_cfg
                .rx_rate_limiter
                .map(|rl| rl.ops.map(|b| b.into_token_bucket()))
                .unwrap_or(None),
            new_cfg
                .tx_rate_limiter
                .map(|rl| rl.bandwidth.map(|b| b.into_token_bucket()))
                .unwrap_or(None),
            new_cfg
                .tx_rate_limiter
                .map(|rl| rl.ops.map(|b| b.into_token_bucket()))
                .unwrap_or(None),
        );

        Ok(VmmData::Empty)
    }

    #[cfg(feature = "vsock")]
    fn insert_vsock_device(&mut self, body: VsockDeviceConfig) -> VmmRequestOutcome {
        if self.is_instance_initialized() {
            return Err(VmmActionError::VsockConfig(
                ErrorKind::User,
                VsockError::UpdateNotAllowedPostBoot,
            ));
        }
        self.vsock_device_configs
            .add(body)
            .map(|_| VmmData::Empty)
            .map_err(|e| VmmActionError::VsockConfig(ErrorKind::User, e))
    }

    fn set_block_device_path(
        &mut self,
        drive_id: String,
        path_on_host: String,
    ) -> VmmRequestOutcome {
        // Get the block device configuration specified by drive_id.
        let block_device_index = self
            .block_device_configs
            .get_index_of_drive_id(&drive_id)
            .ok_or(DriveError::InvalidBlockDeviceID)?;

        let file_path = PathBuf::from(path_on_host);
        // Try to open the file specified by path_on_host using the permissions of the block_device.
        let disk_file = OpenOptions::new()
            .read(true)
            .write(!self.block_device_configs.config_list[block_device_index].is_read_only())
            .open(&file_path)
            .map_err(|_| DriveError::CannotOpenBlockDevice)?;

        // Update the path of the block device with the specified path_on_host.
        self.block_device_configs.config_list[block_device_index].path_on_host = file_path;

        // When the microvm is running, we also need to update the drive handler and send a
        // rescan command to the drive.
        if self.is_instance_initialized() {
            self.update_drive_handler(&drive_id, disk_file)?;
            self.rescan_block_device(&drive_id)?;
        }
        Ok(VmmData::Empty)
    }

    fn rescan_block_device(&mut self, drive_id: &str) -> VmmRequestOutcome {
        // Rescan can only happen after the guest is booted.
        if !self.is_instance_initialized() {
            Err(DriveError::OperationNotAllowedPreBoot)?;
        }

        // Safe to unwrap() because mmio_device_manager is initialized in init_devices(), which is
        // called before the guest boots, and this function is called after boot.
        let device_manager = self.mmio_device_manager.as_ref().unwrap();
        for drive_config in self.block_device_configs.config_list.iter() {
            if drive_config.drive_id == *drive_id {
                let metadata = metadata(&drive_config.path_on_host)
                    .map_err(|_| DriveError::BlockDeviceUpdateFailed)?;
                let new_size = metadata.len();
                if new_size % virtio::block::SECTOR_SIZE != 0 {
                    warn!(
                        "Disk size {} is not a multiple of sector size {}; \
                         the remainder will not be visible to the guest.",
                        new_size,
                        virtio::block::SECTOR_SIZE
                    );
                }
                return device_manager
                    .update_drive(drive_id, new_size)
                    .map(|_| VmmData::Empty)
                    .map_err(|_| VmmActionError::from(DriveError::BlockDeviceUpdateFailed));
            }
        }
        Err(VmmActionError::from(DriveError::InvalidBlockDeviceID))
    }

    // Only call this function as part of the API.
    // If the drive_id does not exist, a new Block Device Config is added to the list.
    fn insert_block_device(&mut self, block_device_config: BlockDeviceConfig) -> VmmRequestOutcome {
        if self.is_instance_initialized() {
            Err(DriveError::UpdateNotAllowedPostBoot)?;
        }

        self.block_device_configs
            .insert(block_device_config)
            .map(|_| VmmData::Empty)
            .map_err(VmmActionError::from)
    }

    fn init_logger(&self, api_logger: LoggerConfig) -> VmmRequestOutcome {
        if self.is_instance_initialized() {
            return Err(VmmActionError::Logger(
                ErrorKind::User,
                LoggerConfigError::InitializationFailure(
                    "Cannot initialize logger after boot.".to_string(),
                ),
            ));
        }

        let instance_id;
        let firecracker_version;
        {
            let guard = self.shared_info.read().unwrap();
            instance_id = guard.id.clone();
            firecracker_version = guard.vmm_version.clone();
        }

        match api_logger.level {
            LoggerLevel::Error => LOGGER.set_level(Level::Error),
            LoggerLevel::Warning => LOGGER.set_level(Level::Warn),
            LoggerLevel::Info => LOGGER.set_level(Level::Info),
            LoggerLevel::Debug => LOGGER.set_level(Level::Debug),
        }

        LOGGER.set_include_origin(api_logger.show_log_origin, api_logger.show_log_origin);
        LOGGER.set_include_level(api_logger.show_level);

        #[cfg(target_arch = "aarch64")]
        let options: &Vec<Value> = &vec![];
        #[cfg(target_arch = "x86_64")]
        let options = api_logger.options.as_array().unwrap();

        LOGGER
            .init(
                &AppInfo::new("Firecracker", &firecracker_version),
                &instance_id,
                api_logger.log_fifo,
                api_logger.metrics_fifo,
                options,
            )
            .map(|_| VmmData::Empty)
            .map_err(|e| {
                VmmActionError::Logger(
                    ErrorKind::User,
                    LoggerConfigError::InitializationFailure(e.to_string()),
                )
            })
    }

    fn send_response(outcome: VmmRequestOutcome, sender: OutcomeSender) {
        sender
            .send(outcome)
            .map_err(|_| ())
            .expect("one-shot channel closed");
    }

    fn validate_vcpus_are_active(&self) -> std::result::Result<(), StateError> {
        if !self.is_instance_initialized() {
            return Err(StateError::MicroVMIsNotRunning);
        }
        for handle in self.vcpus_handles.iter() {
            handle
                .validate_active()
                .map_err(|_| StateError::VcpusInvalidState)?;
        }
        Ok(())
    }

    fn pause_vcpus(&mut self) -> VmmRequestOutcome {
        self.validate_vcpus_are_active()
            .map_err(PauseMicrovmError::MicroVMInvalidState)?;

        for handle in self.vcpus_handles.iter() {
            handle
                .send_event(VcpuEvent::Pause)
                .map_err(PauseMicrovmError::SignalVcpu)?;
        }
        for handle in self.vcpus_handles.iter() {
            match handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(100))
            {
                Ok(VcpuResponse::Paused) => (),
                _ => Err(PauseMicrovmError::VcpuPause)?,
            }
        }

        Ok(VmmData::Empty)
    }

    fn resume_vcpus(&mut self) -> VmmRequestOutcome {
        self.validate_vcpus_are_active()
            .map_err(ResumeMicrovmError::MicroVMInvalidState)?;

        for handle in self.vcpus_handles.iter() {
            handle
                .send_event(VcpuEvent::Resume)
                .map_err(ResumeMicrovmError::SignalVcpu)?;
        }
        for handle in self.vcpus_handles.iter() {
            match handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(100))
            {
                Ok(VcpuResponse::Resumed) => (),
                _ => Err(ResumeMicrovmError::VcpuResume)?,
            }
        }
        Ok(VmmData::Empty)
    }

    fn initiate_vcpu_pause(&mut self) -> VmmRequestOutcome {
        let vcpus_thread_barrier = Arc::new(Barrier::new(self.vcpus_handles.len() + 1));
        for handle in self.vcpus_handles.iter() {
            handle
                .send_event(VcpuEvent::PauseToSnapshot(vcpus_thread_barrier.clone()))
                .map_err(PauseMicrovmError::SignalVcpu)?;
        }
        // All vcpus need to be out of KVM_RUN before trying serialization.
        vcpus_thread_barrier.wait();
        Ok(VmmData::Empty)
    }

    #[cfg(target_arch = "x86_64")]
    fn serialize_microvm(&mut self) -> VmmRequestOutcome {
        // Retrieve the vcpus states and serialize them.
        // Should any fail, force-resume all.
        // Consume the responses from all vCPUs; otherwise, if the `?` operator breaks the loop
        // while a `VcpuResponse` is still pending, it will be consumed at the next run, where
        // it will most likely be unexpected.
        let responses = self
            .vcpus_handles
            .iter()
            .map(|handle| {
                handle
                    .response_receiver()
                    .recv_timeout(Duration::from_millis(400))
            })
            .collect::<std::result::Result<Vec<VcpuResponse>, RecvTimeoutError>>()
            .map_err(|_| PauseMicrovmError::VcpuPause)?;

        for (idx, response) in responses.into_iter().enumerate() {
            match response {
                VcpuResponse::PausedToSnapshot(vcpu_state) => self
                    .snapshot_image
                    .as_mut()
                    .ok_or(PauseMicrovmError::InvalidSnapshot)?
                    .serialize_vcpu(idx, vcpu_state)
                    .map_err(PauseMicrovmError::SerializeVcpu)?,
                VcpuResponse::SaveStateFailed(err) => {
                    Err(PauseMicrovmError::SaveVcpuState(Some(err)))?
                }
                _ => Err(PauseMicrovmError::SaveVcpuState(None))?,
            }
        }

        // Serialize kvm VM state after the vCPUs are paused and serialized.
        self.snapshot_image
            .as_mut()
            .ok_or(PauseMicrovmError::InvalidSnapshot)?
            .set_kvm_vm_state(
                self.vm
                    .save_state()
                    .map_err(PauseMicrovmError::SaveVmState)?,
            );

        // Persist the snapshot header and the guest memory.
        self.snapshot_image
            .as_mut()
            .ok_or(PauseMicrovmError::InvalidSnapshot)?
            .sync_header()
            .map_err(PauseMicrovmError::SyncHeader)?;
        self.guest_memory
            .as_ref()
            .ok_or(PauseMicrovmError::SyncMemory(
                GuestMemoryError::MemoryNotInitialized,
            ))?
            .sync()
            .map_err(PauseMicrovmError::SyncMemory)?;
        Ok(VmmData::Empty)
    }

    fn mmio_device_states(
        &mut self,
    ) -> std::result::Result<Vec<MmioDeviceState>, MmioDeviceStateError> {
        let mut states: Vec<MmioDeviceState> = Vec::new();

        // Safe to unwrap() because mmio_device_manager is initialized in init_devices(), which is
        // called before the guest boots, and this function is called after boot.
        let device_manager: &MMIODeviceManager = self.mmio_device_manager.as_ref().unwrap();

        for ((device_type, device_id), device_info) in device_manager.get_device_info().iter() {
            let DeviceType::Virtio(type_id) = device_type;

            // We lack support for saving VSOCK devices state for the moment
            #[cfg(feature = "vsock")]
            {
                if *type_id == TYPE_VSOCK {
                    continue;
                }
            }

            // Get the virtio device starting from the BusDevice.
            // The device is listed by the MMIODeviceManager so it should be present on the bus.
            let bus_device_mutex = device_manager
                .get_device(device_type.clone(), device_id)
                .unwrap();
            let bus_device = &mut *bus_device_mutex
                .lock()
                .expect("Failed to save virtio device due to poisoned lock");
            // Any device listed by the MMIODeviceManager should be a MmioDevice
            let mmio_device = bus_device
                .as_mut_any()
                .downcast_mut::<MmioDevice>()
                .unwrap();
            let virtio_device = mmio_device.device_mut();

            // Get the EpollHandler associated with the virtio device
            let maybe_epoll_handler = self
                .epoll_context
                .get_generic_device_handler_by_device_id(*type_id, device_id);
            // If the EpollHandler doesn't exist, the device hasn't been activated yet, so we'll skip it
            if maybe_epoll_handler.is_err() {
                continue;
            }
            let epoll_handler = maybe_epoll_handler.unwrap();

            let device_state = MmioDeviceState::new(
                device_info.addr(),
                device_info.irq(),
                *type_id,
                device_id,
                virtio_device,
                epoll_handler,
            )?;
            states.push(device_state);
        }

        // Sort the devices by addr since they will have to be added back in the same order
        states.sort_by(|a, b| a.addr().partial_cmp(&b.addr()).unwrap());

        Ok(states)
    }

    #[cfg(target_arch = "x86_64")]
    fn save_mmio_devices(&mut self) -> std::result::Result<(), MmioDeviceStateError> {
        // TODO: save devices to file
        self.mmio_device_states()?;

        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn pause_to_snapshot(&mut self) -> VmmRequestOutcome {
        let request_ts = TimestampUs {
            time_us: get_time_us(),
            cputime_us: now_cputime_us(),
        };

        self.validate_vcpus_are_active()
            .map_err(PauseMicrovmError::MicroVMInvalidState)?;

        // Signal vcpus to pause to snapshot.
        self.initiate_vcpu_pause().map_err(|e| {
            self.resume_vcpus()
                .expect("Failed to resume vCPUs after an unsuccessful microVM pause");
            e
        })?;

        // Serialize vCPUs and guest memory.
        self.serialize_microvm().map_err(|e| {
            self.resume_vcpus()
                .expect("Failed to resume vCPUs after an unsuccessful microVM pause");
            e
        })?;

        self.save_mmio_devices()
            .map_err(PauseMicrovmError::SaveMmioDeviceState)?;

        // Relinquish ownership of the snapshot image.
        self.snapshot_image = None;

        Self::log_boot_time(&request_ts);

        Ok(VmmData::Empty)
    }

    #[cfg(target_arch = "x86_64")]
    fn resume_from_snapshot(&mut self, snapshot_path: &str) -> VmmRequestOutcome {
        let request_ts = TimestampUs {
            time_us: get_time_us(),
            cputime_us: now_cputime_us(),
        };
        if self.is_instance_initialized() {
            Err(ResumeMicrovmError::MicroVMInvalidState(
                StateError::MicroVMAlreadyRunning,
            ))?;
        }

        let snapshot_image: SnapshotImage = SnapshotImage::open_existing(
            snapshot_path,
            self.vm.supported_msrs().as_original_struct().nmsrs,
            self.vm.supported_cpuid().as_original_struct().nent,
        )
        .map_err(ResumeMicrovmError::OpenSnapshotFile)?;

        snapshot_image
            .can_deserialize()
            .map_err(ResumeMicrovmError::OpenSnapshotFile)?;

        // Use expect() to crash if the other thread poisoned this lock.
        self.shared_info
            .write()
            .expect("Failed to start microVM because shared info couldn't be written due to poisoned lock")
            .state = InstanceState::Resuming;

        self.vm_config.vcpu_count = Some(snapshot_image.vcpu_count());
        self.vm_config.mem_size_mib = Some(snapshot_image.mem_size_mib());

        self.snapshot_image = Some(snapshot_image);

        self.init_guest_memory()?;

        self.setup_interrupt_controller()?;

        self.vm
            .restore_state(
                self.snapshot_image
                    .as_mut()
                    .unwrap()
                    .kvm_vm_state()
                    .as_ref()
                    .unwrap(),
            )
            .map_err(ResumeMicrovmError::RestoreVmState)?;

        self.attach_legacy_devices()?;

        {
            // Instantiate the MMIO device manager.
            // 'mmio_base' address has to be an address which is protected by the kernel.
            self.mmio_device_manager = Some(MMIODeviceManager::new(
                self.guest_memory
                    .clone()
                    .ok_or(StartMicrovmError::GuestMemory(
                        memory_model::GuestMemoryError::MemoryNotInitialized,
                    ))?,
                &mut (arch::get_reserved_mem_addr() as u64),
                (arch::IRQ_BASE, arch::IRQ_MAX),
            ));
        }
        self.register_events()?;

        self.create_vcpus(request_ts.clone())?;

        self.start_vcpus()?;

        {
            let image = self.snapshot_image.as_mut().unwrap();
            assert_eq!(self.vcpus_handles.len(), image.vcpu_count() as usize);
            for (idx, handle) in self.vcpus_handles.iter_mut().enumerate() {
                let state: VcpuState = image
                    .deser_vcpu(idx)
                    .map_err(ResumeMicrovmError::DeserializeVcpu)?;
                handle
                    .send_event(VcpuEvent::Deserialize(Box::new(state)))
                    .map_err(ResumeMicrovmError::SignalVcpu)?;
            }
        }

        for handle in self.vcpus_handles.iter() {
            match handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(100))
            {
                Ok(VcpuResponse::Deserialized) => (),
                _ => {
                    Err(ResumeMicrovmError::RestoreVcpuState)?;
                }
            }
        }

        // Send the 'resume' command so that vcpus actually start running.
        self.resume_vcpus()?;

        Self::log_boot_time(&request_ts);

        // Use expect() to crash if the other thread poisoned this lock.
        self.shared_info
            .write()
            .expect("Failed to start microVM because shared info couldn't be written due to poisoned lock")
            .state = InstanceState::Running;

        Ok(VmmData::Empty)
    }

    fn run_vmm_action(&mut self) -> Result<()> {
        let request = match self.from_api.try_recv() {
            Ok(t) => *t,
            Err(TryRecvError::Empty) => {
                return Err(Error::ApiChannel)?;
            }
            Err(TryRecvError::Disconnected) => {
                panic!("The channel's sending half was disconnected. Cannot receive data.");
            }
        };

        match request {
            VmmAction::ConfigureBootSource(boot_source_body, sender) => {
                Vmm::send_response(
                    self.configure_boot_source(
                        boot_source_body.kernel_image_path,
                        boot_source_body.boot_args,
                    ),
                    sender,
                );
            }
            VmmAction::ConfigureLogger(logger_description, sender) => {
                Vmm::send_response(self.init_logger(logger_description), sender);
            }
            VmmAction::FlushMetrics(sender) => {
                Vmm::send_response(self.flush_metrics(), sender);
            }
            VmmAction::GetVmConfiguration(sender) => {
                Vmm::send_response(
                    Ok(VmmData::MachineConfiguration(self.vm_config.clone())),
                    sender,
                );
            }
            VmmAction::InsertBlockDevice(block_device_config, sender) => {
                Vmm::send_response(self.insert_block_device(block_device_config), sender);
            }
            VmmAction::InsertNetworkDevice(netif_body, sender) => {
                Vmm::send_response(self.insert_net_device(netif_body), sender);
            }
            #[cfg(feature = "vsock")]
            VmmAction::InsertVsockDevice(vsock_cfg, sender) => {
                Vmm::send_response(self.insert_vsock_device(vsock_cfg), sender);
            }
            #[cfg(target_arch = "x86_64")]
            VmmAction::PauseToSnapshot(sender) => {
                let result = self.pause_to_snapshot();
                let pause_ok = result.is_ok();
                Vmm::send_response(result, sender);
                if pause_ok {
                    thread::sleep(Duration::from_millis(150));
                    self.stop(i32::from(FC_EXIT_CODE_OK));
                }
            }
            VmmAction::PauseVCPUs(sender) => {
                Vmm::send_response(self.pause_vcpus(), sender);
            }
            VmmAction::RescanBlockDevice(drive_id, sender) => {
                Vmm::send_response(self.rescan_block_device(&drive_id), sender);
            }
            VmmAction::ResumeVCPUs(sender) => {
                Vmm::send_response(self.resume_vcpus(), sender);
            }
            #[cfg(target_arch = "x86_64")]
            VmmAction::ResumeFromSnapshot(snapshot_path, sender) => {
                let result = self.resume_from_snapshot(snapshot_path.as_str());
                let resume_failed = result.is_err();
                Vmm::send_response(result, sender);
                if resume_failed {
                    error!("Failed to resume from snapshot. Will terminate the VM.");
                    thread::sleep(Duration::from_millis(150));
                    self.stop(i32::from(FC_EXIT_CODE_RESUME_ERROR));
                }
            }
            VmmAction::StartMicroVm(snapshot_path, sender) => {
                Vmm::send_response(self.start_microvm(snapshot_path), sender);
            }
            VmmAction::SendCtrlAltDel(sender) => {
                Vmm::send_response(self.send_ctrl_alt_del(), sender);
            }
            VmmAction::SetVmConfiguration(machine_config_body, sender) => {
                Vmm::send_response(self.set_vm_configuration(machine_config_body), sender);
            }
            VmmAction::UpdateBlockDevicePath(drive_id, path_on_host, sender) => {
                Vmm::send_response(self.set_block_device_path(drive_id, path_on_host), sender);
            }
            VmmAction::UpdateNetworkInterface(netif_update, sender) => {
                Vmm::send_response(self.update_net_device(netif_update), sender);
            }
        };
        Ok(())
    }

    fn log_boot_time(t0_ts: &TimestampUs) {
        let now_cpu_us = now_cputime_us();
        let now_us = get_time_us();

        let boot_time_us = now_us - t0_ts.time_us;
        let boot_time_cpu_us = now_cpu_us - t0_ts.cputime_us;
        info!(
            "Guest-boot-time = {:>6} us {} ms, {:>6} CPU us {} CPU ms",
            boot_time_us,
            boot_time_us / 1000,
            boot_time_cpu_us,
            boot_time_cpu_us / 1000
        );
    }
}

// Can't derive PartialEq directly because the sender members can't be compared.
// This implementation is only used in tests, but cannot be moved to mod tests,
// because it is used in tests outside of the vmm crate (api_server).
impl PartialEq for VmmAction {
    fn eq(&self, other: &VmmAction) -> bool {
        // Guard match to catch new enums.
        match self {
            VmmAction::ConfigureBootSource(_, _)
            | VmmAction::ConfigureLogger(_, _)
            | VmmAction::GetVmConfiguration(_)
            | VmmAction::FlushMetrics(_)
            | VmmAction::InsertBlockDevice(_, _)
            | VmmAction::InsertNetworkDevice(_, _)
            | VmmAction::PauseVCPUs(_)
            | VmmAction::RescanBlockDevice(_, _)
            | VmmAction::ResumeVCPUs(_)
            | VmmAction::SetVmConfiguration(_, _)
            | VmmAction::SendCtrlAltDel(_)
            | VmmAction::StartMicroVm(_, _)
            | VmmAction::UpdateBlockDevicePath(_, _, _)
            | VmmAction::UpdateNetworkInterface(_, _) => (),
            #[cfg(feature = "vsock")]
            VmmAction::InsertVsockDevice(_, _) => (),
            #[cfg(target_arch = "x86_64")]
            VmmAction::PauseToSnapshot(_) | VmmAction::ResumeFromSnapshot(_, _) => (),
        };
        match (self, other) {
            (
                &VmmAction::ConfigureBootSource(ref boot_source, _),
                &VmmAction::ConfigureBootSource(ref other_boot_source, _),
            ) => boot_source == other_boot_source,
            (
                &VmmAction::ConfigureLogger(ref log, _),
                &VmmAction::ConfigureLogger(ref other_log, _),
            ) => log == other_log,
            (&VmmAction::GetVmConfiguration(_), &VmmAction::GetVmConfiguration(_)) => true,
            (&VmmAction::FlushMetrics(_), &VmmAction::FlushMetrics(_)) => true,
            (
                &VmmAction::InsertBlockDevice(ref block_device, _),
                &VmmAction::InsertBlockDevice(ref other_other_block_device, _),
            ) => block_device == other_other_block_device,
            (
                &VmmAction::InsertNetworkDevice(ref net_dev, _),
                &VmmAction::InsertNetworkDevice(ref other_net_dev, _),
            ) => net_dev == other_net_dev,
            #[cfg(target_arch = "x86_64")]
            (&VmmAction::PauseToSnapshot(_), &VmmAction::PauseToSnapshot(_)) => true,
            (&VmmAction::PauseVCPUs(_), &VmmAction::PauseVCPUs(_)) => true,
            (
                &VmmAction::RescanBlockDevice(ref req, _),
                &VmmAction::RescanBlockDevice(ref other_req, _),
            ) => req == other_req,
            (
                &VmmAction::StartMicroVm(ref path, _),
                &VmmAction::StartMicroVm(ref other_path, _),
            ) => path == other_path,
            (&VmmAction::SendCtrlAltDel(_), &VmmAction::SendCtrlAltDel(_)) => true,
            #[cfg(target_arch = "x86_64")]
            (
                &VmmAction::ResumeFromSnapshot(ref snap_path, _),
                &VmmAction::ResumeFromSnapshot(ref other_snap_path, _),
            ) => snap_path == other_snap_path,
            (&VmmAction::ResumeVCPUs(_), &VmmAction::ResumeVCPUs(_)) => true,
            (
                &VmmAction::SetVmConfiguration(ref vm_config, _),
                &VmmAction::SetVmConfiguration(ref other_vm_config, _),
            ) => vm_config == other_vm_config,
            (
                &VmmAction::UpdateBlockDevicePath(ref drive_id, ref path_on_host, _),
                &VmmAction::UpdateBlockDevicePath(ref other_drive_id, ref other_path_on_host, _),
            ) => drive_id == other_drive_id && path_on_host == other_path_on_host,
            (
                &VmmAction::UpdateNetworkInterface(ref net_dev, _),
                &VmmAction::UpdateNetworkInterface(ref other_net_dev, _),
            ) => net_dev == other_net_dev,
            _ => false,
        }
    }
}

/// Starts a new vmm thread that can service API requests.
///
/// # Arguments
///
/// * `api_shared_info` - A parameter for storing information on the VMM (e.g the current state).
/// * `api_event_fd` - An event fd used for receiving API associated events.
/// * `from_api` - The receiver end point of the communication channel.
/// * `seccomp_level` - The level of seccomp filtering used. Filters are loaded before executing
///                     guest code. Can be one of 0 (seccomp disabled), 1 (filter by syscall
///                     number) or 2 (filter by syscall number and argument values).
/// * `kvm_fd` - Provides the option of supplying an already existing raw file descriptor
///              associated with `/dev/kvm`.
pub fn start_vmm_thread(
    api_shared_info: Arc<RwLock<InstanceInfo>>,
    api_event_fd: EventFd,
    from_api: Receiver<Box<VmmAction>>,
    seccomp_level: u32,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("fc_vmm".to_string())
        .spawn(move || {
            // If this fails, consider it fatal. Use expect().
            let mut vmm = Vmm::new(api_shared_info, api_event_fd, from_api, seccomp_level)
                .expect("Cannot create VMM");
            match vmm.run_control() {
                Ok(()) => {
                    info!("Gracefully terminated VMM control loop");
                    vmm.stop(i32::from(FC_EXIT_CODE_OK))
                }
                Err(e) => {
                    error!("Abruptly exited VMM control loop: {:?}", e);
                    vmm.stop(i32::from(FC_EXIT_CODE_GENERIC_ERROR));
                }
            }
        })
        .expect("VMM thread spawn failed.")
}

#[cfg(test)]
mod tests {
    extern crate tempfile;

    use super::*;

    use serde_json::Value;
    use std::fs::{remove_file, File};
    use std::io::BufRead;
    use std::io::BufReader;
    use std::sync::atomic::AtomicUsize;

    use self::tempfile::NamedTempFile;
    use arch::DeviceType;
    use devices::virtio::{ActivateResult, MmioDevice, Queue};
    use net_util::MacAddr;
    use std::path::Path;
    use vmm_config::machine_config::CpuFeaturesTemplate;
    use vmm_config::{instance_info::KillVcpusError, RateLimiterConfig, TokenBucketConfig};

    fn good_kernel_file() -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let parent = path.parent().unwrap();

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        return [parent.to_str().unwrap(), "kernel/src/loader/test_elf.bin"]
            .iter()
            .collect();
        #[cfg(target_arch = "aarch64")]
        return [parent.to_str().unwrap(), "kernel/src/loader/test_pe.bin"]
            .iter()
            .collect();
    }

    impl Vmm {
        fn get_kernel_cmdline_str(&self) -> &str {
            if let Some(ref k) = self.kernel_config {
                k.cmdline.as_str()
            } else {
                ""
            }
        }

        fn remove_device_info(&mut self, type_id: u32, id: &str) {
            self.mmio_device_manager
                .as_mut()
                .unwrap()
                .remove_device_info(type_id, id);
        }

        fn default_kernel_config(&mut self, cust_kernel_path: Option<PathBuf>) {
            let kernel_temp_file =
                NamedTempFile::new().expect("Failed to create temporary kernel file.");
            let kernel_path = if cust_kernel_path.is_some() {
                cust_kernel_path.unwrap()
            } else {
                kernel_temp_file.path().to_path_buf()
            };
            let kernel_file = File::open(kernel_path).expect("Cannot open kernel file");
            let mut cmdline = kernel_cmdline::Cmdline::new(arch::CMDLINE_MAX_SIZE);
            assert!(cmdline.insert_str(DEFAULT_KERNEL_CMDLINE).is_ok());
            let kernel_cfg = KernelConfig {
                cmdline,
                kernel_file,
                #[cfg(target_arch = "x86_64")]
                cmdline_addr: GuestAddress(arch::x86_64::layout::CMDLINE_START),
            };
            self.configure_kernel(kernel_cfg);
        }

        fn set_instance_state(&mut self, instance_state: InstanceState) {
            self.shared_info.write().unwrap().state = instance_state;
        }

        fn update_block_device_path(&mut self, block_device_id: &str, new_path: PathBuf) {
            for config in self.block_device_configs.config_list.iter_mut() {
                if config.drive_id == block_device_id {
                    config.path_on_host = new_path;
                    break;
                }
            }
        }

        fn change_id(&mut self, prev_id: &str, new_id: &str) {
            for config in self.block_device_configs.config_list.iter_mut() {
                if config.drive_id == prev_id {
                    config.drive_id = new_id.to_string();
                    break;
                }
            }
        }

        #[cfg(target_arch = "x86_64")]
        fn kill_vcpus(&mut self) -> std::result::Result<(), KillVcpusError> {
            self.validate_vcpus_are_active()
                .map_err(KillVcpusError::MicroVMInvalidState)?;

            for handle in self.vcpus_handles.iter() {
                handle
                    .send_event(VcpuEvent::Exit)
                    .map_err(KillVcpusError::SignalVcpu)?;
            }
            for mut handle in self.vcpus_handles.drain(..) {
                handle.join_vcpu_thread().expect("Unreachable.");
            }

            Ok(())
        }
    }

    struct DummyEpollHandler {
        evt: Option<DeviceEventT>,
    }

    impl EpollHandler for DummyEpollHandler {
        fn handle_event(
            &mut self,
            device_event: DeviceEventT,
        ) -> std::result::Result<(), devices::Error> {
            self.evt = Some(device_event);
            Ok(())
        }

        fn interrupt_status(&self) -> usize {
            unimplemented!()
        }

        fn queues(&self) -> Vec<Queue> {
            unimplemented!()
        }
    }

    #[allow(dead_code)]
    #[derive(Clone)]
    struct DummyDevice {
        dummy: u32,
    }

    impl devices::virtio::VirtioDevice for DummyDevice {
        fn device_type(&self) -> u32 {
            0
        }

        fn queue_max_sizes(&self) -> &[u16] {
            &[10]
        }

        fn ack_features(&mut self, page: u32, value: u32) {
            let _ = page;
            let _ = value;
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            let _ = offset;
            let _ = data;
        }

        fn write_config(&mut self, offset: u64, data: &[u8]) {
            let _ = offset;
            let _ = data;
        }

        #[allow(unused_variables)]
        #[allow(unused_mut)]
        fn activate(
            &mut self,
            mem: GuestMemory,
            interrupt_evt: EventFd,
            status: Arc<AtomicUsize>,
            queues: Vec<devices::virtio::Queue>,
            mut queue_evts: Vec<EventFd>,
        ) -> ActivateResult {
            Ok(())
        }

        fn avail_features(&self) -> u64 {
            unimplemented!()
        }

        fn acked_features(&self) -> u64 {
            unimplemented!()
        }

        fn config_space(&self) -> Vec<u8> {
            unimplemented!()
        }
    }

    fn create_vmm_object(state: InstanceState) -> Vmm {
        let shared_info = Arc::new(RwLock::new(InstanceInfo {
            state,
            id: "TEST_ID".to_string(),
            vmm_version: "1.0".to_string(),
        }));

        let (_to_vmm, from_api) = channel();
        Vmm::new(
            shared_info,
            EventFd::new().expect("cannot create eventFD"),
            from_api,
            seccomp::SECCOMP_LEVEL_ADVANCED,
        )
        .expect("Cannot Create VMM")
    }

    /// Generate a random path using a temporary file, which is removed when it goes out of scope.
    fn tmp_path() -> String {
        let tmp_file = NamedTempFile::new().unwrap();
        let tmp_path = tmp_file.into_temp_path();
        tmp_path.to_str().unwrap().to_string()
    }

    #[test]
    fn test_device_handler() {
        let mut ep = EpollContext::new().unwrap();
        let (base, sender) = ep.allocate_tokens(1);
        assert_eq!(ep.device_handlers.len(), 1);
        assert_eq!(base, 1);

        let handler = DummyEpollHandler { evt: None };
        let handler_id = 0;
        assert!(sender.send(Box::new(handler)).is_ok());
        assert!(ep.get_device_handler_by_handler_id(handler_id).is_ok());

        let device_type = 0;
        let device_id = "0";
        ep.device_id_to_handler_id
            .insert((device_type, device_id.to_string()), 0);
        assert!(ep
            .get_generic_device_handler_by_device_id(device_type, device_id)
            .is_ok());
        assert!(ep
            .get_device_handler_by_device_id::<DummyEpollHandler>(device_type, device_id)
            .is_ok());
    }

    #[test]
    fn test_insert_block_device() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let f = NamedTempFile::new().unwrap();
        // Test that creating a new block device returns the correct output.
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm
            .block_device_configs
            .config_list
            .contains(&root_block_device));

        // Test that updating a block device returns the correct output.
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: true,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm
            .block_device_configs
            .config_list
            .contains(&root_block_device));

        // Test insert second drive with the same path fails.
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("dummy_dev"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: false,
            partuuid: None,
            is_read_only: true,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(root_block_device.clone()).is_err());

        // Test inserting a second drive is ok.
        let f = NamedTempFile::new().unwrap();
        // Test that creating a new block device returns the correct output.
        let non_root = BlockDeviceConfig {
            drive_id: String::from("non_root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(non_root).is_ok());

        // Test that making the second device root fails (it would result in 2 root block
        // devices.
        let non_root = BlockDeviceConfig {
            drive_id: String::from("non_root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(non_root).is_err());

        // Test update after boot.
        vmm.set_instance_state(InstanceState::Running);
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: false,
            partuuid: None,
            is_read_only: true,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(root_block_device).is_err())
    }

    #[test]
    fn test_insert_net_device() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        // test create network interface
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname1"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        };
        assert!(vmm.insert_net_device(network_interface).is_ok());

        let mac = MacAddr::parse_str("01:23:45:67:89:0A").unwrap();
        // test update network interface
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname2"),
            guest_mac: Some(mac),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        };
        assert!(vmm.insert_net_device(network_interface).is_ok());

        // Test insert new net device with same mac fails.
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif2"),
            host_dev_name: String::from("hostname3"),
            guest_mac: Some(mac),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        };
        assert!(vmm.insert_net_device(network_interface).is_err());

        // Test that update post-boot fails.
        vmm.set_instance_state(InstanceState::Running);
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname2"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        };
        assert!(vmm.insert_net_device(network_interface).is_err());
    }

    #[test]
    fn test_update_net_device() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        let tbc_1mtps = TokenBucketConfig {
            size: 1024 * 1024,
            one_time_burst: None,
            refill_time: 1000,
        };
        let tbc_2mtps = TokenBucketConfig {
            size: 2 * 1024 * 1024,
            one_time_burst: None,
            refill_time: 1000,
        };

        vmm.insert_net_device(NetworkInterfaceConfig {
            iface_id: String::from("1"),
            host_dev_name: String::from("hostname4"),
            guest_mac: None,
            rx_rate_limiter: Some(RateLimiterConfig {
                bandwidth: Some(tbc_1mtps),
                ops: None,
            }),
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        })
        .unwrap();

        vmm.update_net_device(NetworkInterfaceUpdateConfig {
            iface_id: "1".to_string(),
            rx_rate_limiter: Some(RateLimiterConfig {
                bandwidth: None,
                ops: Some(tbc_2mtps),
            }),
            tx_rate_limiter: Some(RateLimiterConfig {
                bandwidth: None,
                ops: Some(tbc_2mtps),
            }),
        })
        .unwrap();

        {
            let nic_1: &mut NetworkInterfaceConfig =
                vmm.network_interface_configs.iter_mut().next().unwrap();
            // The RX bandwidth should be unaffected.
            assert_eq!(nic_1.rx_rate_limiter.unwrap().bandwidth.unwrap(), tbc_1mtps);
            // The RX ops should be set to 2mtps.
            assert_eq!(nic_1.rx_rate_limiter.unwrap().ops.unwrap(), tbc_2mtps);
            // The TX bandwith should be unlimited (unaffected).
            assert_eq!(nic_1.tx_rate_limiter.unwrap().bandwidth, None);
            // The TX ops should be set to 2mtps.
            assert_eq!(nic_1.tx_rate_limiter.unwrap().ops.unwrap(), tbc_2mtps);
        }

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.setup_interrupt_controller().is_ok());
        vmm.default_kernel_config(None);
        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");

        vmm.attach_net_devices().unwrap();
        vmm.set_instance_state(InstanceState::Running);

        // The update should fail before device activation.
        assert!(vmm
            .update_net_device(NetworkInterfaceUpdateConfig {
                iface_id: "1".to_string(),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            })
            .is_err());

        // Activate the device
        {
            let device_manager = vmm.mmio_device_manager.as_ref().unwrap();
            let bus_device_mutex = device_manager
                .get_device(DeviceType::Virtio(TYPE_NET), "1")
                .unwrap();
            let bus_device = &mut *bus_device_mutex.lock().unwrap();
            let mmio_device: &mut MmioDevice = bus_device
                .as_mut_any()
                .downcast_mut::<MmioDevice>()
                .unwrap();

            assert!(mmio_device
                .device_mut()
                .activate(
                    vmm.guest_memory.as_ref().unwrap().clone(),
                    EventFd::new().unwrap(),
                    Arc::new(AtomicUsize::new(0)),
                    vec![Queue::new(0), Queue::new(0)],
                    vec![EventFd::new().unwrap(), EventFd::new().unwrap()],
                )
                .is_ok());
        }

        // the update should succeed after the device activation
        vmm.update_net_device(NetworkInterfaceUpdateConfig {
            iface_id: "1".to_string(),
            rx_rate_limiter: Some(RateLimiterConfig {
                bandwidth: Some(tbc_2mtps),
                ops: None,
            }),
            tx_rate_limiter: Some(RateLimiterConfig {
                bandwidth: Some(tbc_1mtps),
                ops: None,
            }),
        })
        .unwrap();
    }

    #[test]
    #[allow(clippy::cyclomatic_complexity)]
    fn test_machine_configuration() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        // test the default values of machine config
        // vcpu_count = 1
        assert_eq!(vmm.vm_config.vcpu_count, Some(1));
        // mem_size = 128
        assert_eq!(vmm.vm_config.mem_size_mib, Some(128));
        // ht_enabled = false
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));
        // no cpu template
        assert!(vmm.vm_config.cpu_template.is_none());

        // 1. Tests with no hyperthreading
        // test put machine configuration for vcpu count with valid value
        let machine_config = VmConfig {
            vcpu_count: Some(3),
            mem_size_mib: None,
            ht_enabled: None,
            cpu_template: None,
        };
        assert!(vmm.set_vm_configuration(machine_config).is_ok());
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));
        assert_eq!(vmm.vm_config.mem_size_mib, Some(128));
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));

        // test put machine configuration for mem size with valid value
        let machine_config = VmConfig {
            vcpu_count: None,
            mem_size_mib: Some(256),
            ht_enabled: None,
            cpu_template: None,
        };
        assert!(vmm.set_vm_configuration(machine_config).is_ok());
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));
        assert_eq!(vmm.vm_config.mem_size_mib, Some(256));
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));

        // Test Error cases for put_machine_configuration with invalid value for vcpu_count
        // Test that the put method return error & that the vcpu value is not changed
        let machine_config = VmConfig {
            vcpu_count: Some(0),
            mem_size_mib: None,
            ht_enabled: None,
            cpu_template: None,
        };
        assert!(vmm.set_vm_configuration(machine_config).is_err());
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));

        // Test Error cases for put_machine_configuration with invalid value for the mem_size_mib
        // Test that the put method return error & that the mem_size_mib value is not changed
        let machine_config = VmConfig {
            vcpu_count: Some(1),
            mem_size_mib: Some(0),
            ht_enabled: Some(false),
            cpu_template: Some(CpuFeaturesTemplate::T2),
        };
        assert!(vmm.set_vm_configuration(machine_config).is_err());
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));
        assert_eq!(vmm.vm_config.mem_size_mib, Some(256));
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));
        assert!(vmm.vm_config.cpu_template.is_none());

        // 2. Test with hyperthreading enabled
        // Test that you can't change the hyperthreading value to false when the vcpu count
        // is odd
        let machine_config = VmConfig {
            vcpu_count: None,
            mem_size_mib: None,
            ht_enabled: Some(true),
            cpu_template: None,
        };
        assert!(vmm.set_vm_configuration(machine_config).is_err());
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));
        // Test that you can change the ht flag when you have a valid vcpu count
        // Also set the CPU Template since we are here
        let machine_config = VmConfig {
            vcpu_count: Some(2),
            mem_size_mib: None,
            ht_enabled: Some(true),
            cpu_template: Some(CpuFeaturesTemplate::T2),
        };
        assert!(vmm.set_vm_configuration(machine_config).is_ok());
        assert_eq!(vmm.vm_config.vcpu_count, Some(2));
        assert_eq!(vmm.vm_config.ht_enabled, Some(true));
        assert_eq!(vmm.vm_config.cpu_template, Some(CpuFeaturesTemplate::T2));

        // 3. Test update vm configuration after boot.
        vmm.set_instance_state(InstanceState::Running);
        let machine_config = VmConfig {
            vcpu_count: Some(2),
            mem_size_mib: None,
            ht_enabled: Some(true),
            cpu_template: Some(CpuFeaturesTemplate::T2),
        };
        assert!(vmm.set_vm_configuration(machine_config).is_err());
    }

    #[test]
    fn new_epoll_context_test() {
        assert!(EpollContext::new().is_ok());
    }

    #[test]
    fn enable_disable_stdin_test() {
        let mut ep = EpollContext::new().unwrap();
        // enabling stdin should work
        assert!(ep.enable_stdin_event().is_ok());

        // doing it again should fail
        // TODO: commented out because stdin & /dev/null related issues, as mentioned in another
        // comment from enable_stdin_event().
        // assert!(ep.enable_stdin_event().is_err());

        // disabling stdin should work
        assert!(ep.disable_stdin_event().is_ok());

        // enabling stdin should work now
        assert!(ep.enable_stdin_event().is_ok());
        // disabling it again should work
        assert!(ep.disable_stdin_event().is_ok());
    }

    #[test]
    fn add_event_test() {
        let mut ep = EpollContext::new().unwrap();
        let evfd = EventFd::new().unwrap();

        // adding new event should work
        let epev = ep.add_event(evfd, EpollDispatch::Exit);
        assert!(epev.is_ok());
    }

    #[test]
    fn epoll_event_test() {
        let mut ep = EpollContext::new().unwrap();
        let evfd = EventFd::new().unwrap();

        // adding new event should work
        let epev = ep.add_event(evfd, EpollDispatch::Exit);
        assert!(epev.is_ok());
        let epev = epev.unwrap();

        let evpoll_events_len = 10;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); evpoll_events_len];

        // epoll should have no pending events
        let epollret = epoll::wait(ep.epoll_raw_fd, 0, &mut events[..]);
        let num_events = epollret.unwrap();
        assert_eq!(num_events, 0);

        // raise the event
        assert!(epev.fd.write(1).is_ok());

        // epoll should report one event
        let epollret = epoll::wait(ep.epoll_raw_fd, 0, &mut events[..]);
        let num_events = epollret.unwrap();
        assert_eq!(num_events, 1);

        // reported event should be the one we raised
        let idx = events[0].data as usize;
        assert!(ep.dispatch_table[idx].is_some());
        assert_eq!(
            *ep.dispatch_table[idx].as_ref().unwrap(),
            EpollDispatch::Exit
        );
    }

    #[test]
    fn test_kvm_context() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::FromRawFd;

        let c = KvmContext::new().unwrap();

        assert!(c.max_memslots >= 32);

        let kvm = Kvm::new().unwrap();
        let f = unsafe { File::from_raw_fd(kvm.as_raw_fd()) };
        let m1 = f.metadata().unwrap();
        let m2 = File::open("/dev/kvm").unwrap().metadata().unwrap();

        assert_eq!(m1.dev(), m2.dev());
        assert_eq!(m1.ino(), m2.ino());
    }

    #[test]
    fn test_check_health() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.check_health().is_err());

        let dummy_addr = GuestAddress(0x1000);
        vmm.configure_kernel(KernelConfig {
            #[cfg(target_arch = "x86_64")]
            cmdline_addr: dummy_addr,
            cmdline: kernel_cmdline::Cmdline::new(10),
            kernel_file: tempfile::tempfile().unwrap(),
        });
        assert!(vmm.check_health().is_ok());
    }

    #[test]
    fn test_microvm_start() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.shared_info.write().unwrap().id = String::from("microvm_start_test");

        vmm.default_kernel_config(Some(good_kernel_file()));
        // The kernel provided contains  "return 0" which will make the
        // advanced seccomp filter return bad syscall so we disable it.
        vmm.seccomp_level = seccomp::SECCOMP_LEVEL_NONE;
        let res = vmm.start_microvm(None);
        let stdin_handle = io::stdin();
        stdin_handle.lock().set_canon_mode().unwrap();
        // Kill vcpus and join spawned threads.
        vmm.kill_vcpus().expect("failed to kill vcpus");
        assert!(res.is_ok());
    }

    #[test]
    fn test_instance_state() {
        let vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(!vmm.is_instance_initialized());
        assert!(!vmm.is_instance_running());

        let vmm = create_vmm_object(InstanceState::Starting);
        assert!(vmm.is_instance_initialized());
        assert!(!vmm.is_instance_running());

        let vmm = create_vmm_object(InstanceState::Halting);
        assert!(vmm.is_instance_initialized());
        assert!(!vmm.is_instance_running());

        let vmm = create_vmm_object(InstanceState::Halted);
        assert!(vmm.is_instance_initialized());
        assert!(!vmm.is_instance_running());

        let vmm = create_vmm_object(InstanceState::Running);
        assert!(vmm.is_instance_initialized());
        assert!(vmm.is_instance_running());
    }

    #[test]
    fn test_attach_block_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let block_file = NamedTempFile::new().unwrap();

        // Use Case 1: Root Block Device is not specified through PARTUUID.
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: block_file.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        // Test that creating a new block device returns the correct output.
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());
        assert!(vmm.setup_interrupt_controller().is_ok());

        vmm.default_kernel_config(None);
        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");

        assert!(vmm.attach_block_devices().is_ok());
        assert!(vmm.get_kernel_cmdline_str().contains("root=/dev/vda rw"));

        // Use Case 2: Root Block Device is specified through PARTUUID.
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: block_file.path().to_path_buf(),
            is_root_device: true,
            partuuid: Some("0eaa91a0-01".to_string()),
            is_read_only: false,
            rate_limiter: None,
        };

        // Test that creating a new block device returns the correct output.
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());
        assert!(vmm.setup_interrupt_controller().is_ok());

        vmm.default_kernel_config(None);
        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");

        assert!(vmm.attach_block_devices().is_ok());
        assert!(vmm
            .get_kernel_cmdline_str()
            .contains("root=PARTUUID=0eaa91a0-01 rw"));

        // Use Case 3: Root Block Device is not added at all.
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let non_root_block_device = BlockDeviceConfig {
            drive_id: String::from("not_root"),
            path_on_host: block_file.path().to_path_buf(),
            is_root_device: false,
            partuuid: Some("0eaa91a0-01".to_string()),
            is_read_only: false,
            rate_limiter: None,
        };

        // Test that creating a new block device returns the correct output.
        assert!(vmm
            .insert_block_device(non_root_block_device.clone())
            .is_ok());
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());
        assert!(vmm.setup_interrupt_controller().is_ok());

        vmm.default_kernel_config(None);
        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");

        assert!(vmm.attach_block_devices().is_ok());
        // Test that kernel commandline does not contain either /dev/vda or PARTUUID.
        assert!(!vmm.get_kernel_cmdline_str().contains("root=PARTUUID="));
        assert!(!vmm.get_kernel_cmdline_str().contains("root=/dev/vda"));

        // Test that the non root device is attached.
        {
            let device_manager = vmm.mmio_device_manager.as_ref().unwrap();
            assert!(device_manager
                .get_device(
                    DeviceType::Virtio(TYPE_BLOCK),
                    &non_root_block_device.drive_id
                )
                .is_some());
        }

        // Test partial update of block devices.
        let new_block = NamedTempFile::new().unwrap();
        let path = String::from(new_block.path().to_path_buf().to_str().unwrap());
        assert!(vmm
            .set_block_device_path("not_root".to_string(), path)
            .is_ok());

        // Test partial update of block device fails due to invalid file.
        assert!(vmm
            .set_block_device_path("not_root".to_string(), String::from("dummy_path"))
            .is_err());
    }

    #[test]
    fn test_attach_net_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        vmm.default_kernel_config(None);
        vmm.setup_interrupt_controller()
            .expect("Failed to setup interrupt controller");
        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");

        // test create network interface
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname5"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        };

        assert!(vmm.insert_net_device(network_interface).is_ok());

        assert!(vmm.attach_net_devices().is_ok());
        // a second call to attach_net_devices should fail because when
        // we are creating the virtio::Net object, we are taking the tap.
        assert!(vmm.attach_net_devices().is_err());
    }

    #[test]
    fn test_init_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.default_kernel_config(None);
        assert!(vmm.init_guest_memory().is_ok());
        vmm.setup_interrupt_controller()
            .expect("Failed to setup interrupt controller");

        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");
        assert!(vmm.attach_virtio_devices().is_ok());
    }

    #[test]
    fn test_configure_boot_source() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        // Test invalid kernel path.
        assert!(vmm
            .configure_boot_source(String::from("dummy-path"), None)
            .is_err());

        // Test valid kernel path and invalid cmdline.
        let kernel_file = NamedTempFile::new().expect("Failed to create temporary kernel file.");
        let kernel_path = String::from(kernel_file.path().to_path_buf().to_str().unwrap());
        let invalid_cmdline = String::from_utf8(vec![b'X'; arch::CMDLINE_MAX_SIZE + 1]).unwrap();
        assert!(vmm
            .configure_boot_source(kernel_path.clone(), Some(invalid_cmdline))
            .is_err());

        // Test valid configuration.
        assert!(vmm.configure_boot_source(kernel_path.clone(), None).is_ok());
        assert!(vmm
            .configure_boot_source(kernel_path.clone(), Some(String::from("reboot=k")))
            .is_ok());

        // Test valid configuration after boot (should fail).
        vmm.set_instance_state(InstanceState::Running);
        assert!(vmm
            .configure_boot_source(kernel_path.clone(), None)
            .is_err());
    }

    #[test]
    fn test_block_device_rescan() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.default_kernel_config(None);

        let root_file = NamedTempFile::new().unwrap();
        let scratch_file = NamedTempFile::new().unwrap();
        let scratch_id = "not_root".to_string();

        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: root_file.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        let non_root_block_device = BlockDeviceConfig {
            drive_id: scratch_id.clone(),
            path_on_host: scratch_file.path().to_path_buf(),
            is_root_device: false,
            partuuid: None,
            is_read_only: true,
            rate_limiter: None,
        };

        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm
            .insert_block_device(non_root_block_device.clone())
            .is_ok());

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());
        assert!(vmm.setup_interrupt_controller().is_ok());

        vmm.init_mmio_device_manager()
            .expect("Cannot initialize mmio device manager");

        {
            let dummy_box = Box::new(DummyDevice { dummy: 0 });
            let device_manager = vmm.mmio_device_manager.as_mut().unwrap();

            // Use a dummy command line as it is not used in this test.
            let _addr = device_manager
                .register_virtio_device(
                    vmm.vm.get_fd(),
                    dummy_box,
                    &mut kernel_cmdline::Cmdline::new(arch::CMDLINE_MAX_SIZE),
                    TYPE_BLOCK,
                    &scratch_id,
                )
                .unwrap();
        }

        vmm.set_instance_state(InstanceState::Running);

        // Test valid rescan_block_device.
        assert!(vmm.rescan_block_device(&scratch_id).is_ok());

        // Test rescan block device with size not a multiple of sector size.
        let new_size = 10 * virtio::block::SECTOR_SIZE + 1;
        scratch_file.as_file().set_len(new_size).unwrap();
        assert!(vmm.rescan_block_device(&scratch_id).is_ok());

        // Test rescan block device with invalid path.
        let prev_path = non_root_block_device.path_on_host().clone();
        vmm.update_block_device_path(&scratch_id, PathBuf::from("foo"));
        match vmm.rescan_block_device(&scratch_id) {
            Err(VmmActionError::DriveConfig(
                ErrorKind::User,
                DriveError::BlockDeviceUpdateFailed,
            )) => (),
            _ => assert!(false),
        }
        vmm.update_block_device_path(&scratch_id, prev_path);

        // Test rescan_block_device with invalid ID.
        match vmm.rescan_block_device(&"foo".to_string()) {
            Err(VmmActionError::DriveConfig(ErrorKind::User, DriveError::InvalidBlockDeviceID)) => {
            }
            _ => assert!(false),
        }
        vmm.change_id(&scratch_id, "scratch");
        match vmm.rescan_block_device(&scratch_id) {
            Err(VmmActionError::DriveConfig(ErrorKind::User, DriveError::InvalidBlockDeviceID)) => {
            }
            _ => assert!(false),
        }

        // Test rescan_block_device with invalid device address.
        vmm.remove_device_info(TYPE_BLOCK, &scratch_id);
        match vmm.rescan_block_device(&scratch_id) {
            Err(VmmActionError::DriveConfig(ErrorKind::User, DriveError::InvalidBlockDeviceID)) => {
            }
            _ => assert!(false),
        }

        // Test rescan not allowed.
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm
            .insert_block_device(non_root_block_device.clone())
            .is_ok());
        match vmm.rescan_block_device(&scratch_id) {
            Err(VmmActionError::DriveConfig(
                ErrorKind::User,
                DriveError::OperationNotAllowedPreBoot,
            )) => (),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_init_logger_from_api() {
        // Error case: update after instance is running
        let log_file = NamedTempFile::new().unwrap();
        let metrics_file = NamedTempFile::new().unwrap();
        let desc = LoggerConfig {
            log_fifo: log_file.path().to_str().unwrap().to_string(),
            metrics_fifo: metrics_file.path().to_str().unwrap().to_string(),
            level: LoggerLevel::Warning,
            show_level: true,
            show_log_origin: true,
            #[cfg(target_arch = "x86_64")]
            options: Value::Array(vec![]),
        };

        let mut vmm = create_vmm_object(InstanceState::Running);
        assert!(vmm.init_logger(desc).is_err());

        // Reset vmm state to test the other scenarios.
        vmm.set_instance_state(InstanceState::Uninitialized);

        // Error case: initializing logger with invalid pipes returns error.
        let desc = LoggerConfig {
            log_fifo: String::from("not_found_file_log"),
            metrics_fifo: String::from("not_found_file_metrics"),
            level: LoggerLevel::Warning,
            show_level: false,
            show_log_origin: false,
            #[cfg(target_arch = "x86_64")]
            options: Value::Array(vec![]),
        };
        assert!(vmm.init_logger(desc).is_err());

        // Error case: initializing logger with invalid option flags returns error.
        let desc = LoggerConfig {
            log_fifo: String::from("not_found_file_log"),
            metrics_fifo: String::from("not_found_file_metrics"),
            level: LoggerLevel::Warning,
            show_level: false,
            show_log_origin: false,
            #[cfg(target_arch = "x86_64")]
            options: Value::Array(vec![Value::String("foobar".to_string())]),
        };
        assert!(vmm.init_logger(desc).is_err());

        // Initializing logger with valid pipes is ok.
        let log_file = NamedTempFile::new().unwrap();
        let metrics_file = NamedTempFile::new().unwrap();
        let desc = LoggerConfig {
            log_fifo: log_file.path().to_str().unwrap().to_string(),
            metrics_fifo: metrics_file.path().to_str().unwrap().to_string(),
            level: LoggerLevel::Info,
            show_level: true,
            show_log_origin: true,
            #[cfg(target_arch = "x86_64")]
            options: Value::Array(vec![Value::String("LogDirtyPages".to_string())]),
        };
        // Flushing metrics before initializing logger is erroneous.
        let err = vmm.flush_metrics();
        assert!(err.is_err());
        assert_eq!(
            format!("{:?}", err.unwrap_err()),
            "Logger(Internal, FlushMetrics(\"Logger was not initialized.\"))"
        );

        assert!(vmm.init_logger(desc).is_ok());

        assert!(vmm.flush_metrics().is_ok());

        let f = File::open(metrics_file).unwrap();
        let mut reader = BufReader::new(f);

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("utc_timestamp_ms"));

        // It is safe to do that because the tests are run sequentially (so no other test may be
        // writing to the same file.
        assert!(vmm.flush_metrics().is_ok());
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("utc_timestamp_ms"));

        // Validate logfile works.
        warn!("this is a test");

        let f = File::open(log_file).unwrap();
        let mut reader = BufReader::new(f);

        let mut line = String::new();
        loop {
            if line.contains("this is a test") {
                break;
            }
            if reader.read_line(&mut line).unwrap() == 0 {
                // If it ever gets here, this assert will fail.
                assert!(line.contains("this is a test"));
            }
        }

        // Validate logging the boot time works.
        Vmm::log_boot_time(&TimestampUs::default());
        let mut line = String::new();
        loop {
            if line.contains("Guest-boot-time =") {
                break;
            }
            if reader.read_line(&mut line).unwrap() == 0 {
                // If it ever gets here, this assert will fail.
                assert!(line.contains("Guest-boot-time ="));
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_dirty_page_count() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert_eq!(vmm.get_dirty_page_count(), 0);
        // Booting an actual guest and getting real data is covered by `kvm::tests::run_code_test`.
    }

    #[test]
    fn test_create_vcpus() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.default_kernel_config(None);

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.vm.get_memory().is_some());

        #[cfg(target_arch = "x86_64")]
        // `KVM_CREATE_VCPU` fails if the irqchip is not created beforehand. This is x86_64 speciifc.
        vmm.vm
            .setup_irqchip()
            .expect("Cannot create IRQCHIP or PIT");

        assert!(vmm.create_vcpus(TimestampUs::default()).is_ok());
    }

    #[test]
    fn test_setup_interrupt_controller() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.setup_interrupt_controller().is_ok());
    }

    #[test]
    fn test_load_kernel() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert_eq!(
            vmm.load_kernel().unwrap_err().to_string(),
            "Cannot start microvm without kernel configuration."
        );

        vmm.default_kernel_config(None);

        assert_eq!(
            vmm.load_kernel().unwrap_err().to_string(),
            "Invalid Memory Configuration: MemoryNotInitialized"
        );

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.vm.get_memory().is_some());

        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            vmm.load_kernel().unwrap_err().to_string(),
            "Cannot load kernel due to invalid memory configuration or invalid kernel image. Failed to read magic number"
        );

        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            vmm.load_kernel().unwrap_err().to_string(),
            "Cannot load kernel due to invalid memory configuration or invalid kernel image. Failed to read ELF header"
        );

        vmm.default_kernel_config(Some(good_kernel_file()));
        assert!(vmm.load_kernel().is_ok());
    }

    #[test]
    fn test_configure_system() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert_eq!(
            vmm.configure_system().unwrap_err().to_string(),
            "Cannot start microvm without kernel configuration."
        );

        vmm.default_kernel_config(None);

        assert_eq!(
            vmm.configure_system().unwrap_err().to_string(),
            "Invalid Memory Configuration: MemoryNotInitialized"
        );

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.vm.get_memory().is_some());

        assert!(vmm.configure_system().is_ok());
    }

    #[test]
    fn test_attach_virtio_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.default_kernel_config(None);

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.vm.get_memory().is_some());
        vmm.setup_interrupt_controller()
            .expect("Failed to setup interrupt controller");

        // Create test network interface.
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname6"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
            tap: None,
        };

        assert!(vmm.insert_net_device(network_interface).is_ok());
        assert!(vmm.attach_virtio_devices().is_ok());
        assert!(vmm.mmio_device_manager.is_some());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_attach_legacy_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        assert!(vmm.attach_legacy_devices().is_ok());
        assert!(vmm.legacy_device_manager.io_bus.get_device(0x3f8).is_some());
        assert!(vmm.legacy_device_manager.io_bus.get_device(0x2f8).is_some());
        assert!(vmm.legacy_device_manager.io_bus.get_device(0x3e8).is_some());
        assert!(vmm.legacy_device_manager.io_bus.get_device(0x2e8).is_some());
        assert!(vmm.legacy_device_manager.io_bus.get_device(0x060).is_some());
        let stdin_handle = io::stdin();
        stdin_handle.lock().set_canon_mode().unwrap();
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_attach_legacy_devices_without_uart() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let device_manager = MMIODeviceManager::new(
            guest_mem.clone(),
            &mut (arch::get_reserved_mem_addr() as u64),
            (arch::IRQ_BASE, arch::IRQ_MAX),
        );
        vmm.mmio_device_manager = Some(device_manager);

        vmm.default_kernel_config(None);
        vmm.setup_interrupt_controller()
            .expect("Failed to setup interrupt controller");
        assert!(vmm.attach_legacy_devices().is_ok());
        let kernel_config = vmm.kernel_config.as_mut();

        let dev_man = vmm.mmio_device_manager.as_ref().unwrap();
        // On aarch64, we are using first region of the memory
        // reserved for attaching MMIO devices for measuring boot time.
        assert!(dev_man
            .bus
            .get_device(arch::get_reserved_mem_addr() as u64)
            .is_none());
        assert!(dev_man
            .get_device_info()
            .get(&(DeviceType::Serial, "uart".to_string()))
            .is_none());
        assert!(dev_man
            .get_device_info()
            .get(&(DeviceType::RTC, "rtc".to_string()))
            .is_some());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_attach_legacy_devices_with_uart() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let device_manager = MMIODeviceManager::new(
            guest_mem.clone(),
            &mut (arch::get_reserved_mem_addr() as u64),
            (arch::IRQ_BASE, arch::IRQ_MAX),
        );
        vmm.mmio_device_manager = Some(device_manager);

        vmm.default_kernel_config(None);
        vmm.setup_interrupt_controller()
            .expect("Failed to setup interrupt controller");
        {
            let kernel_config = vmm.kernel_config.as_mut().unwrap();
            kernel_config.cmdline.insert("console", "tty1").unwrap();
        }
        assert!(vmm.attach_legacy_devices().is_ok());
        let dev_man = vmm.mmio_device_manager.as_ref().unwrap();
        assert!(dev_man
            .get_device_info()
            .get(&(DeviceType::Serial, "uart".to_string()))
            .is_some());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn microvm_pause_and_resume_from_snapshot() {
        let microvm_id = String::from("pause_resume_test");
        let mut vmm1_wrap = Some(create_vmm_object(InstanceState::Uninitialized));
        let mut vmm2 = create_vmm_object(InstanceState::Running);
        // Create microVM and snapshot it.
        let snapshot_filename = tmp_path();

        {
            // Take it out of the wrapper so it goes out of scope at the end of this block.
            let mut vmm1 = vmm1_wrap.take().unwrap();
            vmm1.shared_info.write().unwrap().id = microvm_id.clone();

            vmm1.default_kernel_config(Some(good_kernel_file()));
            // The kernel provided contains  "return 0" which will make the
            // advanced seccomp filter return bad syscall so we disable it.
            vmm1.seccomp_level = seccomp::SECCOMP_LEVEL_NONE;
            vmm1.start_microvm(Some(snapshot_filename.clone()))
                .expect("failed to start microvm");
            let stdin_handle = io::stdin();
            stdin_handle.lock().set_canon_mode().unwrap();

            let tmp_img = vmm1.snapshot_image;
            vmm1.snapshot_image = None;
            assert!(vmm1.pause_to_snapshot().is_err());
            vmm1.snapshot_image = tmp_img;
            assert!(vmm1.pause_to_snapshot().is_ok());
        }

        // Wait a bit to make sure all the kvm resources associated with this thread are released.
        thread::sleep(Duration::from_millis(100));
        // Resume second microVM from snapshot.
        {
            vmm2.shared_info.write().unwrap().id = microvm_id.clone();
            vmm2.seccomp_level = seccomp::SECCOMP_LEVEL_NONE;
            assert!(vmm2
                .resume_from_snapshot(snapshot_filename.as_str())
                .is_err());
            vmm2.shared_info.write().unwrap().state = InstanceState::Uninitialized;
            assert!(vmm2
                .resume_from_snapshot(snapshot_filename.as_str())
                .is_ok());
            let stdin_handle = io::stdin();
            stdin_handle.lock().set_canon_mode().unwrap();
            // Kill vcpus and join spawned threads.
            vmm2.kill_vcpus().expect("failed to kill vcpus");
        }
        std::fs::remove_file(snapshot_filename).expect("failed to delete snapshot");
    }

    // Helper function to get ErrorKind of error.
    fn error_kind<T: std::convert::Into<VmmActionError>>(err: T) -> ErrorKind {
        let err: VmmActionError = err.into();
        err.kind().clone()
    }

    #[test]
    fn test_drive_error_conversion() {
        // Test `DriveError` conversion
        assert_eq!(
            error_kind(DriveError::CannotOpenBlockDevice),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(DriveError::InvalidBlockDevicePath),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(DriveError::BlockDevicePathAlreadyExists),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(DriveError::BlockDeviceUpdateFailed),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(DriveError::OperationNotAllowedPreBoot),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(DriveError::UpdateNotAllowedPostBoot),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(DriveError::RootBlockDeviceAlreadyAdded),
            ErrorKind::User
        );
    }

    #[test]
    fn test_vmconfig_error_conversion() {
        // Test `VmConfigError` conversion
        assert_eq!(error_kind(VmConfigError::InvalidVcpuCount), ErrorKind::User);
        assert_eq!(
            error_kind(VmConfigError::InvalidMemorySize),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(VmConfigError::UpdateNotAllowedPostBoot),
            ErrorKind::User
        );
    }

    #[test]
    fn test_network_interface_error_conversion() {
        // Test `NetworkInterfaceError` conversion
        assert_eq!(
            error_kind(NetworkInterfaceError::GuestMacAddressInUse(String::new())),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(NetworkInterfaceError::EpollHandlerNotFound(
                Error::DeviceEventHandlerNotFound
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(NetworkInterfaceError::HostDeviceNameInUse(String::new())),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(NetworkInterfaceError::DeviceIdNotFound),
            ErrorKind::User
        );
        // NetworkInterfaceError::OpenTap can be of multiple kinds.
        {
            assert_eq!(
                error_kind(NetworkInterfaceError::OpenTap(TapError::OpenTun(
                    io::Error::from_raw_os_error(0)
                ))),
                ErrorKind::User
            );
            assert_eq!(
                error_kind(NetworkInterfaceError::OpenTap(TapError::CreateTap(
                    io::Error::from_raw_os_error(0)
                ))),
                ErrorKind::User
            );
            assert_eq!(
                error_kind(NetworkInterfaceError::OpenTap(TapError::IoctlError(
                    io::Error::from_raw_os_error(0)
                ))),
                ErrorKind::Internal
            );
            assert_eq!(
                error_kind(NetworkInterfaceError::OpenTap(TapError::NetUtil(
                    net_util::Error::CreateSocket(io::Error::from_raw_os_error(0))
                ))),
                ErrorKind::Internal
            );
            assert_eq!(
                error_kind(NetworkInterfaceError::OpenTap(TapError::InvalidIfname)),
                ErrorKind::User
            );
        }
        assert_eq!(
            error_kind(NetworkInterfaceError::RateLimiterUpdateFailed(
                devices::Error::FailedReadTap
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(NetworkInterfaceError::UpdateNotAllowedPostBoot),
            ErrorKind::User
        );
    }

    #[test]
    fn test_start_microvm_error_conversion_cl() {
        // Test `StartMicrovmError` conversion
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            error_kind(StartMicrovmError::ConfigureSystem(
                arch::Error::X86_64Setup(arch::x86_64::Error::ZeroPageSetup)
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::ConfigureVm(
                vstate::Error::NotEnoughMemorySlots
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::CreateBlockDevice(
                io::Error::from_raw_os_error(0)
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::CreateNetDevice(
                devices::virtio::Error::TapOpen(TapError::CreateTap(io::Error::from_raw_os_error(
                    0
                )))
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::CreateRateLimiter(
                io::Error::from_raw_os_error(0)
            )),
            ErrorKind::Internal
        );
        #[cfg(feature = "vsock")]
        assert_eq!(
            error_kind(StartMicrovmError::CreateVsockDevice(
                devices::virtio::vhost::Error::PollError(io::Error::from_raw_os_error(0))
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::DeviceManager),
            ErrorKind::Internal
        );
        assert_eq!(error_kind(StartMicrovmError::EventFd), ErrorKind::Internal);
        assert_eq!(
            error_kind(StartMicrovmError::GuestMemory(
                memory_model::GuestMemoryError::NoMemoryRegions
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::KernelCmdline(String::new())),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::KernelLoader(
                kernel_loader::Error::SeekKernelImage
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::LegacyIOBus(
                device_manager::legacy::Error::EventFd(io::Error::from_raw_os_error(0))
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::LoadCommandline(
                kernel::cmdline::Error::CommandLineOverflow
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::LoadCommandline(
                kernel::cmdline::Error::CommandLineCopy
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::SnapshotBackingFile(
                snapshot::Error::InvalidSnapshot
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::MicroVMInvalidState(
                StateError::MicroVMAlreadyRunning
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::MicroVMInvalidState(
                StateError::VcpusInvalidState
            )),
            ErrorKind::Internal
        );
    }

    #[test]
    #[allow(clippy::cyclomatic_complexity)]
    fn test_start_microvm_error_conversion_mv() {
        assert_eq!(
            error_kind(StartMicrovmError::MicroVMInvalidState(
                StateError::MicroVMAlreadyRunning
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::MicroVMInvalidState(
                StateError::VcpusInvalidState
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::MissingKernelConfig),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::NetDeviceNotConfigured),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::OpenBlockDevice(
                io::Error::from_raw_os_error(0)
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::RegisterBlockDevice(
                device_manager::mmio::Error::IrqsExhausted
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::RegisterEvent),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::RegisterNetDevice(
                device_manager::mmio::Error::IrqsExhausted
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::RegisterMMIODevice(
                device_manager::mmio::Error::IrqsExhausted
            )),
            ErrorKind::Internal
        );
        #[cfg(feature = "vsock")]
        assert_eq!(
            error_kind(StartMicrovmError::RegisterVsockDevice(
                device_manager::mmio::Error::IrqsExhausted
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::SeccompFilters(
                seccomp::Error::InvalidArgumentNumber
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::Vcpu(vstate::Error::VcpuUnhandledKvmExit)),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::VcpuConfigure(
                vstate::Error::VcpuSetCpuid(io::Error::from_raw_os_error(0))
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(StartMicrovmError::VcpusNotConfigured),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(StartMicrovmError::VcpuSpawn(vstate::Error::VcpuSpawn(
                io::Error::from_raw_os_error(0)
            ))),
            ErrorKind::Internal
        );
        // Test `PauseMicrovmError` conversion.
        assert_eq!(
            error_kind(PauseMicrovmError::MicroVMInvalidState(
                StateError::MicroVMAlreadyRunning
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(PauseMicrovmError::MicroVMInvalidState(
                StateError::MicroVMIsNotRunning
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(PauseMicrovmError::MicroVMInvalidState(
                StateError::VcpusInvalidState
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::OpenSnapshotFile(
                snapshot::Error::InvalidSnapshotSize
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(PauseMicrovmError::SaveVcpuState(None)),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::SaveVmState(vstate::Error::VmGetIrqChip(
                io::Error::from_raw_os_error(0)
            ))),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::SerializeVcpu(
                snapshot::Error::InvalidFileType
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::SignalVcpu(vstate::Error::SignalVcpu(
                io::Error::from_raw_os_error(0)
            ))),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::StopVcpus(
                KillVcpusError::MicroVMInvalidState(StateError::MicroVMAlreadyRunning)
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::SyncHeader(
                snapshot::Error::InvalidFileType
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(PauseMicrovmError::SyncMemory(
                memory_model::GuestMemoryError::MemoryNotInitialized
            )),
            ErrorKind::Internal
        );
        assert_eq!(error_kind(PauseMicrovmError::VcpuPause), ErrorKind::User);

        // Test `ResumeMicrovmError` conversion.
        assert_eq!(
            error_kind(ResumeMicrovmError::DeserializeVcpu(
                snapshot::Error::InvalidFileType
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::MicroVMInvalidState(
                StateError::MicroVMAlreadyRunning
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::MicroVMInvalidState(
                StateError::MicroVMIsNotRunning
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::MicroVMInvalidState(
                StateError::VcpusInvalidState
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::OpenSnapshotFile(
                snapshot::Error::InvalidFileType
            )),
            ErrorKind::User
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::RestoreVmState(
                vstate::Error::VmSetIrqChip(io::Error::from_raw_os_error(0))
            )),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::RestoreVcpuState),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::SignalVcpu(vstate::Error::SignalVcpu(
                io::Error::from_raw_os_error(0)
            ))),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(ResumeMicrovmError::StartMicroVm(
                StartMicrovmError::DeviceManager
            )),
            ErrorKind::Internal
        );
        assert_eq!(error_kind(ResumeMicrovmError::VcpuResume), ErrorKind::User);
    }

    #[test]
    #[allow(clippy::cyclomatic_complexity)]
    fn test_error_messages() {
        // Enum `Error`
        assert_eq!(
            format!("{:?}", Error::ApiChannel),
            "ApiChannel: error receiving data from the API server"
        );
        assert_eq!(
            format!(
                "{:?}",
                Error::CreateLegacyDevice(device_manager::legacy::Error::EventFd(
                    io::Error::from_raw_os_error(42)
                ))
            ),
            format!(
                "Error creating legacy device: EventFd({:?})",
                io::Error::from_raw_os_error(42)
            )
        );
        assert_eq!(
            format!("{:?}", Error::EpollFd(io::Error::from_raw_os_error(42))),
            "Epoll fd error: No message of desired type (os error 42)"
        );
        assert_eq!(
            format!("{:?}", Error::EventFd(io::Error::from_raw_os_error(42))),
            "Event fd error: No message of desired type (os error 42)"
        );
        assert_eq!(
            format!("{:?}", Error::DeviceEventHandlerNotFound),
            "Device event handler not found. This might point to a guest device driver issue."
        );
        assert_eq!(
            format!("{:?}", Error::DeviceEventHandlerInvalidDowncast),
            "Device event handler couldn't be downcasted to expected type."
        );
        assert_eq!(
            format!("{:?}", Error::Kvm(io::Error::from_raw_os_error(42))),
            "Cannot open /dev/kvm. Error: No message of desired type (os error 42)"
        );
        assert_eq!(
            format!("{:?}", Error::KvmApiVersion(42)),
            "Bad KVM API version: 42"
        );
        assert_eq!(
            format!("{:?}", Error::KvmCap(Cap::Hlt)),
            "Missing KVM capability: Hlt"
        );
        assert_eq!(
            format!("{:?}", Error::Poll(io::Error::from_raw_os_error(42))),
            "Epoll wait failed: No message of desired type (os error 42)"
        );
        assert_eq!(
            format!("{:?}", Error::Serial(io::Error::from_raw_os_error(42))),
            format!(
                "Error writing to the serial console: {:?}",
                io::Error::from_raw_os_error(42)
            )
        );
        assert_eq!(
            format!("{:?}", Error::TimerFd(io::Error::from_raw_os_error(42))),
            "Error creating timer fd: No message of desired type (os error 42)"
        );
        assert_eq!(
            format!("{:?}", Error::Vm(vstate::Error::HTNotInitialized)),
            "Error opening VM fd: HTNotInitialized"
        );

        // Enum `ErrorKind`

        assert_ne!(ErrorKind::User, ErrorKind::Internal);
        assert_eq!(format!("{:?}", ErrorKind::User), "User");
        assert_eq!(format!("{:?}", ErrorKind::Internal), "Internal");

        // Enum VmmActionError

        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::BootSource(
                    ErrorKind::User,
                    BootSourceConfigError::InvalidKernelCommandLine
                )
            ),
            "BootSource(User, InvalidKernelCommandLine)"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::DriveConfig(
                    ErrorKind::User,
                    DriveError::BlockDevicePathAlreadyExists
                )
            ),
            "DriveConfig(User, BlockDevicePathAlreadyExists)"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::Logger(
                    ErrorKind::User,
                    LoggerConfigError::InitializationFailure(String::from("foobar"))
                )
            ),
            "Logger(User, InitializationFailure(\"foobar\"))"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::MachineConfig(ErrorKind::User, VmConfigError::InvalidMemorySize)
            ),
            "MachineConfig(User, InvalidMemorySize)"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::NetworkConfig(
                    ErrorKind::User,
                    NetworkInterfaceError::DeviceIdNotFound
                )
            ),
            "NetworkConfig(User, DeviceIdNotFound)"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::PauseMicrovm(
                    ErrorKind::Internal,
                    PauseMicrovmError::SaveVcpuState(None)
                )
            ),
            "PauseMicrovm(Internal, SaveVcpuState(None))"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::ResumeMicrovm(
                    ErrorKind::Internal,
                    ResumeMicrovmError::RestoreVcpuState
                )
            ),
            "ResumeMicrovm(Internal, RestoreVcpuState)"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::StartMicrovm(ErrorKind::User, StartMicrovmError::EventFd)
            ),
            "StartMicrovm(User, EventFd)"
        );
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::SendCtrlAltDel(
                    ErrorKind::User,
                    I8042DeviceError::InternalBufferFull
                )
            ),
            "SendCtrlAltDel(User, InternalBufferFull)"
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::SendCtrlAltDel(
                    ErrorKind::User,
                    I8042DeviceError::InternalBufferFull
                )
            ),
            I8042DeviceError::InternalBufferFull.to_string()
        );
        assert_eq!(
            VmmActionError::SendCtrlAltDel(ErrorKind::User, I8042DeviceError::InternalBufferFull)
                .kind(),
            &ErrorKind::User
        );
        #[cfg(feature = "vsock")]
        assert_eq!(
            format!(
                "{:?}",
                VmmActionError::VsockConfig(ErrorKind::User, VsockError::UpdateNotAllowedPostBoot)
            ),
            "VsockConfig(User, UpdateNotAllowedPostBoot)"
        );
    }

    #[test]
    fn test_display_errors() {
        assert_eq!(
            format!(
                "{}",
                VmmActionError::BootSource(
                    ErrorKind::User,
                    BootSourceConfigError::InvalidKernelCommandLine
                )
            ),
            "The kernel command line is invalid!"
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::DriveConfig(ErrorKind::User, DriveError::CannotOpenBlockDevice)
            ),
            "Cannot open block device. Invalid permission/path."
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::Logger(
                    ErrorKind::User,
                    LoggerConfigError::InitializationFailure("foo".to_string())
                )
            ),
            "foo"
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::MachineConfig(ErrorKind::User, VmConfigError::InvalidMemorySize)
            ),
            "The memory size (MiB) is invalid."
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::NetworkConfig(
                    ErrorKind::User,
                    NetworkInterfaceError::DeviceIdNotFound
                )
            ),
            "Invalid interface ID - not found."
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::PauseMicrovm(
                    ErrorKind::User,
                    PauseMicrovmError::SaveVcpuState(None)
                )
            ),
            "Failed to save vCPU state."
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::PauseMicrovm(
                    ErrorKind::User,
                    PauseMicrovmError::SaveVcpuState(Some(vstate::Error::VcpuCountNotInitialized))
                )
            ),
            "Failed to save vCPU state: VcpuCountNotInitialized"
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::ResumeMicrovm(
                    ErrorKind::User,
                    ResumeMicrovmError::RestoreVcpuState
                )
            ),
            "Failed to restore vCPU state."
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::StartMicrovm(ErrorKind::User, StartMicrovmError::DeviceManager)
            ),
            "The device manager was not configured."
        );
        assert_eq!(
            format!(
                "{}",
                VmmActionError::SendCtrlAltDel(
                    ErrorKind::User,
                    I8042DeviceError::KbdInterruptDisabled
                )
            ),
            "Keyboard interrupt disabled by guest driver."
        );
        #[cfg(feature = "vsock")]
        assert_eq!(
            format!(
                "{}",
                VmmActionError::VsockConfig(ErrorKind::User, VsockError::UpdateNotAllowedPostBoot)
            ),
            "The update operation is not allowed after boot."
        );
    }

    #[test]
    fn test_create_snapshot_file() {
        let mut vmm = create_vmm_object(InstanceState::Running);

        let snapshot = NamedTempFile::new().unwrap().into_temp_path();
        let snapshot_path = snapshot.to_str().unwrap();

        // Error case: no vCPUs.
        vmm.vm_config.vcpu_count = None;
        let res = vmm.create_snapshot_file(snapshot_path.to_string());
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res.err().unwrap()),
            "SnapshotBackingFile(MissingVcpuNum)"
        );

        vmm.vm_config.vcpu_count = Some(1);
        assert!(vmm.create_snapshot_file(snapshot_path.to_string()).is_ok());
        assert!(Path::new(snapshot_path).exists());
        remove_file(snapshot_path).unwrap();
    }
}
