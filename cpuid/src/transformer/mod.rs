// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod amd;
pub mod common;
pub mod intel;

use brand_string::BrandString;
use brand_string::Reg as BsReg;
use common::get_vendor_id;
use kvm::CpuId;

pub use kvm_bindings::kvm_cpuid_entry2;

/// Structure containing the specifications of the VM
///
pub struct VmSpec {
    /// The vendor id of the CPU
    cpu_vendor_id: [u8; 12],
    /// The id of the current logical cpu in the range [0..cpu_count].
    cpu_id: u8,
    /// The total number of logical cpus.
    cpu_count: u8,
    /// Specifies whether hyper-threading is enabled.
    ht_enabled: bool,
    /// The desired brand string for the guest.
    brand_string: BrandString,
}

impl VmSpec {
    /// Creates a new instance of VmSpec with the specified parameters
    /// The brand string is deduced from the vendor_id
    ///
    pub fn new(cpu_id: u8, cpu_count: u8, ht_enabled: bool) -> Result<VmSpec, Error> {
        let cpu_vendor_id = get_vendor_id().map_err(Error::InternalError)?;

        Ok(VmSpec {
            cpu_vendor_id,
            cpu_id,
            cpu_count,
            ht_enabled,
            brand_string: BrandString::from_vendor_id(&cpu_vendor_id),
        })
    }

    /// Returns an immutable reference to cpu_vendor_id
    ///
    pub fn cpu_vendor_id(&self) -> &[u8; 12] {
        &self.cpu_vendor_id
    }
}

/// Errors associated with processing the CPUID leaves.
#[derive(Debug, Clone)]
pub enum Error {
    /// The maximum number of addressable logical CPUs cannot be stored in an `u8`.
    VcpuCountOverflow,
    /// The max size has been exceeded
    SizeLimitExceeded,
    /// A call to an internal helper method failed
    InternalError(super::common::Error),
}

pub type EntryTransformerFn =
    fn(entry: &mut kvm_cpuid_entry2, vm_spec: &VmSpec) -> Result<(), Error>;

/// Generic trait that provides methods for transforming the cpuid
///
pub trait CpuidTransformer {
    /// Trait main function. It processes the cpuid and makes the desired transformations.
    /// The default logic can be overwritten if needed. For example see `AmdCpuidTransformer`.
    ///
    fn process_cpuid(&self, cpuid: &mut CpuId, vm_spec: &VmSpec) -> Result<(), Error> {
        self.process_entries(cpuid, vm_spec)
    }

    /// Iterates through all the cpuid entries and calls the associated transformer for each one.
    ///
    fn process_entries(&self, cpuid: &mut CpuId, vm_spec: &VmSpec) -> Result<(), Error> {
        for entry in cpuid.as_mut_entries_slice().iter_mut() {
            let maybe_transformer_fn = self.entry_transformer_fn(entry);

            if let Some(transformer_fn) = maybe_transformer_fn {
                transformer_fn(entry, vm_spec)?;
            }
        }

        Ok(())
    }

    /// Gets the associated transformer for a cpuid entry
    ///
    fn entry_transformer_fn(&self, _entry: &mut kvm_cpuid_entry2) -> Option<EntryTransformerFn> {
        None
    }
}
