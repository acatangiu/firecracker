// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::result;
use std::sync::{Arc, Mutex};

use crate::rpc_interface::resources::VmResourceStore;
use crate::rpc_interface::{VmmAction, VmmActionError, VmmData};
use crate::Vmm;
use arch::DeviceType;
use builder::StartMicrovmError;
use device_manager::mmio::MMIO_CFG_SPACE_OFF;
use devices::virtio::{Block, MmioTransport, Net, TYPE_BLOCK, TYPE_NET};
use logger::METRICS;
use polly::event_manager::EventManager;
use rpc_interface;
use rpc_interface::drive::DriveError;
use rpc_interface::machine_config::VmConfig;
use rpc_interface::net::{NetworkInterfaceError, NetworkInterfaceUpdateConfig};
use rpc_interface::rate_limiter::TokenBucketConfig;
use seccomp::BpfProgram;

/// Enables pre-boot setup and instantiation of a Firecracker VMM.
pub struct PrebootApiController<'a> {
    seccomp_filter: BpfProgram,
    firecracker_version: String,
    vm_resources: &'a mut VmResourceStore,
    event_manager: &'a mut EventManager,

    built_vmm: Option<Arc<Mutex<Vmm>>>,
}

impl<'a> PrebootApiController<'a> {
    /// Constructor for the PrebootApiController.
    pub fn new(
        seccomp_filter: BpfProgram,
        firecracker_version: String,
        vm_resources: &'a mut VmResourceStore,
        event_manager: &'a mut EventManager,
    ) -> PrebootApiController<'a> {
        PrebootApiController {
            seccomp_filter,
            firecracker_version,
            vm_resources,
            event_manager,
            built_vmm: None,
        }
    }

    /// Default implementation for the function that builds and starts a microVM.
    /// It takes two closures `recv_req` and `respond` as params which abstract away
    /// the message transport.
    ///
    /// Returns a populated `VmResources` object and a running `Vmm` object.
    pub fn build_microvm_from_requests<F, G>(
        seccomp_filter: BpfProgram,
        event_manager: &mut EventManager,
        firecracker_version: String,
        recv_req: F,
        respond: G,
    ) -> (VmResourceStore, Arc<Mutex<Vmm>>)
    where
        F: Fn() -> VmmAction,
        G: Fn(result::Result<VmmData, VmmActionError>),
    {
        let mut vm_resources = VmResourceStore::default();
        let mut preboot_controller = PrebootApiController::new(
            seccomp_filter,
            firecracker_version,
            &mut vm_resources,
            event_manager,
        );
        // Configure and start microVM through successive API calls.
        // Iterate through API calls to configure microVm.
        // The loop breaks when a microVM is successfully started, and a running Vmm is built.
        while preboot_controller.built_vmm.is_none() {
            // Get request, process it, send back the response.
            respond(preboot_controller.handle_preboot_request(recv_req()));
        }

        // Safe to unwrap because previous loop cannot end on None.
        let vmm = preboot_controller.built_vmm.unwrap();
        (vm_resources, vmm)
    }

    /// Handles the incoming preboot request and provides a response for it.
    /// Returns a built/running `Vmm` after handling a successful `StartMicroVm` request.
    pub fn handle_preboot_request(
        &mut self,
        request: VmmAction,
    ) -> result::Result<VmmData, VmmActionError> {
        use self::VmmAction::*;

        match request {
            // Supported operations allowed pre-boot.
            ConfigureBootSource(boot_source_body) => self
                .vm_resources
                .set_boot_source(boot_source_body)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::BootSource),
            ConfigureLogger(logger_cfg) => {
                rpc_interface::logger::init_logger(logger_cfg, &self.firecracker_version)
                    .map(|_| VmmData::Empty)
                    .map_err(VmmActionError::Logger)
            }
            ConfigureMetrics(metrics_cfg) => rpc_interface::metrics::init_metrics(metrics_cfg)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::Metrics),
            GetVmConfiguration => Ok(VmmData::MachineConfiguration(
                self.vm_resources.vm_config().clone(),
            )),
            InsertBlockDevice(block_device_config) => self
                .vm_resources
                .set_block_device(block_device_config)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::DriveConfig),
            InsertNetworkDevice(netif_body) => self
                .vm_resources
                .set_net_device(netif_body)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::NetworkConfig),
            SetVsockDevice(vsock_cfg) => self
                .vm_resources
                .set_vsock_device(vsock_cfg)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::VsockConfig),
            SetVmConfiguration(machine_config_body) => self
                .vm_resources
                .set_vm_config(&machine_config_body)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::MachineConfig),
            StartMicroVm => crate::builder::build_microvm(
                // FIXME: fix errors and remove unwrap.
                self.vm_resources.build_resources().unwrap(),
                &mut self.event_manager,
                &self.seccomp_filter,
            )
            .map(|vmm| {
                self.built_vmm = Some(vmm);
                VmmData::Empty
            })
            .map_err(VmmActionError::StartMicrovm),

            // Operations not allowed pre-boot.
            UpdateBlockDevicePath(_, _) | UpdateNetworkInterface(_) | FlushMetrics => {
                Err(VmmActionError::OperationNotSupportedPreBoot)
            }
            #[cfg(target_arch = "x86_64")]
            SendCtrlAltDel => Err(VmmActionError::OperationNotSupportedPreBoot),
        }
    }
}

/// Shorthand result type for external VMM commands.
pub type ActionResult = result::Result<(), VmmActionError>;

/// Enables RPC interaction with a running Firecracker VMM.
pub struct RuntimeApiController {
    vmm: Arc<Mutex<Vmm>>,
    vm_config: VmConfig,
}

impl RuntimeApiController {
    /// Handles the incoming runtime `VmmAction` request and provides a response for it.
    pub fn handle_request(
        &mut self,
        request: VmmAction,
    ) -> result::Result<VmmData, VmmActionError> {
        use self::VmmAction::*;
        match request {
            // Supported operations allowed post-boot.
            FlushMetrics => self.flush_metrics().map(|_| VmmData::Empty),
            GetVmConfiguration => Ok(VmmData::MachineConfiguration(self.vm_config.clone())),
            #[cfg(target_arch = "x86_64")]
            SendCtrlAltDel => self.send_ctrl_alt_del().map(|_| VmmData::Empty),
            UpdateBlockDevicePath(drive_id, path_on_host) => self
                .update_block_device_path(&drive_id, path_on_host)
                .map(|_| VmmData::Empty)
                .map_err(VmmActionError::DriveConfig),
            UpdateNetworkInterface(netif_update) => self
                .update_net_rate_limiters(netif_update)
                .map(|_| VmmData::Empty),

            // Operations not allowed post-boot.
            ConfigureBootSource(_)
            | ConfigureLogger(_)
            | ConfigureMetrics(_)
            | InsertBlockDevice(_)
            | InsertNetworkDevice(_)
            | SetVsockDevice(_)
            | SetVmConfiguration(_) => Err(VmmActionError::OperationNotSupportedPostBoot),
            StartMicroVm => Err(VmmActionError::StartMicrovm(
                StartMicrovmError::MicroVMAlreadyRunning,
            )),
        }
    }

    /// Creates a new `RuntimeApiController`.
    pub fn new(vm_config: VmConfig, vmm: Arc<Mutex<Vmm>>) -> Self {
        Self { vm_config, vmm }
    }

    /// Write the metrics on user demand (flush). We use the word `flush` here to highlight the fact
    /// that the metrics will be written immediately.
    /// Defer to inner Vmm. We'll move to a variant where the Vmm simply exposes functionality like
    /// getting the dirty pages, and then we'll have the metrics flushing logic entirely on the outside.
    fn flush_metrics(&mut self) -> ActionResult {
        // FIXME: we're losing the bool saying whether metrics were actually written.
        METRICS
            .write()
            .map(|_| ())
            .map_err(crate::Error::Metrics)
            .map_err(VmmActionError::InternalVmm)
    }

    /// Injects CTRL+ALT+DEL keystroke combo to the inner Vmm (if present).
    #[cfg(target_arch = "x86_64")]
    fn send_ctrl_alt_del(&mut self) -> ActionResult {
        self.vmm
            .lock()
            .unwrap()
            .send_ctrl_alt_del()
            .map_err(VmmActionError::InternalVmm)
    }

    /// Updates the path of the host file backing the emulated block device with id `drive_id`.
    /// We update the disk image on the device and its virtio configuration.
    fn update_block_device_path<P: AsRef<Path>>(
        &mut self,
        drive_id: &str,
        disk_image_path: P,
    ) -> result::Result<(), DriveError> {
        if let Some(busdev) = self
            .vmm
            .lock()
            .unwrap()
            .get_bus_device(DeviceType::Virtio(TYPE_BLOCK), drive_id)
        {
            let new_size;
            // Call the update_disk_image() handler on Block. Release the lock when done.
            {
                let virtio_dev = busdev
                    .lock()
                    .expect("Poisoned device lock")
                    .as_any()
                    // Only MmioTransport implements BusDevice at this point.
                    .downcast_ref::<MmioTransport>()
                    .expect("Unexpected BusDevice type")
                    // Here we get a *new* clone of Arc<Mutex<dyn VirtioDevice>>.
                    .device();

                // We need this bound to a variable so that it lives as long as the 'block' ref.
                let mut locked_device = virtio_dev.lock().expect("Poisoned device lock");
                // Get a '&mut Block' ref from the above MutexGuard<dyn VirtioDevice>.
                let block = locked_device
                    .as_mut_any()
                    // We know this is a block device from the HashMap.
                    .downcast_mut::<Block>()
                    .expect("Unexpected VirtioDevice type");

                // Try to open the file specified by path_on_host using the permissions of the block_device.
                let mut disk_image = OpenOptions::new()
                    .read(true)
                    .write(!block.is_read_only())
                    .open(disk_image_path)
                    .map_err(DriveError::OpenBlockDevice)?;

                // Use seek() instead of stat() (std::fs::Metadata) to support block devices.
                new_size = disk_image
                    .seek(SeekFrom::End(0))
                    .map_err(|_| DriveError::BlockDeviceUpdateFailed)?;
                // Return cursor to the start of the file.
                disk_image
                    .seek(SeekFrom::Start(0))
                    .map_err(|_| DriveError::BlockDeviceUpdateFailed)?;

                // Now we have a Block, so call its update handler.
                block
                    .update_disk_image(disk_image)
                    .map_err(|_| DriveError::BlockDeviceUpdateFailed)?;
            }

            // Update the virtio config space and kick the driver to pick up the changes.
            let new_cfg = devices::virtio::block::device::build_config_space(new_size);
            let mut locked_dev = busdev.lock().expect("Poisoned device lock");
            locked_dev.write(MMIO_CFG_SPACE_OFF, &new_cfg[..]);
            locked_dev
                .interrupt(devices::virtio::VIRTIO_MMIO_INT_CONFIG)
                .map_err(|_| DriveError::BlockDeviceUpdateFailed)?;

            Ok(())
        } else {
            Err(DriveError::InvalidBlockDeviceID)
        }
    }

    /// Updates configuration for an emulated net device as described in `new_cfg`.
    fn update_net_rate_limiters(&mut self, new_cfg: NetworkInterfaceUpdateConfig) -> ActionResult {
        if let Some(busdev) = self
            .vmm
            .lock()
            .unwrap()
            .get_bus_device(DeviceType::Virtio(TYPE_NET), &new_cfg.iface_id)
        {
            let virtio_device = busdev
                .lock()
                .expect("Poisoned device lock")
                .as_any()
                .downcast_ref::<MmioTransport>()
                // Only MmioTransport implements BusDevice at this point.
                .expect("Unexpected BusDevice type")
                .device();

            macro_rules! get_handler_arg {
                ($rate_limiter: ident, $metric: ident) => {{
                    new_cfg
                        .$rate_limiter
                        .map(|rl| rl.$metric.map(TokenBucketConfig::into))
                        .unwrap_or(None)
                }};
            }

            virtio_device
                .lock()
                .expect("Poisoned device lock")
                .as_mut_any()
                .downcast_mut::<Net>()
                .unwrap()
                .patch_rate_limiters(
                    get_handler_arg!(rx_rate_limiter, bandwidth),
                    get_handler_arg!(rx_rate_limiter, ops),
                    get_handler_arg!(tx_rate_limiter, bandwidth),
                    get_handler_arg!(tx_rate_limiter, ops),
                );
        } else {
            return Err(VmmActionError::NetworkConfig(
                NetworkInterfaceError::DeviceIdNotFound,
            ));
        }

        Ok(())
    }
}
