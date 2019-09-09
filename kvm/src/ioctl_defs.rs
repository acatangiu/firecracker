// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Declares necessary ioctls specific to their platform.

use kvm_bindings::*;

// Ioctls for /dev/kvm.

ioctl_io_nr!(KVM_GET_API_VERSION, KVMIO, 0x00);
ioctl_io_nr!(KVM_CREATE_VM, KVMIO, 0x01);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iowr_nr!(KVM_GET_MSR_INDEX_LIST, KVMIO, 0x02, kvm_msr_list);
ioctl_io_nr!(KVM_CHECK_EXTENSION, KVMIO, 0x03);
ioctl_io_nr!(KVM_GET_VCPU_MMAP_SIZE, KVMIO, 0x04);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iowr_nr!(KVM_GET_SUPPORTED_CPUID, KVMIO, 0x05, kvm_cpuid2);
/* Available with KVM_CAP_GET_MSR_FEATURES */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iowr_nr!(KVM_GET_MSR_FEATURE_INDEX_LIST, KVMIO, 0x0a, kvm_msr_list);

// Ioctls for VM fds.

ioctl_io_nr!(KVM_CREATE_VCPU, KVMIO, 0x41);
ioctl_iow_nr!(KVM_GET_DIRTY_LOG, KVMIO, 0x42, kvm_dirty_log);
ioctl_iow_nr!(
    KVM_SET_USER_MEMORY_REGION,
    KVMIO,
    0x46,
    kvm_userspace_memory_region
);
/* Available with KVM_CAP_SET_TSS_ADDR */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_io_nr!(KVM_SET_TSS_ADDR, KVMIO, 0x47);
/* Available with KVM_CAP_IRQCHIP */
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "s390"
))]
ioctl_io_nr!(KVM_CREATE_IRQCHIP, KVMIO, 0x60);
/* Available with KVM_CAP_IRQFD */
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "s390"
))]
ioctl_iow_nr!(KVM_IRQFD, KVMIO, 0x76, kvm_irqfd);
/* Available with KVM_CAP_PIT2 */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_CREATE_PIT2, KVMIO, 0x77, kvm_pit_config);
/* Available with KVM_CAP_IOEVENTFD */
ioctl_iow_nr!(KVM_IOEVENTFD, KVMIO, 0x79, kvm_ioeventfd);
/* Available with KVM_CAP_IRQCHIP */
ioctl_iow_nr!(KVM_IRQ_LINE, KVMIO, 0x61, kvm_irq_level);
/* Available with KVM_CAP_IRQCHIP */
ioctl_iowr_nr!(KVM_GET_IRQCHIP, KVMIO, 0x62, kvm_irqchip);
/* Available with KVM_CAP_IRQCHIP */
ioctl_ior_nr!(KVM_SET_IRQCHIP, KVMIO, 0x63, kvm_irqchip);
/* Available with KVM_CAP_ADJUST_CLOCK */
ioctl_iow_nr!(KVM_SET_CLOCK, KVMIO, 0x7b, kvm_clock_data);
/* Available with KVM_CAP_ADJUST_CLOCK */
ioctl_ior_nr!(KVM_GET_CLOCK, KVMIO, 0x7c, kvm_clock_data);
/* Available with KVM_CAP_PIT_STATE2 */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_ior_nr!(KVM_GET_PIT2, KVMIO, 0x9f, kvm_pit_state2);
/* Available with KVM_CAP_PIT_STATE2 */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_PIT2, KVMIO, 0xa0, kvm_pit_state2);

// Ioctls for VCPU fds.

ioctl_io_nr!(KVM_RUN, KVMIO, 0x80);
#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
ioctl_ior_nr!(KVM_GET_REGS, KVMIO, 0x81, kvm_regs);
#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
ioctl_iow_nr!(KVM_SET_REGS, KVMIO, 0x82, kvm_regs);
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "powerpc",
    target_arch = "powerpc64"
))]
ioctl_ior_nr!(KVM_GET_SREGS, KVMIO, 0x83, kvm_sregs);
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "powerpc",
    target_arch = "powerpc64"
))]
ioctl_iow_nr!(KVM_SET_SREGS, KVMIO, 0x84, kvm_sregs);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iowr_nr!(KVM_GET_MSRS, KVMIO, 0x88, kvm_msrs);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_MSRS, KVMIO, 0x89, kvm_msrs);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_ior_nr!(KVM_GET_FPU, KVMIO, 0x8c, kvm_fpu);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_FPU, KVMIO, 0x8d, kvm_fpu);
/* Available with KVM_CAP_IRQCHIP */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_ior_nr!(KVM_GET_LAPIC, KVMIO, 0x8e, kvm_lapic_state);
/* Available with KVM_CAP_IRQCHIP */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_LAPIC, KVMIO, 0x8f, kvm_lapic_state);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_CPUID2, KVMIO, 0x90, kvm_cpuid2);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iowr_nr!(KVM_GET_CPUID2, KVMIO, 0x91, kvm_cpuid2);
/* Available with KVM_CAP_MP_STATE */
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "s390"
))]
ioctl_ior_nr!(KVM_GET_MP_STATE, KVMIO, 0x98, kvm_mp_state);
/* Available with KVM_CAP_MP_STATE */
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "s390"
))]
ioctl_iow_nr!(KVM_SET_MP_STATE, KVMIO, 0x99, kvm_mp_state);
/* Available with KVM_CAP_VCPU_EVENTS */
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64"
))]
ioctl_ior_nr!(KVM_GET_VCPU_EVENTS, KVMIO, 0x9f, kvm_vcpu_events);
/* Available with KVM_CAP_VCPU_EVENTS */
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64"
))]
ioctl_iow_nr!(KVM_SET_VCPU_EVENTS, KVMIO, 0xa0, kvm_vcpu_events);
/* Available with KVM_CAP_DEBUGREGS */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_ior_nr!(KVM_GET_DEBUGREGS, KVMIO, 0xa1, kvm_debugregs);
/* Available with KVM_CAP_DEBUGREGS */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_DEBUGREGS, KVMIO, 0xa2, kvm_debugregs);
/* Available with KVM_CAP_XSAVE */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_ior_nr!(KVM_GET_XSAVE, KVMIO, 0xa4, kvm_xsave);
/* Available with KVM_CAP_XSAVE */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_XSAVE, KVMIO, 0xa5, kvm_xsave);
/* Available with KVM_CAP_XCRS */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_ior_nr!(KVM_GET_XCRS, KVMIO, 0xa6, kvm_xcrs);
/* Available with KVM_CAP_XCRS */
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
ioctl_iow_nr!(KVM_SET_XCRS, KVMIO, 0xa7, kvm_xcrs);
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
ioctl_iow_nr!(KVM_SET_ONE_REG, KVMIO, 0xac, kvm_one_reg);
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
ioctl_iow_nr!(KVM_ARM_VCPU_INIT, KVMIO, 0xae, kvm_vcpu_init);
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
ioctl_ior_nr!(KVM_ARM_PREFERRED_TARGET, KVMIO, 0xaf, kvm_vcpu_init);

// Device ioctls.

ioctl_iowr_nr!(KVM_CREATE_DEVICE, KVMIO, 0xe0, kvm_create_device);
ioctl_iow_nr!(KVM_SET_DEVICE_ATTR, KVMIO, 0xe1, kvm_device_attr);

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::unix::io::FromRawFd;

    use libc::{c_char, open, O_RDWR};

    use super::*;
    use sys_util::{ioctl, ioctl_with_val};

    const KVM_PATH: &str = "/dev/kvm\0";

    #[test]
    fn get_version() {
        let sys_fd = unsafe { open(KVM_PATH.as_ptr() as *const c_char, O_RDWR) };
        assert!(sys_fd >= 0);

        let ret = unsafe { ioctl(&File::from_raw_fd(sys_fd), KVM_GET_API_VERSION()) };
        assert_eq!(ret as u32, KVM_API_VERSION);
    }

    #[test]
    fn create_vm_fd() {
        let sys_fd = unsafe { open(KVM_PATH.as_ptr() as *const c_char, O_RDWR) };
        assert!(sys_fd >= 0);

        let vm_fd = unsafe { ioctl(&File::from_raw_fd(sys_fd), KVM_CREATE_VM()) };
        assert!(vm_fd >= 0);
    }

    #[test]
    fn check_vm_extension() {
        let sys_fd = unsafe { open(KVM_PATH.as_ptr() as *const c_char, O_RDWR) };
        assert!(sys_fd >= 0);

        let has_user_memory = unsafe {
            ioctl_with_val(
                &File::from_raw_fd(sys_fd),
                KVM_CHECK_EXTENSION(),
                KVM_CAP_USER_MEMORY.into(),
            )
        };
        assert_eq!(has_user_memory, 1);
    }
}