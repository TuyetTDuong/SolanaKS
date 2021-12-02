use crate::{alloc, BpfError};
use alloc::Alloc;
use solana_program_runtime::{
    ic_msg,
    instruction_processor::InstructionProcessor,
    invoke_context::{ComputeMeter, InvokeContext, Logger},
    stable_log,
};
use solana_rbpf::{
    aligned_memory::AlignedMemory,
    ebpf,
    error::EbpfError,
    memory_region::{AccessType, MemoryMapping},
    question_mark,
    vm::{EbpfVm, SyscallObject, SyscallRegistry},
};
#[allow(deprecated)]
use solana_sdk::sysvar::fees::Fees;
use solana_sdk::{
    account::{AccountSharedData, ReadableAccount, WritableAccount},
    account_info::AccountInfo,
    blake3, bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable,
    clock::Clock,
    entrypoint::{MAX_PERMITTED_DATA_INCREASE, SUCCESS},
    epoch_schedule::EpochSchedule,
    feature_set::{
        blake3_syscall_enabled, demote_program_write_locks, disable_fees_sysvar,
        do_support_realloc, libsecp256k1_0_5_upgrade_enabled,
        prevent_calling_precompiles_as_programs, return_data_syscall_enabled,
        secp256k1_recover_syscall_enabled, sol_log_data_syscall_enabled,
    },
    hash::{Hasher, HASH_BYTES},
    instruction::{AccountMeta, Instruction, InstructionError},
    keccak,
    message::Message,
    native_loader,
    precompiles::is_precompile,
    program::MAX_RETURN_DATA,
    pubkey::{Pubkey, PubkeyError, MAX_SEEDS, MAX_SEED_LEN},
    rent::Rent,
    secp256k1_recover::{
        Secp256k1RecoverError, SECP256K1_PUBLIC_KEY_LENGTH, SECP256K1_SIGNATURE_LENGTH,
    },
    sysvar::{self, Sysvar, SysvarId},
};
use std::{
    alloc::Layout,
    cell::{Ref, RefCell, RefMut},
    mem::{align_of, size_of},
    rc::Rc,
    slice::from_raw_parts_mut,
    str::{from_utf8, Utf8Error},
};
use thiserror::Error as ThisError;

/// Maximum signers
pub const MAX_SIGNERS: usize = 16;

/// Error definitions
#[derive(Debug, ThisError, PartialEq)]
pub enum SyscallError {
    #[error("{0}: {1:?}")]
    InvalidString(Utf8Error, Vec<u8>),
    #[error("BPF program panicked")]
    Abort,
    #[error("BPF program Panicked in {0} at {1}:{2}")]
    Panic(String, u64, u64),
    #[error("Cannot borrow invoke context")]
    InvokeContextBorrowFailed,
    #[error("Malformed signer seed: {0}: {1:?}")]
    MalformedSignerSeed(Utf8Error, Vec<u8>),
    #[error("Could not create program address with signer seeds: {0}")]
    BadSeeds(PubkeyError),
    #[error("Program {0} not supported by inner instructions")]
    ProgramNotSupported(Pubkey),
    #[error("{0}")]
    InstructionError(InstructionError),
    #[error("Unaligned pointer")]
    UnalignedPointer,
    #[error("Too many signers")]
    TooManySigners,
    #[error("Instruction passed to inner instruction is too large ({0} > {1})")]
    InstructionTooLarge(usize, usize),
    #[error("Too many accounts passed to inner instruction")]
    TooManyAccounts,
    #[error("Overlapping copy")]
    CopyOverlapping,
    #[error("Return data too large ({0} > {1})")]
    ReturnDataTooLarge(u64, u64),
}
impl From<SyscallError> for EbpfError<BpfError> {
    fn from(error: SyscallError) -> Self {
        EbpfError::UserError(error.into())
    }
}

trait SyscallConsume {
    fn consume(&mut self, amount: u64) -> Result<(), EbpfError<BpfError>>;
}
impl SyscallConsume for Rc<RefCell<dyn ComputeMeter>> {
    fn consume(&mut self, amount: u64) -> Result<(), EbpfError<BpfError>> {
        self.try_borrow_mut()
            .map_err(|_| SyscallError::InvokeContextBorrowFailed)?
            .consume(amount)
            .map_err(SyscallError::InstructionError)?;
        Ok(())
    }
}

/// Program heap allocators are intended to allocate/free from a given
/// chunk of memory.  The specific allocator implementation is
/// selectable at build-time.
/// Only one allocator is currently supported

/// Simple bump allocator, never frees
use crate::allocator_bump::BpfAllocator;

pub fn register_syscalls(
    invoke_context: &mut dyn InvokeContext,
) -> Result<SyscallRegistry, EbpfError<BpfError>> {
    let mut syscall_registry = SyscallRegistry::default();

    syscall_registry.register_syscall_by_name(b"abort", SyscallAbort::call)?;
    syscall_registry.register_syscall_by_name(b"sol_panic_", SyscallPanic::call)?;
    syscall_registry.register_syscall_by_name(b"sol_log_", SyscallLog::call)?;
    syscall_registry.register_syscall_by_name(b"sol_log_64_", SyscallLogU64::call)?;

    syscall_registry
        .register_syscall_by_name(b"sol_log_compute_units_", SyscallLogBpfComputeUnits::call)?;

    syscall_registry.register_syscall_by_name(b"sol_log_pubkey", SyscallLogPubkey::call)?;

    syscall_registry.register_syscall_by_name(
        b"sol_create_program_address",
        SyscallCreateProgramAddress::call,
    )?;
    syscall_registry.register_syscall_by_name(
        b"sol_try_find_program_address",
        SyscallTryFindProgramAddress::call,
    )?;

    syscall_registry.register_syscall_by_name(b"sol_sha256", SyscallSha256::call)?;
    syscall_registry.register_syscall_by_name(b"sol_keccak256", SyscallKeccak256::call)?;

    if invoke_context.is_feature_active(&secp256k1_recover_syscall_enabled::id()) {
        syscall_registry
            .register_syscall_by_name(b"sol_secp256k1_recover", SyscallSecp256k1Recover::call)?;
    }

    if invoke_context.is_feature_active(&blake3_syscall_enabled::id()) {
        syscall_registry.register_syscall_by_name(b"sol_blake3", SyscallBlake3::call)?;
    }

    syscall_registry
        .register_syscall_by_name(b"sol_get_clock_sysvar", SyscallGetClockSysvar::call)?;
    syscall_registry.register_syscall_by_name(
        b"sol_get_epoch_schedule_sysvar",
        SyscallGetEpochScheduleSysvar::call,
    )?;
    if !invoke_context.is_feature_active(&disable_fees_sysvar::id()) {
        syscall_registry
            .register_syscall_by_name(b"sol_get_fees_sysvar", SyscallGetFeesSysvar::call)?;
    }
    syscall_registry
        .register_syscall_by_name(b"sol_get_rent_sysvar", SyscallGetRentSysvar::call)?;

    syscall_registry.register_syscall_by_name(b"sol_memcpy_", SyscallMemcpy::call)?;
    syscall_registry.register_syscall_by_name(b"sol_memmove_", SyscallMemmove::call)?;
    syscall_registry.register_syscall_by_name(b"sol_memcmp_", SyscallMemcmp::call)?;
    syscall_registry.register_syscall_by_name(b"sol_memset_", SyscallMemset::call)?;

    // Cross-program invocation syscalls
    syscall_registry
        .register_syscall_by_name(b"sol_invoke_signed_c", SyscallInvokeSignedC::call)?;
    syscall_registry
        .register_syscall_by_name(b"sol_invoke_signed_rust", SyscallInvokeSignedRust::call)?;

    // Memory allocator
    syscall_registry.register_syscall_by_name(b"sol_alloc_free_", SyscallAllocFree::call)?;

    // Return data
    if invoke_context.is_feature_active(&return_data_syscall_enabled::id()) {
        syscall_registry
            .register_syscall_by_name(b"sol_set_return_data", SyscallSetReturnData::call)?;
        syscall_registry
            .register_syscall_by_name(b"sol_get_return_data", SyscallGetReturnData::call)?;
    }

    // Log data
    if invoke_context.is_feature_active(&sol_log_data_syscall_enabled::id()) {
        syscall_registry.register_syscall_by_name(b"sol_log_data", SyscallLogData::call)?;
    }

    Ok(syscall_registry)
}

macro_rules! bind_feature_gated_syscall_context_object {
    ($vm:expr, $is_feature_active:expr, $syscall_context_object:expr $(,)?) => {
        if $is_feature_active {
            match $vm.bind_syscall_context_object($syscall_context_object, None) {
                Err(EbpfError::SyscallNotRegistered(_)) | Ok(()) => {}
                Err(err) => {
                    return Err(err);
                }
            }
        }
    };
}

pub fn bind_syscall_context_objects<'a>(
    loader_id: &'a Pubkey,
    vm: &mut EbpfVm<'a, BpfError, crate::ThisInstructionMeter>,
    invoke_context: &'a mut dyn InvokeContext,
    heap: AlignedMemory,
    orig_data_lens: &'a [usize],
) -> Result<(), EbpfError<BpfError>> {
    let compute_budget = invoke_context.get_compute_budget();

    // Syscall functions common across languages

    vm.bind_syscall_context_object(Box::new(SyscallAbort {}), None)?;
    vm.bind_syscall_context_object(
        Box::new(SyscallPanic {
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallLog {
            compute_meter: invoke_context.get_compute_meter(),
            logger: invoke_context.get_logger(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallLogU64 {
            cost: compute_budget.log_64_units,
            compute_meter: invoke_context.get_compute_meter(),
            logger: invoke_context.get_logger(),
        }),
        None,
    )?;

    vm.bind_syscall_context_object(
        Box::new(SyscallLogBpfComputeUnits {
            cost: 0,
            compute_meter: invoke_context.get_compute_meter(),
            logger: invoke_context.get_logger(),
        }),
        None,
    )?;

    vm.bind_syscall_context_object(
        Box::new(SyscallLogPubkey {
            cost: compute_budget.log_pubkey_units,
            compute_meter: invoke_context.get_compute_meter(),
            logger: invoke_context.get_logger(),
            loader_id,
        }),
        None,
    )?;

    vm.bind_syscall_context_object(
        Box::new(SyscallCreateProgramAddress {
            cost: compute_budget.create_program_address_units,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallTryFindProgramAddress {
            cost: compute_budget.create_program_address_units,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;

    vm.bind_syscall_context_object(
        Box::new(SyscallSha256 {
            sha256_base_cost: compute_budget.sha256_base_cost,
            sha256_byte_cost: compute_budget.sha256_byte_cost,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;

    vm.bind_syscall_context_object(
        Box::new(SyscallKeccak256 {
            base_cost: compute_budget.sha256_base_cost,
            byte_cost: compute_budget.sha256_byte_cost,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;

    vm.bind_syscall_context_object(
        Box::new(SyscallMemcpy {
            cost: invoke_context.get_compute_budget().cpi_bytes_per_unit,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallMemmove {
            cost: invoke_context.get_compute_budget().cpi_bytes_per_unit,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallMemcmp {
            cost: invoke_context.get_compute_budget().cpi_bytes_per_unit,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallMemset {
            cost: invoke_context.get_compute_budget().cpi_bytes_per_unit,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
        None,
    )?;

    bind_feature_gated_syscall_context_object!(
        vm,
        invoke_context.is_feature_active(&blake3_syscall_enabled::id()),
        Box::new(SyscallBlake3 {
            base_cost: compute_budget.sha256_base_cost,
            byte_cost: compute_budget.sha256_byte_cost,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
        }),
    );

    bind_feature_gated_syscall_context_object!(
        vm,
        invoke_context.is_feature_active(&secp256k1_recover_syscall_enabled::id()),
        Box::new(SyscallSecp256k1Recover {
            cost: compute_budget.secp256k1_recover_cost,
            compute_meter: invoke_context.get_compute_meter(),
            loader_id,
            libsecp256k1_0_5_upgrade_enabled: invoke_context
                .is_feature_active(&libsecp256k1_0_5_upgrade_enabled::id()),
        }),
    );

    let is_fee_sysvar_via_syscall_active =
        !invoke_context.is_feature_active(&disable_fees_sysvar::id());
    let is_return_data_syscall_active =
        invoke_context.is_feature_active(&return_data_syscall_enabled::id());
    let is_sol_log_data_syscall_active =
        invoke_context.is_feature_active(&sol_log_data_syscall_enabled::id());

    let invoke_context = Rc::new(RefCell::new(invoke_context));

    vm.bind_syscall_context_object(
        Box::new(SyscallGetClockSysvar {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallGetEpochScheduleSysvar {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
        None,
    )?;
    bind_feature_gated_syscall_context_object!(
        vm,
        is_fee_sysvar_via_syscall_active,
        Box::new(SyscallGetFeesSysvar {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
    );
    vm.bind_syscall_context_object(
        Box::new(SyscallGetRentSysvar {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
        None,
    )?;

    // Return data
    bind_feature_gated_syscall_context_object!(
        vm,
        is_return_data_syscall_active,
        Box::new(SyscallSetReturnData {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
    );

    bind_feature_gated_syscall_context_object!(
        vm,
        is_return_data_syscall_active,
        Box::new(SyscallGetReturnData {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
    );

    // sol_log_data
    bind_feature_gated_syscall_context_object!(
        vm,
        is_sol_log_data_syscall_active,
        Box::new(SyscallLogData {
            invoke_context: invoke_context.clone(),
            loader_id,
        }),
    );

    // Cross-program invocation syscalls
    vm.bind_syscall_context_object(
        Box::new(SyscallInvokeSignedC {
            invoke_context: invoke_context.clone(),
            orig_data_lens,
            loader_id,
        }),
        None,
    )?;
    vm.bind_syscall_context_object(
        Box::new(SyscallInvokeSignedRust {
            invoke_context: invoke_context.clone(),
            orig_data_lens,
            loader_id,
        }),
        None,
    )?;

    // Memory allocator
    vm.bind_syscall_context_object(
        Box::new(SyscallAllocFree {
            aligned: *loader_id != bpf_loader_deprecated::id(),
            allocator: BpfAllocator::new(heap, ebpf::MM_HEAP_START),
        }),
        None,
    )?;

    Ok(())
}

fn translate(
    memory_mapping: &MemoryMapping,
    access_type: AccessType,
    vm_addr: u64,
    len: u64,
) -> Result<u64, EbpfError<BpfError>> {
    memory_mapping.map::<BpfError>(access_type, vm_addr, len)
}

fn translate_type_inner<'a, T>(
    memory_mapping: &MemoryMapping,
    access_type: AccessType,
    vm_addr: u64,
    loader_id: &Pubkey,
) -> Result<&'a mut T, EbpfError<BpfError>> {
    let host_addr = translate(memory_mapping, access_type, vm_addr, size_of::<T>() as u64)?;

    if loader_id != &bpf_loader_deprecated::id()
        && (host_addr as *mut T).align_offset(align_of::<T>()) != 0
    {
        return Err(SyscallError::UnalignedPointer.into());
    }
    Ok(unsafe { &mut *(host_addr as *mut T) })
}
fn translate_type_mut<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    loader_id: &Pubkey,
) -> Result<&'a mut T, EbpfError<BpfError>> {
    translate_type_inner::<T>(memory_mapping, AccessType::Store, vm_addr, loader_id)
}
fn translate_type<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    loader_id: &Pubkey,
) -> Result<&'a T, EbpfError<BpfError>> {
    translate_type_inner::<T>(memory_mapping, AccessType::Load, vm_addr, loader_id)
        .map(|value| &*value)
}

fn translate_slice_inner<'a, T>(
    memory_mapping: &MemoryMapping,
    access_type: AccessType,
    vm_addr: u64,
    len: u64,
    loader_id: &Pubkey,
) -> Result<&'a mut [T], EbpfError<BpfError>> {
    if len == 0 {
        return Ok(&mut []);
    }

    let host_addr = translate(
        memory_mapping,
        access_type,
        vm_addr,
        len.saturating_mul(size_of::<T>() as u64),
    )?;

    if loader_id != &bpf_loader_deprecated::id()
        && (host_addr as *mut T).align_offset(align_of::<T>()) != 0
    {
        return Err(SyscallError::UnalignedPointer.into());
    }
    Ok(unsafe { from_raw_parts_mut(host_addr as *mut T, len as usize) })
}
fn translate_slice_mut<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    loader_id: &Pubkey,
) -> Result<&'a mut [T], EbpfError<BpfError>> {
    translate_slice_inner::<T>(memory_mapping, AccessType::Store, vm_addr, len, loader_id)
}
fn translate_slice<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    loader_id: &Pubkey,
) -> Result<&'a [T], EbpfError<BpfError>> {
    translate_slice_inner::<T>(memory_mapping, AccessType::Load, vm_addr, len, loader_id)
        .map(|value| &*value)
}

/// Take a virtual pointer to a string (points to BPF VM memory space), translate it
/// pass it to a user-defined work function
fn translate_string_and_do(
    memory_mapping: &MemoryMapping,
    addr: u64,
    len: u64,
    loader_id: &Pubkey,
    work: &mut dyn FnMut(&str) -> Result<u64, EbpfError<BpfError>>,
) -> Result<u64, EbpfError<BpfError>> {
    let buf = translate_slice::<u8>(memory_mapping, addr, len, loader_id)?;
    let i = match buf.iter().position(|byte| *byte == 0) {
        Some(i) => i,
        None => len as usize,
    };
    match from_utf8(&buf[..i]) {
        Ok(message) => work(message),
        Err(err) => Err(SyscallError::InvalidString(err, buf[..i].to_vec()).into()),
    }
}

/// Abort syscall functions, called when the BPF program calls `abort()`
/// LLVM will insert calls to `abort()` if it detects an untenable situation,
/// `abort()` is not intended to be called explicitly by the program.
/// Causes the BPF program to be halted immediately
pub struct SyscallAbort {}
impl SyscallObject<BpfError> for SyscallAbort {
    fn call(
        &mut self,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = Err(SyscallError::Abort.into());
    }
}

/// Panic syscall function, called when the BPF program calls 'sol_panic_()`
/// Causes the BPF program to be halted immediately
/// Log a user's info message
pub struct SyscallPanic<'a> {
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallPanic<'a> {
    fn call(
        &mut self,
        file: u64,
        len: u64,
        line: u64,
        column: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(len), result);
        *result = translate_string_and_do(
            memory_mapping,
            file,
            len,
            self.loader_id,
            &mut |string: &str| Err(SyscallError::Panic(string.to_string(), line, column).into()),
        );
    }
}

/// Log a user's info message
pub struct SyscallLog<'a> {
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    logger: Rc<RefCell<dyn Logger>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallLog<'a> {
    fn call(
        &mut self,
        addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(len), result);
        question_mark!(
            translate_string_and_do(
                memory_mapping,
                addr,
                len,
                self.loader_id,
                &mut |string: &str| {
                    stable_log::program_log(&self.logger, string);
                    Ok(0)
                },
            ),
            result
        );
        *result = Ok(0);
    }
}

/// Log 5 64-bit values
pub struct SyscallLogU64 {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    logger: Rc<RefCell<dyn Logger>>,
}
impl SyscallObject<BpfError> for SyscallLogU64 {
    fn call(
        &mut self,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
        arg5: u64,
        _memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.cost), result);
        stable_log::program_log(
            &self.logger,
            &format!(
                "{:#x}, {:#x}, {:#x}, {:#x}, {:#x}",
                arg1, arg2, arg3, arg4, arg5
            ),
        );
        *result = Ok(0);
    }
}

/// Log current compute consumption
pub struct SyscallLogBpfComputeUnits {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    logger: Rc<RefCell<dyn Logger>>,
}
impl SyscallObject<BpfError> for SyscallLogBpfComputeUnits {
    fn call(
        &mut self,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.cost), result);
        let logger = question_mark!(
            self.logger
                .try_borrow_mut()
                .map_err(|_| SyscallError::InvokeContextBorrowFailed),
            result
        );
        if logger.log_enabled() {
            logger.log(&format!(
                "Program consumption: {} units remaining",
                self.compute_meter.borrow().get_remaining()
            ));
        }
        *result = Ok(0);
    }
}

/// Log 5 64-bit values
pub struct SyscallLogPubkey<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    logger: Rc<RefCell<dyn Logger>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallLogPubkey<'a> {
    fn call(
        &mut self,
        pubkey_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.cost), result);
        let pubkey = question_mark!(
            translate_type::<Pubkey>(memory_mapping, pubkey_addr, self.loader_id,),
            result
        );
        stable_log::program_log(&self.logger, &pubkey.to_string());
        *result = Ok(0);
    }
}

/// Dynamic memory allocation syscall called when the BPF program calls
/// `sol_alloc_free_()`.  The allocator is expected to allocate/free
/// from/to a given chunk of memory and enforce size restrictions.  The
/// memory chunk is given to the allocator during allocator creation and
/// information about that memory (start address and size) is passed
/// to the VM to use for enforcement.
pub struct SyscallAllocFree {
    aligned: bool,
    allocator: BpfAllocator,
}
impl SyscallObject<BpfError> for SyscallAllocFree {
    fn call(
        &mut self,
        size: u64,
        free_addr: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        let align = if self.aligned {
            align_of::<u128>()
        } else {
            align_of::<u8>()
        };
        let layout = match Layout::from_size_align(size as usize, align) {
            Ok(layout) => layout,
            Err(_) => {
                *result = Ok(0);
                return;
            }
        };
        *result = if free_addr == 0 {
            match self.allocator.alloc(layout) {
                Ok(addr) => Ok(addr as u64),
                Err(_) => Ok(0),
            }
        } else {
            self.allocator.dealloc(free_addr, layout);
            Ok(0)
        };
    }
}

fn translate_and_check_program_address_inputs<'a>(
    seeds_addr: u64,
    seeds_len: u64,
    program_id_addr: u64,
    memory_mapping: &MemoryMapping,
    loader_id: &Pubkey,
) -> Result<(Vec<&'a [u8]>, &'a Pubkey), EbpfError<BpfError>> {
    let untranslated_seeds =
        translate_slice::<&[&u8]>(memory_mapping, seeds_addr, seeds_len, loader_id)?;
    if untranslated_seeds.len() > MAX_SEEDS {
        return Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into());
    }
    let seeds = untranslated_seeds
        .iter()
        .map(|untranslated_seed| {
            if untranslated_seed.len() > MAX_SEED_LEN {
                return Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into());
            }
            translate_slice::<u8>(
                memory_mapping,
                untranslated_seed.as_ptr() as *const _ as u64,
                untranslated_seed.len() as u64,
                loader_id,
            )
        })
        .collect::<Result<Vec<_>, EbpfError<BpfError>>>()?;
    let program_id = translate_type::<Pubkey>(memory_mapping, program_id_addr, loader_id)?;
    Ok((seeds, program_id))
}

/// Create a program address
struct SyscallCreateProgramAddress<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallCreateProgramAddress<'a> {
    fn call(
        &mut self,
        seeds_addr: u64,
        seeds_len: u64,
        program_id_addr: u64,
        address_addr: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.cost), result);

        let (seeds, program_id) = question_mark!(
            translate_and_check_program_address_inputs(
                seeds_addr,
                seeds_len,
                program_id_addr,
                memory_mapping,
                self.loader_id,
            ),
            result
        );

        let new_address = match Pubkey::create_program_address(&seeds, program_id) {
            Ok(address) => address,
            Err(_) => {
                *result = Ok(1);
                return;
            }
        };
        let address = question_mark!(
            translate_slice_mut::<u8>(memory_mapping, address_addr, 32, self.loader_id,),
            result
        );
        address.copy_from_slice(new_address.as_ref());
        *result = Ok(0);
    }
}

/// Create a program address
struct SyscallTryFindProgramAddress<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallTryFindProgramAddress<'a> {
    fn call(
        &mut self,
        seeds_addr: u64,
        seeds_len: u64,
        program_id_addr: u64,
        address_addr: u64,
        bump_seed_addr: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.cost), result);

        let (seeds, program_id) = question_mark!(
            translate_and_check_program_address_inputs(
                seeds_addr,
                seeds_len,
                program_id_addr,
                memory_mapping,
                self.loader_id,
            ),
            result
        );

        let mut bump_seed = [std::u8::MAX];
        for _ in 0..std::u8::MAX {
            {
                let mut seeds_with_bump = seeds.to_vec();
                seeds_with_bump.push(&bump_seed);

                if let Ok(new_address) =
                    Pubkey::create_program_address(&seeds_with_bump, program_id)
                {
                    let bump_seed_ref = question_mark!(
                        translate_type_mut::<u8>(memory_mapping, bump_seed_addr, self.loader_id,),
                        result
                    );
                    let address = question_mark!(
                        translate_slice_mut::<u8>(memory_mapping, address_addr, 32, self.loader_id,),
                        result
                    );
                    *bump_seed_ref = bump_seed[0];
                    address.copy_from_slice(new_address.as_ref());
                    *result = Ok(0);
                    return;
                }
            }
            bump_seed[0] -= 1;
            question_mark!(self.compute_meter.consume(self.cost), result);
        }
        *result = Ok(1);
    }
}

/// SHA256
pub struct SyscallSha256<'a> {
    sha256_base_cost: u64,
    sha256_byte_cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallSha256<'a> {
    fn call(
        &mut self,
        vals_addr: u64,
        vals_len: u64,
        result_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.sha256_base_cost), result);
        let hash_result = question_mark!(
            translate_slice_mut::<u8>(
                memory_mapping,
                result_addr,
                HASH_BYTES as u64,
                self.loader_id,
            ),
            result
        );
        let mut hasher = Hasher::default();
        if vals_len > 0 {
            let vals = question_mark!(
                translate_slice::<&[u8]>(memory_mapping, vals_addr, vals_len, self.loader_id,),
                result
            );
            for val in vals.iter() {
                let bytes = question_mark!(
                    translate_slice::<u8>(
                        memory_mapping,
                        val.as_ptr() as u64,
                        val.len() as u64,
                        self.loader_id,
                    ),
                    result
                );
                question_mark!(
                    self.compute_meter
                        .consume(self.sha256_byte_cost * (val.len() as u64 / 2)),
                    result
                );
                hasher.hash(bytes);
            }
        }
        hash_result.copy_from_slice(&hasher.result().to_bytes());
        *result = Ok(0);
    }
}

fn get_sysvar<T: std::fmt::Debug + Sysvar + SysvarId>(
    id: &Pubkey,
    var_addr: u64,
    loader_id: &Pubkey,
    memory_mapping: &MemoryMapping,
    invoke_context: Rc<RefCell<&mut dyn InvokeContext>>,
) -> Result<u64, EbpfError<BpfError>> {
    let invoke_context = invoke_context
        .try_borrow()
        .map_err(|_| SyscallError::InvokeContextBorrowFailed)?;

    invoke_context
        .get_compute_meter()
        .consume(invoke_context.get_compute_budget().sysvar_base_cost + size_of::<T>() as u64)?;
    let var = translate_type_mut::<T>(memory_mapping, var_addr, loader_id)?;

    *var = solana_program_runtime::invoke_context::get_sysvar::<T>(*invoke_context, id)
        .map_err(SyscallError::InstructionError)?;

    Ok(SUCCESS)
}

/// Get a Clock sysvar
struct SyscallGetClockSysvar<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallGetClockSysvar<'a> {
    fn call(
        &mut self,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = get_sysvar::<Clock>(
            &sysvar::clock::id(),
            var_addr,
            self.loader_id,
            memory_mapping,
            self.invoke_context.clone(),
        );
    }
}
/// Get a EpochSchedule sysvar
struct SyscallGetEpochScheduleSysvar<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallGetEpochScheduleSysvar<'a> {
    fn call(
        &mut self,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = get_sysvar::<EpochSchedule>(
            &sysvar::epoch_schedule::id(),
            var_addr,
            self.loader_id,
            memory_mapping,
            self.invoke_context.clone(),
        );
    }
}
/// Get a Fees sysvar
struct SyscallGetFeesSysvar<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
#[allow(deprecated)]
impl<'a> SyscallObject<BpfError> for SyscallGetFeesSysvar<'a> {
    fn call(
        &mut self,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = get_sysvar::<Fees>(
            &sysvar::fees::id(),
            var_addr,
            self.loader_id,
            memory_mapping,
            self.invoke_context.clone(),
        );
    }
}
/// Get a Rent sysvar
struct SyscallGetRentSysvar<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallGetRentSysvar<'a> {
    fn call(
        &mut self,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = get_sysvar::<Rent>(
            &sysvar::rent::id(),
            var_addr,
            self.loader_id,
            memory_mapping,
            self.invoke_context.clone(),
        );
    }
}

// Keccak256
pub struct SyscallKeccak256<'a> {
    base_cost: u64,
    byte_cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallKeccak256<'a> {
    fn call(
        &mut self,
        vals_addr: u64,
        vals_len: u64,
        result_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.base_cost), result);
        let hash_result = question_mark!(
            translate_slice_mut::<u8>(
                memory_mapping,
                result_addr,
                keccak::HASH_BYTES as u64,
                self.loader_id,
            ),
            result
        );
        let mut hasher = keccak::Hasher::default();
        if vals_len > 0 {
            let vals = question_mark!(
                translate_slice::<&[u8]>(memory_mapping, vals_addr, vals_len, self.loader_id),
                result
            );
            for val in vals.iter() {
                let bytes = question_mark!(
                    translate_slice::<u8>(
                        memory_mapping,
                        val.as_ptr() as u64,
                        val.len() as u64,
                        self.loader_id,
                    ),
                    result
                );
                question_mark!(
                    self.compute_meter
                        .consume(self.byte_cost * (val.len() as u64 / 2)),
                    result
                );
                hasher.hash(bytes);
            }
        }
        hash_result.copy_from_slice(&hasher.result().to_bytes());
        *result = Ok(0);
    }
}

fn check_overlapping(src_addr: u64, dst_addr: u64, n: u64) -> bool {
    (src_addr <= dst_addr && src_addr + n > dst_addr)
        || (dst_addr <= src_addr && dst_addr + n > src_addr)
}

/// memcpy
pub struct SyscallMemcpy<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallMemcpy<'a> {
    fn call(
        &mut self,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        if check_overlapping(src_addr, dst_addr, n) {
            *result = Err(SyscallError::CopyOverlapping.into());
            return;
        }

        question_mark!(self.compute_meter.consume(n / self.cost), result);
        let dst = question_mark!(
            translate_slice_mut::<u8>(memory_mapping, dst_addr, n, self.loader_id),
            result
        );
        let src = question_mark!(
            translate_slice::<u8>(memory_mapping, src_addr, n, self.loader_id),
            result
        );
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), n as usize);
        }
        *result = Ok(0);
    }
}
/// memmove
pub struct SyscallMemmove<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallMemmove<'a> {
    fn call(
        &mut self,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(n / self.cost), result);
        let dst = question_mark!(
            translate_slice_mut::<u8>(memory_mapping, dst_addr, n, self.loader_id),
            result
        );
        let src = question_mark!(
            translate_slice::<u8>(memory_mapping, src_addr, n, self.loader_id),
            result
        );
        unsafe {
            std::ptr::copy(src.as_ptr(), dst.as_mut_ptr(), n as usize);
        }
        *result = Ok(0);
    }
}
/// memcmp
pub struct SyscallMemcmp<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallMemcmp<'a> {
    fn call(
        &mut self,
        s1_addr: u64,
        s2_addr: u64,
        n: u64,
        cmp_result_addr: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(n / self.cost), result);
        let s1 = question_mark!(
            translate_slice::<u8>(memory_mapping, s1_addr, n, self.loader_id),
            result
        );
        let s2 = question_mark!(
            translate_slice::<u8>(memory_mapping, s2_addr, n, self.loader_id),
            result
        );
        let cmp_result = question_mark!(
            translate_type_mut::<i32>(memory_mapping, cmp_result_addr, self.loader_id),
            result
        );
        let mut i = 0;
        while i < n as usize {
            let a = s1[i];
            let b = s2[i];
            if a != b {
                *cmp_result = a as i32 - b as i32;
                *result = Ok(0);
                return;
            }
            i += 1;
        }
        *cmp_result = 0;
        *result = Ok(0);
    }
}
/// memset
pub struct SyscallMemset<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallMemset<'a> {
    fn call(
        &mut self,
        s_addr: u64,
        c: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(n / self.cost), result);
        let s = question_mark!(
            translate_slice_mut::<u8>(memory_mapping, s_addr, n, self.loader_id),
            result
        );
        for val in s.iter_mut().take(n as usize) {
            *val = c as u8;
        }
        *result = Ok(0);
    }
}

/// secp256k1_recover
pub struct SyscallSecp256k1Recover<'a> {
    cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
    libsecp256k1_0_5_upgrade_enabled: bool,
}

impl<'a> SyscallObject<BpfError> for SyscallSecp256k1Recover<'a> {
    fn call(
        &mut self,
        hash_addr: u64,
        recovery_id_val: u64,
        signature_addr: u64,
        result_addr: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.cost), result);

        let hash = question_mark!(
            translate_slice::<u8>(
                memory_mapping,
                hash_addr,
                keccak::HASH_BYTES as u64,
                self.loader_id,
            ),
            result
        );
        let signature = question_mark!(
            translate_slice::<u8>(
                memory_mapping,
                signature_addr,
                SECP256K1_SIGNATURE_LENGTH as u64,
                self.loader_id,
            ),
            result
        );
        let secp256k1_recover_result = question_mark!(
            translate_slice_mut::<u8>(
                memory_mapping,
                result_addr,
                SECP256K1_PUBLIC_KEY_LENGTH as u64,
                self.loader_id,
            ),
            result
        );

        let message = match libsecp256k1::Message::parse_slice(hash) {
            Ok(msg) => msg,
            Err(_) => {
                *result = Ok(Secp256k1RecoverError::InvalidHash.into());
                return;
            }
        };
        let recovery_id = match libsecp256k1::RecoveryId::parse(recovery_id_val as u8) {
            Ok(id) => id,
            Err(_) => {
                *result = Ok(Secp256k1RecoverError::InvalidRecoveryId.into());
                return;
            }
        };
        let sig_parse_result = if self.libsecp256k1_0_5_upgrade_enabled {
            libsecp256k1::Signature::parse_standard_slice(signature)
        } else {
            libsecp256k1::Signature::parse_overflowing_slice(signature)
        };

        let signature = match sig_parse_result {
            Ok(sig) => sig,
            Err(_) => {
                *result = Ok(Secp256k1RecoverError::InvalidSignature.into());
                return;
            }
        };

        let public_key = match libsecp256k1::recover(&message, &signature, &recovery_id) {
            Ok(key) => key.serialize(),
            Err(_) => {
                *result = Ok(Secp256k1RecoverError::InvalidSignature.into());
                return;
            }
        };

        secp256k1_recover_result.copy_from_slice(&public_key[1..65]);
        *result = Ok(SUCCESS);
    }
}

// Blake3
pub struct SyscallBlake3<'a> {
    base_cost: u64,
    byte_cost: u64,
    compute_meter: Rc<RefCell<dyn ComputeMeter>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallBlake3<'a> {
    fn call(
        &mut self,
        vals_addr: u64,
        vals_len: u64,
        result_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        question_mark!(self.compute_meter.consume(self.base_cost), result);
        let hash_result = question_mark!(
            translate_slice_mut::<u8>(
                memory_mapping,
                result_addr,
                blake3::HASH_BYTES as u64,
                self.loader_id,
            ),
            result
        );
        let mut hasher = blake3::Hasher::default();
        if vals_len > 0 {
            let vals = question_mark!(
                translate_slice::<&[u8]>(memory_mapping, vals_addr, vals_len, self.loader_id),
                result
            );
            for val in vals.iter() {
                let bytes = question_mark!(
                    translate_slice::<u8>(
                        memory_mapping,
                        val.as_ptr() as u64,
                        val.len() as u64,
                        self.loader_id,
                    ),
                    result
                );
                question_mark!(
                    self.compute_meter
                        .consume(self.byte_cost * (val.len() as u64 / 2)),
                    result
                );
                hasher.hash(bytes);
            }
        }
        hash_result.copy_from_slice(&hasher.result().to_bytes());
        *result = Ok(0);
    }
}

// Cross-program invocation syscalls

struct CallerAccount<'a> {
    lamports: &'a mut u64,
    owner: &'a mut Pubkey,
    original_data_len: usize,
    data: &'a mut [u8],
    vm_data_addr: u64,
    ref_to_len_in_vm: &'a mut u64,
    serialized_len_ptr: &'a mut u64,
    executable: bool,
    rent_epoch: u64,
}
type TranslatedAccounts<'a> = (
    Vec<usize>,
    Vec<(Rc<RefCell<AccountSharedData>>, Option<CallerAccount<'a>>)>,
);

/// Implemented by language specific data structure translators
trait SyscallInvokeSigned<'a> {
    fn get_context_mut(&self) -> Result<RefMut<&'a mut dyn InvokeContext>, EbpfError<BpfError>>;
    fn get_context(&self) -> Result<Ref<&'a mut dyn InvokeContext>, EbpfError<BpfError>>;
    fn translate_instruction(
        &self,
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut dyn InvokeContext,
    ) -> Result<Instruction, EbpfError<BpfError>>;
    fn translate_accounts(
        &self,
        message: &Message,
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut dyn InvokeContext,
    ) -> Result<TranslatedAccounts<'a>, EbpfError<BpfError>>;
    fn translate_signers(
        &self,
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
    ) -> Result<Vec<Pubkey>, EbpfError<BpfError>>;
}

/// Cross-program invocation called from Rust
pub struct SyscallInvokeSignedRust<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    orig_data_lens: &'a [usize],
    loader_id: &'a Pubkey,
}
impl<'a> SyscallInvokeSigned<'a> for SyscallInvokeSignedRust<'a> {
    fn get_context_mut(&self) -> Result<RefMut<&'a mut dyn InvokeContext>, EbpfError<BpfError>> {
        self.invoke_context
            .try_borrow_mut()
            .map_err(|_| SyscallError::InvokeContextBorrowFailed.into())
    }
    fn get_context(&self) -> Result<Ref<&'a mut dyn InvokeContext>, EbpfError<BpfError>> {
        self.invoke_context
            .try_borrow()
            .map_err(|_| SyscallError::InvokeContextBorrowFailed.into())
    }
    fn translate_instruction(
        &self,
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut dyn InvokeContext,
    ) -> Result<Instruction, EbpfError<BpfError>> {
        let ix = translate_type::<Instruction>(memory_mapping, addr, self.loader_id)?;

        check_instruction_size(ix.accounts.len(), ix.data.len(), invoke_context)?;

        let accounts = translate_slice::<AccountMeta>(
            memory_mapping,
            ix.accounts.as_ptr() as u64,
            ix.accounts.len() as u64,
            self.loader_id,
        )?
        .to_vec();
        let data = translate_slice::<u8>(
            memory_mapping,
            ix.data.as_ptr() as u64,
            ix.data.len() as u64,
            self.loader_id,
        )?
        .to_vec();
        Ok(Instruction {
            program_id: ix.program_id,
            accounts,
            data,
        })
    }

    fn translate_accounts(
        &self,
        message: &Message,
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut dyn InvokeContext,
    ) -> Result<TranslatedAccounts<'a>, EbpfError<BpfError>> {
        let account_infos = translate_slice::<AccountInfo>(
            memory_mapping,
            account_infos_addr,
            account_infos_len,
            self.loader_id,
        )?;
        check_account_infos(account_infos.len(), invoke_context)?;
        let account_info_keys = account_infos
            .iter()
            .map(|account_info| {
                translate_type::<Pubkey>(
                    memory_mapping,
                    account_info.key as *const _ as u64,
                    self.loader_id,
                )
            })
            .collect::<Result<Vec<_>, EbpfError<BpfError>>>()?;

        let translate = |account_info: &AccountInfo, invoke_context: &dyn InvokeContext| {
            // Translate the account from user space

            let lamports = {
                // Double translate lamports out of RefCell
                let ptr = translate_type::<u64>(
                    memory_mapping,
                    account_info.lamports.as_ptr() as u64,
                    self.loader_id,
                )?;
                translate_type_mut::<u64>(memory_mapping, *ptr, self.loader_id)?
            };
            let owner = translate_type_mut::<Pubkey>(
                memory_mapping,
                account_info.owner as *const _ as u64,
                self.loader_id,
            )?;

            let (data, vm_data_addr, ref_to_len_in_vm, serialized_len_ptr) = {
                // Double translate data out of RefCell
                let data = *translate_type::<&[u8]>(
                    memory_mapping,
                    account_info.data.as_ptr() as *const _ as u64,
                    self.loader_id,
                )?;

                invoke_context.get_compute_meter().consume(
                    data.len() as u64 / invoke_context.get_compute_budget().cpi_bytes_per_unit,
                )?;

                let translated = translate(
                    memory_mapping,
                    AccessType::Store,
                    unsafe { (account_info.data.as_ptr() as *const u64).offset(1) as u64 },
                    8,
                )? as *mut u64;
                let ref_to_len_in_vm = unsafe { &mut *translated };
                let ref_of_len_in_input_buffer = unsafe { data.as_ptr().offset(-8) };
                let serialized_len_ptr = translate_type_mut::<u64>(
                    memory_mapping,
                    ref_of_len_in_input_buffer as *const _ as u64,
                    self.loader_id,
                )?;
                let vm_data_addr = data.as_ptr() as u64;
                (
                    translate_slice_mut::<u8>(
                        memory_mapping,
                        vm_data_addr,
                        data.len() as u64,
                        self.loader_id,
                    )?,
                    vm_data_addr,
                    ref_to_len_in_vm,
                    serialized_len_ptr,
                )
            };

            Ok(CallerAccount {
                lamports,
                owner,
                original_data_len: 0, // set later
                data,
                vm_data_addr,
                ref_to_len_in_vm,
                serialized_len_ptr,
                executable: account_info.executable,
                rent_epoch: account_info.rent_epoch,
            })
        };

        get_translated_accounts(
            message,
            &account_info_keys,
            account_infos,
            invoke_context,
            self.orig_data_lens,
            translate,
        )
    }

    fn translate_signers(
        &self,
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
    ) -> Result<Vec<Pubkey>, EbpfError<BpfError>> {
        let mut signers = Vec::new();
        if signers_seeds_len > 0 {
            let signers_seeds = translate_slice::<&[&[u8]]>(
                memory_mapping,
                signers_seeds_addr,
                signers_seeds_len,
                self.loader_id,
            )?;
            if signers_seeds.len() > MAX_SIGNERS {
                return Err(SyscallError::TooManySigners.into());
            }
            for signer_seeds in signers_seeds.iter() {
                let untranslated_seeds = translate_slice::<&[u8]>(
                    memory_mapping,
                    signer_seeds.as_ptr() as *const _ as u64,
                    signer_seeds.len() as u64,
                    self.loader_id,
                )?;
                if untranslated_seeds.len() > MAX_SEEDS {
                    return Err(SyscallError::InstructionError(
                        InstructionError::MaxSeedLengthExceeded,
                    )
                    .into());
                }
                let seeds = untranslated_seeds
                    .iter()
                    .map(|untranslated_seed| {
                        translate_slice::<u8>(
                            memory_mapping,
                            untranslated_seed.as_ptr() as *const _ as u64,
                            untranslated_seed.len() as u64,
                            self.loader_id,
                        )
                    })
                    .collect::<Result<Vec<_>, EbpfError<BpfError>>>()?;
                let signer = Pubkey::create_program_address(&seeds, program_id)
                    .map_err(SyscallError::BadSeeds)?;
                signers.push(signer);
            }
            Ok(signers)
        } else {
            Ok(vec![])
        }
    }
}
impl<'a> SyscallObject<BpfError> for SyscallInvokeSignedRust<'a> {
    fn call(
        &mut self,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = call(
            self,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        );
    }
}

/// Rust representation of C's SolInstruction
#[derive(Debug)]
struct SolInstruction {
    program_id_addr: u64,
    accounts_addr: u64,
    accounts_len: usize,
    data_addr: u64,
    data_len: usize,
}

/// Rust representation of C's SolAccountMeta
#[derive(Debug)]
struct SolAccountMeta {
    pubkey_addr: u64,
    is_writable: bool,
    is_signer: bool,
}

/// Rust representation of C's SolAccountInfo
#[derive(Debug)]
struct SolAccountInfo {
    key_addr: u64,
    lamports_addr: u64,
    data_len: u64,
    data_addr: u64,
    owner_addr: u64,
    rent_epoch: u64,
    #[allow(dead_code)]
    is_signer: bool,
    #[allow(dead_code)]
    is_writable: bool,
    executable: bool,
}

/// Rust representation of C's SolSignerSeed
#[derive(Debug)]
struct SolSignerSeedC {
    addr: u64,
    len: u64,
}

/// Rust representation of C's SolSignerSeeds
#[derive(Debug)]
struct SolSignerSeedsC {
    #[allow(dead_code)]
    addr: u64,
    #[allow(dead_code)]
    len: u64,
}

/// Cross-program invocation called from C
pub struct SyscallInvokeSignedC<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    orig_data_lens: &'a [usize],
    loader_id: &'a Pubkey,
}
impl<'a> SyscallInvokeSigned<'a> for SyscallInvokeSignedC<'a> {
    fn get_context_mut(&self) -> Result<RefMut<&'a mut dyn InvokeContext>, EbpfError<BpfError>> {
        self.invoke_context
            .try_borrow_mut()
            .map_err(|_| SyscallError::InvokeContextBorrowFailed.into())
    }
    fn get_context(&self) -> Result<Ref<&'a mut dyn InvokeContext>, EbpfError<BpfError>> {
        self.invoke_context
            .try_borrow()
            .map_err(|_| SyscallError::InvokeContextBorrowFailed.into())
    }

    fn translate_instruction(
        &self,
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut dyn InvokeContext,
    ) -> Result<Instruction, EbpfError<BpfError>> {
        let ix_c = translate_type::<SolInstruction>(memory_mapping, addr, self.loader_id)?;

        check_instruction_size(ix_c.accounts_len, ix_c.data_len, invoke_context)?;
        let program_id =
            translate_type::<Pubkey>(memory_mapping, ix_c.program_id_addr, self.loader_id)?;
        let meta_cs = translate_slice::<SolAccountMeta>(
            memory_mapping,
            ix_c.accounts_addr,
            ix_c.accounts_len as u64,
            self.loader_id,
        )?;
        let data = translate_slice::<u8>(
            memory_mapping,
            ix_c.data_addr,
            ix_c.data_len as u64,
            self.loader_id,
        )?
        .to_vec();
        let accounts = meta_cs
            .iter()
            .map(|meta_c| {
                let pubkey =
                    translate_type::<Pubkey>(memory_mapping, meta_c.pubkey_addr, self.loader_id)?;
                Ok(AccountMeta {
                    pubkey: *pubkey,
                    is_signer: meta_c.is_signer,
                    is_writable: meta_c.is_writable,
                })
            })
            .collect::<Result<Vec<AccountMeta>, EbpfError<BpfError>>>()?;

        Ok(Instruction {
            program_id: *program_id,
            accounts,
            data,
        })
    }

    fn translate_accounts(
        &self,
        message: &Message,
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut dyn InvokeContext,
    ) -> Result<TranslatedAccounts<'a>, EbpfError<BpfError>> {
        let account_infos = translate_slice::<SolAccountInfo>(
            memory_mapping,
            account_infos_addr,
            account_infos_len,
            self.loader_id,
        )?;
        check_account_infos(account_infos.len(), invoke_context)?;
        let account_info_keys = account_infos
            .iter()
            .map(|account_info| {
                translate_type::<Pubkey>(memory_mapping, account_info.key_addr, self.loader_id)
            })
            .collect::<Result<Vec<_>, EbpfError<BpfError>>>()?;

        let translate = |account_info: &SolAccountInfo, invoke_context: &dyn InvokeContext| {
            // Translate the account from user space

            let lamports = translate_type_mut::<u64>(
                memory_mapping,
                account_info.lamports_addr,
                self.loader_id,
            )?;
            let owner = translate_type_mut::<Pubkey>(
                memory_mapping,
                account_info.owner_addr,
                self.loader_id,
            )?;
            let vm_data_addr = account_info.data_addr;

            invoke_context.get_compute_meter().consume(
                account_info.data_len / invoke_context.get_compute_budget().cpi_bytes_per_unit,
            )?;

            let data = translate_slice_mut::<u8>(
                memory_mapping,
                vm_data_addr,
                account_info.data_len,
                self.loader_id,
            )?;

            let first_info_addr = &account_infos[0] as *const _ as u64;
            let addr = &account_info.data_len as *const u64 as u64;
            let vm_addr = account_infos_addr + (addr - first_info_addr);
            let _ = translate(
                memory_mapping,
                AccessType::Store,
                vm_addr,
                size_of::<u64>() as u64,
            )?;
            let ref_to_len_in_vm = unsafe { &mut *(addr as *mut u64) };

            let ref_of_len_in_input_buffer =
                unsafe { (account_info.data_addr as *mut u8).offset(-8) };
            let serialized_len_ptr = translate_type_mut::<u64>(
                memory_mapping,
                ref_of_len_in_input_buffer as *const _ as u64,
                self.loader_id,
            )?;

            Ok(CallerAccount {
                lamports,
                owner,
                original_data_len: 0, // set later
                data,
                vm_data_addr,
                ref_to_len_in_vm,
                serialized_len_ptr,
                executable: account_info.executable,
                rent_epoch: account_info.rent_epoch,
            })
        };

        get_translated_accounts(
            message,
            &account_info_keys,
            account_infos,
            invoke_context,
            self.orig_data_lens,
            translate,
        )
    }

    fn translate_signers(
        &self,
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
    ) -> Result<Vec<Pubkey>, EbpfError<BpfError>> {
        if signers_seeds_len > 0 {
            let signers_seeds = translate_slice::<SolSignerSeedC>(
                memory_mapping,
                signers_seeds_addr,
                signers_seeds_len,
                self.loader_id,
            )?;
            if signers_seeds.len() > MAX_SIGNERS {
                return Err(SyscallError::TooManySigners.into());
            }
            Ok(signers_seeds
                .iter()
                .map(|signer_seeds| {
                    let seeds = translate_slice::<SolSignerSeedC>(
                        memory_mapping,
                        signer_seeds.addr,
                        signer_seeds.len,
                        self.loader_id,
                    )?;
                    if seeds.len() > MAX_SEEDS {
                        return Err(SyscallError::InstructionError(
                            InstructionError::MaxSeedLengthExceeded,
                        )
                        .into());
                    }
                    let seeds_bytes = seeds
                        .iter()
                        .map(|seed| {
                            translate_slice::<u8>(
                                memory_mapping,
                                seed.addr,
                                seed.len,
                                self.loader_id,
                            )
                        })
                        .collect::<Result<Vec<_>, EbpfError<BpfError>>>()?;
                    Pubkey::create_program_address(&seeds_bytes, program_id)
                        .map_err(|err| SyscallError::BadSeeds(err).into())
                })
                .collect::<Result<Vec<_>, EbpfError<BpfError>>>()?)
        } else {
            Ok(vec![])
        }
    }
}
impl<'a> SyscallObject<BpfError> for SyscallInvokeSignedC<'a> {
    fn call(
        &mut self,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        *result = call(
            self,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        );
    }
}

fn get_translated_accounts<'a, T, F>(
    message: &Message,
    account_info_keys: &[&Pubkey],
    account_infos: &[T],
    invoke_context: &mut dyn InvokeContext,
    orig_data_lens: &[usize],
    do_translate: F,
) -> Result<TranslatedAccounts<'a>, EbpfError<BpfError>>
where
    F: Fn(&T, &dyn InvokeContext) -> Result<CallerAccount<'a>, EbpfError<BpfError>>,
{
    let demote_program_write_locks =
        invoke_context.is_feature_active(&demote_program_write_locks::id());
    let keyed_accounts = invoke_context
        .get_keyed_accounts()
        .map_err(SyscallError::InstructionError)?;
    let mut account_indices = Vec::with_capacity(message.account_keys.len());
    let mut accounts = Vec::with_capacity(message.account_keys.len());
    for (i, account_key) in message.account_keys.iter().enumerate() {
        if let Some((account_index, account)) = invoke_context.get_account(account_key) {
            if i == message.instructions[0].program_id_index as usize
                || account.borrow().executable()
            {
                // Use the known account
                account_indices.push(account_index);
                accounts.push((account, None));
                continue;
            } else if let Some(caller_account_index) =
                account_info_keys.iter().position(|key| *key == account_key)
            {
                let mut caller_account =
                    do_translate(&account_infos[caller_account_index], invoke_context)?;
                {
                    let mut account = account.borrow_mut();
                    account.copy_into_owner_from_slice(caller_account.owner.as_ref());
                    account.set_data_from_slice(caller_account.data);
                    account.set_lamports(*caller_account.lamports);
                    account.set_executable(caller_account.executable);
                    account.set_rent_epoch(caller_account.rent_epoch);
                }
                let caller_account = if message.is_writable(i, demote_program_write_locks) {
                    if let Some(orig_data_len_index) = keyed_accounts
                        .iter()
                        .position(|keyed_account| keyed_account.unsigned_key() == account_key)
                        .map(|index| {
                            // index starts at first instruction account
                            index - keyed_accounts.len().saturating_sub(orig_data_lens.len())
                        })
                        .and_then(|index| {
                            if index >= orig_data_lens.len() {
                                None
                            } else {
                                Some(index)
                            }
                        })
                    {
                        caller_account.original_data_len = orig_data_lens[orig_data_len_index];
                    } else {
                        ic_msg!(
                            invoke_context,
                            "Internal error: index mismatch for account {}",
                            account_key
                        );
                        return Err(SyscallError::InstructionError(
                            InstructionError::MissingAccount,
                        )
                        .into());
                    }

                    Some(caller_account)
                } else {
                    None
                };
                account_indices.push(account_index);
                accounts.push((account, caller_account));
                continue;
            }
        }
        ic_msg!(
            invoke_context,
            "Instruction references an unknown account {}",
            account_key
        );
        return Err(SyscallError::InstructionError(InstructionError::MissingAccount).into());
    }

    Ok((account_indices, accounts))
}

fn check_instruction_size(
    num_accounts: usize,
    data_len: usize,
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), EbpfError<BpfError>> {
    let size = num_accounts
        .saturating_mul(size_of::<AccountMeta>())
        .saturating_add(data_len);
    let max_size = invoke_context.get_compute_budget().max_cpi_instruction_size;
    if size > max_size {
        return Err(SyscallError::InstructionTooLarge(size, max_size).into());
    }
    Ok(())
}

fn check_account_infos(
    len: usize,
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), EbpfError<BpfError>> {
    if len * size_of::<Pubkey>() > invoke_context.get_compute_budget().max_cpi_instruction_size {
        // Cap the number of account_infos a caller can pass to approximate
        // maximum that accounts that could be passed in an instruction
        return Err(SyscallError::TooManyAccounts.into());
    };
    Ok(())
}

fn check_authorized_program(
    program_id: &Pubkey,
    instruction_data: &[u8],
    invoke_context: &dyn InvokeContext,
) -> Result<(), EbpfError<BpfError>> {
    #[allow(clippy::blocks_in_if_conditions)]
    if native_loader::check_id(program_id)
        || bpf_loader::check_id(program_id)
        || bpf_loader_deprecated::check_id(program_id)
        || (bpf_loader_upgradeable::check_id(program_id)
            && !(bpf_loader_upgradeable::is_upgrade_instruction(instruction_data)
                || bpf_loader_upgradeable::is_set_authority_instruction(instruction_data)
                || bpf_loader_upgradeable::is_close_instruction(instruction_data)))
        || (invoke_context.is_feature_active(&prevent_calling_precompiles_as_programs::id())
            && is_precompile(program_id, |feature_id: &Pubkey| {
                invoke_context.is_feature_active(feature_id)
            }))
    {
        return Err(SyscallError::ProgramNotSupported(*program_id).into());
    }
    Ok(())
}

/// Call process instruction, common to both Rust and C
fn call<'a>(
    syscall: &mut dyn SyscallInvokeSigned<'a>,
    instruction_addr: u64,
    account_infos_addr: u64,
    account_infos_len: u64,
    signers_seeds_addr: u64,
    signers_seeds_len: u64,
    memory_mapping: &MemoryMapping,
) -> Result<u64, EbpfError<BpfError>> {
    let mut invoke_context = syscall.get_context_mut()?;
    invoke_context
        .get_compute_meter()
        .consume(invoke_context.get_compute_budget().invoke_units)?;
    let do_support_realloc = invoke_context.is_feature_active(&do_support_realloc::id());

    // Translate and verify caller's data
    let instruction =
        syscall.translate_instruction(instruction_addr, memory_mapping, *invoke_context)?;
    let caller_program_id = invoke_context
        .get_caller()
        .map_err(SyscallError::InstructionError)?;
    let signers = syscall.translate_signers(
        caller_program_id,
        signers_seeds_addr,
        signers_seeds_len,
        memory_mapping,
    )?;
    let (message, caller_write_privileges, program_indices) =
        InstructionProcessor::create_message(&instruction, &signers, &invoke_context)
            .map_err(SyscallError::InstructionError)?;
    check_authorized_program(&instruction.program_id, &instruction.data, *invoke_context)?;
    let (account_indices, mut accounts) = syscall.translate_accounts(
        &message,
        account_infos_addr,
        account_infos_len,
        memory_mapping,
        *invoke_context,
    )?;

    // Record the instruction
    invoke_context.record_instruction(&instruction);

    // Process instruction
    InstructionProcessor::process_cross_program_instruction(
        &message,
        &program_indices,
        &account_indices,
        &caller_write_privileges,
        *invoke_context,
    )
    .map_err(SyscallError::InstructionError)?;

    // Copy results back to caller
    for (callee_account, caller_account) in accounts.iter_mut() {
        if let Some(caller_account) = caller_account {
            let callee_account = callee_account.borrow();
            *caller_account.lamports = callee_account.lamports();
            *caller_account.owner = *callee_account.owner();
            let new_len = callee_account.data().len();
            if caller_account.data.len() != new_len {
                if !do_support_realloc && !caller_account.data.is_empty() {
                    // Only support for `CreateAccount` at this time.
                    // Need a way to limit total realloc size across multiple CPI calls
                    ic_msg!(
                        invoke_context,
                        "Inner instructions do not support realloc, only SystemProgram::CreateAccount",
                    );
                    return Err(
                        SyscallError::InstructionError(InstructionError::InvalidRealloc).into(),
                    );
                }
                let data_overflow = if do_support_realloc {
                    new_len > caller_account.original_data_len + MAX_PERMITTED_DATA_INCREASE
                } else {
                    new_len > caller_account.data.len() + MAX_PERMITTED_DATA_INCREASE
                };
                if data_overflow {
                    ic_msg!(
                        invoke_context,
                        "Account data size realloc limited to {} in inner instructions",
                        MAX_PERMITTED_DATA_INCREASE
                    );
                    return Err(
                        SyscallError::InstructionError(InstructionError::InvalidRealloc).into(),
                    );
                }
                if new_len < caller_account.data.len() {
                    caller_account.data[new_len..].fill(0);
                }
                caller_account.data = translate_slice_mut::<u8>(
                    memory_mapping,
                    caller_account.vm_data_addr,
                    new_len as u64,
                    &bpf_loader_deprecated::id(), // Don't care since it is byte aligned
                )?;
                *caller_account.ref_to_len_in_vm = new_len as u64;
                *caller_account.serialized_len_ptr = new_len as u64;
            }
            caller_account
                .data
                .copy_from_slice(&callee_account.data()[0..new_len]);
        }
    }

    Ok(SUCCESS)
}

// Return data handling
pub struct SyscallSetReturnData<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallSetReturnData<'a> {
    fn call(
        &mut self,
        addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        let mut invoke_context = question_mark!(
            self.invoke_context
                .try_borrow_mut()
                .map_err(|_| SyscallError::InvokeContextBorrowFailed),
            result
        );

        let budget = invoke_context.get_compute_budget();

        question_mark!(
            invoke_context
                .get_compute_meter()
                .consume(len / budget.cpi_bytes_per_unit + budget.syscall_base_cost),
            result
        );

        if len > MAX_RETURN_DATA as u64 {
            *result = Err(SyscallError::ReturnDataTooLarge(len, MAX_RETURN_DATA as u64).into());
            return;
        }

        let return_data = if len == 0 {
            Vec::new()
        } else {
            question_mark!(
                translate_slice::<u8>(memory_mapping, addr, len, self.loader_id),
                result
            )
            .to_vec()
        };
        question_mark!(
            invoke_context
                .set_return_data(return_data)
                .map_err(SyscallError::InstructionError),
            result
        );

        *result = Ok(0);
    }
}

pub struct SyscallGetReturnData<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallGetReturnData<'a> {
    fn call(
        &mut self,
        return_data_addr: u64,
        mut length: u64,
        program_id_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        let invoke_context = question_mark!(
            self.invoke_context
                .try_borrow()
                .map_err(|_| SyscallError::InvokeContextBorrowFailed),
            result
        );

        let budget = invoke_context.get_compute_budget();

        question_mark!(
            invoke_context
                .get_compute_meter()
                .consume(budget.syscall_base_cost),
            result
        );

        let (program_id, return_data) = invoke_context.get_return_data();
        length = length.min(return_data.len() as u64);
        if length != 0 {
            question_mark!(
                invoke_context
                    .get_compute_meter()
                    .consume((length + size_of::<Pubkey>() as u64) / budget.cpi_bytes_per_unit),
                result
            );

            let return_data_result = question_mark!(
                translate_slice_mut::<u8>(memory_mapping, return_data_addr, length, self.loader_id,),
                result
            );

            return_data_result.copy_from_slice(&return_data[..length as usize]);

            let program_id_result = question_mark!(
                translate_slice_mut::<Pubkey>(memory_mapping, program_id_addr, 1, self.loader_id,),
                result
            );

            program_id_result[0] = program_id;
        }

        // Return the actual length, rather the length returned
        *result = Ok(return_data.len() as u64);
    }
}

// Log data handling
pub struct SyscallLogData<'a> {
    invoke_context: Rc<RefCell<&'a mut dyn InvokeContext>>,
    loader_id: &'a Pubkey,
}
impl<'a> SyscallObject<BpfError> for SyscallLogData<'a> {
    fn call(
        &mut self,
        addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &MemoryMapping,
        result: &mut Result<u64, EbpfError<BpfError>>,
    ) {
        let invoke_context = question_mark!(
            self.invoke_context
                .try_borrow()
                .map_err(|_| SyscallError::InvokeContextBorrowFailed),
            result
        );

        let budget = invoke_context.get_compute_budget();

        question_mark!(
            invoke_context
                .get_compute_meter()
                .consume(budget.syscall_base_cost),
            result
        );

        let untranslated_fields = question_mark!(
            translate_slice::<&[u8]>(memory_mapping, addr, len, self.loader_id),
            result
        );

        question_mark!(
            invoke_context.get_compute_meter().consume(
                budget
                    .syscall_base_cost
                    .saturating_mul(untranslated_fields.len() as u64)
            ),
            result
        );
        question_mark!(
            invoke_context.get_compute_meter().consume(
                untranslated_fields
                    .iter()
                    .fold(0, |total, e| total.saturating_add(e.len() as u64))
            ),
            result
        );

        let mut fields = Vec::with_capacity(untranslated_fields.len());

        for untranslated_field in untranslated_fields {
            fields.push(question_mark!(
                translate_slice::<u8>(
                    memory_mapping,
                    untranslated_field.as_ptr() as *const _ as u64,
                    untranslated_field.len() as u64,
                    self.loader_id,
                ),
                result
            ));
        }

        let logger = invoke_context.get_logger();

        stable_log::program_data(&logger, &fields);

        *result = Ok(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program_runtime::{
        invoke_context::{ThisComputeMeter, ThisInvokeContext, ThisLogger},
        log_collector::LogCollector,
    };
    use solana_rbpf::{
        ebpf::HOST_ALIGN, memory_region::MemoryRegion, user_error::UserError, vm::Config,
    };
    use solana_sdk::{
        bpf_loader, feature_set::FeatureSet, fee_calculator::FeeCalculator, hash::hashv,
    };
    use std::{str::FromStr, sync::Arc};

    macro_rules! assert_access_violation {
        ($result:expr, $va:expr, $len:expr) => {
            match $result {
                Err(EbpfError::AccessViolation(_, _, va, len, _)) if $va == va && $len == len => (),
                Err(EbpfError::StackAccessViolation(_, _, va, len, _))
                    if $va == va && $len == len => {}
                _ => panic!(),
            }
        };
    }

    #[allow(dead_code)]
    struct MockSlice {
        pub vm_addr: u64,
        pub len: usize,
    }

    #[test]
    fn test_translate() {
        const START: u64 = 0x100000000;
        const LENGTH: u64 = 1000;
        let data = vec![0u8; LENGTH as usize];
        let addr = data.as_ptr() as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion::new_from_slice(&data, START, 0, false),
            ],
            &config,
        )
        .unwrap();

        let cases = vec![
            (true, START, 0, addr),
            (true, START, 1, addr),
            (true, START, LENGTH, addr),
            (true, START + 1, LENGTH - 1, addr + 1),
            (false, START + 1, LENGTH, 0),
            (true, START + LENGTH - 1, 1, addr + LENGTH - 1),
            (true, START + LENGTH, 0, addr + LENGTH),
            (false, START + LENGTH, 1, 0),
            (false, START, LENGTH + 1, 0),
            (false, 0, 0, 0),
            (false, 0, 1, 0),
            (false, START - 1, 0, 0),
            (false, START - 1, 1, 0),
            (true, START + LENGTH / 2, LENGTH / 2, addr + LENGTH / 2),
        ];
        for (ok, start, length, value) in cases {
            if ok {
                assert_eq!(
                    translate(&memory_mapping, AccessType::Load, start, length).unwrap(),
                    value
                )
            } else {
                assert!(translate(&memory_mapping, AccessType::Load, start, length).is_err())
            }
        }
    }

    #[test]
    fn test_translate_type() {
        // Pubkey
        let pubkey = solana_sdk::pubkey::new_rand();
        let addr = &pubkey as *const _ as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: std::mem::size_of::<Pubkey>() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let translated_pubkey =
            translate_type::<Pubkey>(&memory_mapping, 0x100000000, &bpf_loader::id()).unwrap();
        assert_eq!(pubkey, *translated_pubkey);

        // Instruction
        let instruction = Instruction::new_with_bincode(
            solana_sdk::pubkey::new_rand(),
            &"foobar",
            vec![AccountMeta::new(solana_sdk::pubkey::new_rand(), false)],
        );
        let addr = &instruction as *const _ as u64;
        let mut memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: std::mem::size_of::<Instruction>() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let translated_instruction =
            translate_type::<Instruction>(&memory_mapping, 0x100000000, &bpf_loader::id()).unwrap();
        assert_eq!(instruction, *translated_instruction);
        memory_mapping.resize_region::<BpfError>(1, 1).unwrap();
        assert!(
            translate_type::<Instruction>(&memory_mapping, 0x100000000, &bpf_loader::id(),)
                .is_err()
        );
    }

    #[test]
    fn test_translate_slice() {
        // zero len
        let good_data = vec![1u8, 2, 3, 4, 5];
        let data: Vec<u8> = vec![];
        assert_eq!(0x1 as *const u8, data.as_ptr());
        let addr = good_data.as_ptr() as *const _ as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: good_data.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let translated_data =
            translate_slice::<u8>(&memory_mapping, data.as_ptr() as u64, 0, &bpf_loader::id())
                .unwrap();
        assert_eq!(data, translated_data);
        assert_eq!(0, translated_data.len());

        // u8
        let mut data = vec![1u8, 2, 3, 4, 5];
        let addr = data.as_ptr() as *const _ as u64;
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: data.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let translated_data = translate_slice::<u8>(
            &memory_mapping,
            0x100000000,
            data.len() as u64,
            &bpf_loader::id(),
        )
        .unwrap();
        assert_eq!(data, translated_data);
        data[0] = 10;
        assert_eq!(data, translated_data);
        assert!(translate_slice::<u8>(
            &memory_mapping,
            data.as_ptr() as u64,
            u64::MAX,
            &bpf_loader::id(),
        )
        .is_err());

        assert!(translate_slice::<u8>(
            &memory_mapping,
            0x100000000 - 1,
            data.len() as u64,
            &bpf_loader::id(),
        )
        .is_err());

        // u64
        let mut data = vec![1u64, 2, 3, 4, 5];
        let addr = data.as_ptr() as *const _ as u64;
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: (data.len() * size_of::<u64>()) as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let translated_data = translate_slice::<u64>(
            &memory_mapping,
            0x100000000,
            data.len() as u64,
            &bpf_loader::id(),
        )
        .unwrap();
        assert_eq!(data, translated_data);
        data[0] = 10;
        assert_eq!(data, translated_data);
        assert!(
            translate_slice::<u64>(&memory_mapping, 0x100000000, u64::MAX, &bpf_loader::id(),)
                .is_err()
        );

        // Pubkeys
        let mut data = vec![solana_sdk::pubkey::new_rand(); 5];
        let addr = data.as_ptr() as *const _ as u64;
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: (data.len() * std::mem::size_of::<Pubkey>()) as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let translated_data = translate_slice::<Pubkey>(
            &memory_mapping,
            0x100000000,
            data.len() as u64,
            &bpf_loader::id(),
        )
        .unwrap();
        assert_eq!(data, translated_data);
        data[0] = solana_sdk::pubkey::new_rand(); // Both should point to same place
        assert_eq!(data, translated_data);
    }

    #[test]
    fn test_translate_string_and_do() {
        let string = "Gaggablaghblagh!";
        let addr = string.as_ptr() as *const _ as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: string.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        assert_eq!(
            42,
            translate_string_and_do(
                &memory_mapping,
                0x100000000,
                string.len() as u64,
                &bpf_loader::id(),
                &mut |string: &str| {
                    assert_eq!(string, "Gaggablaghblagh!");
                    Ok(42)
                }
            )
            .unwrap()
        );
    }

    #[test]
    #[should_panic(expected = "UserError(SyscallError(Abort))")]
    fn test_syscall_abort() {
        let config = Config::default();
        let memory_mapping =
            MemoryMapping::new::<UserError>(vec![MemoryRegion::default()], &config).unwrap();
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        SyscallAbort::call(
            &mut SyscallAbort {},
            0,
            0,
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        result.unwrap();
    }

    #[test]
    #[should_panic(expected = "UserError(SyscallError(Panic(\"Gaggablaghblagh!\", 42, 84)))")]
    fn test_syscall_sol_panic() {
        let string = "Gaggablaghblagh!";
        let addr = string.as_ptr() as *const _ as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: string.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();

        let compute_meter = ThisComputeMeter::new_ref(string.len() as u64 - 1);
        let mut syscall_panic = SyscallPanic {
            compute_meter,
            loader_id: &bpf_loader::id(),
        };
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_panic.call(
            0x100000000,
            string.len() as u64,
            42,
            84,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_eq!(
            Err(EbpfError::UserError(BpfError::SyscallError(
                SyscallError::InstructionError(InstructionError::ComputationalBudgetExceeded)
            ))),
            result
        );

        let compute_meter = ThisComputeMeter::new_ref(string.len() as u64);
        let mut syscall_panic = SyscallPanic {
            compute_meter,
            loader_id: &bpf_loader::id(),
        };
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_panic.call(
            0x100000000,
            string.len() as u64,
            42,
            84,
            0,
            &memory_mapping,
            &mut result,
        );
        result.unwrap();
    }

    #[test]
    fn test_syscall_sol_log() {
        let string = "Gaggablaghblagh!";
        let addr = string.as_ptr() as *const _ as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: string.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let log = Rc::new(LogCollector::default());

        {
            let mut syscall_sol_log = SyscallLog {
                compute_meter: ThisComputeMeter::new_ref(string.len() as u64),
                logger: ThisLogger::new_ref(Some(log.clone())),
                loader_id: &bpf_loader::id(),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall_sol_log.call(
                0x100000000,
                string.len() as u64,
                0,
                0,
                0,
                &memory_mapping,
                &mut result,
            );
            result.unwrap();
        }

        let log: Vec<String> = match Rc::try_unwrap(log) {
            Ok(log) => log.into(),
            Err(_) => panic!("Unwrap failed"),
        };
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], "Program log: Gaggablaghblagh!");

        let mut syscall_sol_log = SyscallLog {
            compute_meter: ThisComputeMeter::new_ref(string.len() as u64 * 3),
            logger: ThisLogger::new_ref(None),
            loader_id: &bpf_loader::id(),
        };
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_log.call(
            0x100000001, // AccessViolation
            string.len() as u64,
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_access_violation!(result, 0x100000001, string.len() as u64);
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_log.call(
            0x100000000,
            string.len() as u64 * 2, // AccessViolation
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_access_violation!(result, 0x100000000, string.len() as u64 * 2);
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_log.call(
            0x100000000,
            string.len() as u64,
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );

        let mut syscall_sol_log = SyscallLog {
            compute_meter: ThisComputeMeter::new_ref((string.len() as u64 * 2) - 1),
            logger: ThisLogger::new_ref(None),
            loader_id: &bpf_loader::id(),
        };
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_log.call(
            0x100000000,
            string.len() as u64,
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        result.unwrap();
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_log.call(
            0x100000000,
            string.len() as u64,
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_eq!(
            Err(EbpfError::UserError(BpfError::SyscallError(
                SyscallError::InstructionError(InstructionError::ComputationalBudgetExceeded)
            ))),
            result
        );
    }

    #[test]
    fn test_syscall_sol_log_u64() {
        let log = Rc::new(LogCollector::default());

        {
            let mut syscall_sol_log_u64 = SyscallLogU64 {
                cost: 0,
                compute_meter: ThisComputeMeter::new_ref(std::u64::MAX),
                logger: ThisLogger::new_ref(Some(log.clone())),
            };
            let config = Config::default();
            let memory_mapping = MemoryMapping::new::<UserError>(vec![], &config).unwrap();
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall_sol_log_u64.call(1, 2, 3, 4, 5, &memory_mapping, &mut result);
            result.unwrap();
        }

        let log: Vec<String> = match Rc::try_unwrap(log) {
            Ok(log) => log.into(),
            Err(_) => panic!("Unwrap failed"),
        };
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], "Program log: 0x1, 0x2, 0x3, 0x4, 0x5");
    }

    #[test]
    fn test_syscall_sol_pubkey() {
        let pubkey = Pubkey::from_str("MoqiU1vryuCGQSxFKA1SZ316JdLEFFhoAu6cKUNk7dN").unwrap();
        let addr = &pubkey.as_ref()[0] as *const _ as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: addr,
                    vm_addr: 0x100000000,
                    len: 32,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let log = Rc::new(LogCollector::default());

        {
            let mut syscall_sol_pubkey = SyscallLogPubkey {
                cost: 1,
                compute_meter: ThisComputeMeter::new_ref(1),
                logger: ThisLogger::new_ref(Some(log.clone())),
                loader_id: &bpf_loader::id(),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall_sol_pubkey.call(0x100000000, 0, 0, 0, 0, &memory_mapping, &mut result);
            result.unwrap();
        }

        let log: Vec<String> = match Rc::try_unwrap(log) {
            Ok(log) => log.into(),
            Err(_) => panic!("Unwrap failed"),
        };
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0],
            "Program log: MoqiU1vryuCGQSxFKA1SZ316JdLEFFhoAu6cKUNk7dN"
        );

        let mut syscall_sol_pubkey = SyscallLogPubkey {
            cost: 1,
            compute_meter: ThisComputeMeter::new_ref(1),
            logger: ThisLogger::new_ref(None),
            loader_id: &bpf_loader::id(),
        };
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_pubkey.call(
            0x100000001, // AccessViolation
            32,
            0,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_access_violation!(result, 0x100000001, 32);
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall_sol_pubkey.call(100, 32, 0, 0, 0, &memory_mapping, &mut result);
        assert_eq!(
            Err(EbpfError::UserError(BpfError::SyscallError(
                SyscallError::InstructionError(InstructionError::ComputationalBudgetExceeded)
            ))),
            result
        );
    }

    #[test]
    fn test_syscall_sol_alloc_free() {
        let config = Config::default();
        // large alloc
        {
            let heap = AlignedMemory::new_with_size(100, HOST_ALIGN);
            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_PROGRAM_START, 0, false),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_STACK_START, 4096, true),
                    MemoryRegion::new_from_slice(heap.as_slice(), ebpf::MM_HEAP_START, 0, true),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_INPUT_START, 0, true),
                ],
                &config,
            )
            .unwrap();
            let mut syscall = SyscallAllocFree {
                aligned: true,
                allocator: BpfAllocator::new(heap, ebpf::MM_HEAP_START),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall.call(100, 0, 0, 0, 0, &memory_mapping, &mut result);
            assert_ne!(result.unwrap(), 0);
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall.call(100, 0, 0, 0, 0, &memory_mapping, &mut result);
            assert_eq!(result.unwrap(), 0);
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall.call(u64::MAX, 0, 0, 0, 0, &memory_mapping, &mut result);
            assert_eq!(result.unwrap(), 0);
        }
        // many small unaligned allocs
        {
            let heap = AlignedMemory::new_with_size(100, HOST_ALIGN);
            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_PROGRAM_START, 0, false),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_STACK_START, 4096, true),
                    MemoryRegion::new_from_slice(heap.as_slice(), ebpf::MM_HEAP_START, 0, true),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_INPUT_START, 0, true),
                ],
                &config,
            )
            .unwrap();
            let mut syscall = SyscallAllocFree {
                aligned: false,
                allocator: BpfAllocator::new(heap, ebpf::MM_HEAP_START),
            };
            for _ in 0..100 {
                let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
                syscall.call(1, 0, 0, 0, 0, &memory_mapping, &mut result);
                assert_ne!(result.unwrap(), 0);
            }
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall.call(100, 0, 0, 0, 0, &memory_mapping, &mut result);
            assert_eq!(result.unwrap(), 0);
        }
        // many small aligned allocs
        {
            let heap = AlignedMemory::new_with_size(100, HOST_ALIGN);
            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_PROGRAM_START, 0, false),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_STACK_START, 4096, true),
                    MemoryRegion::new_from_slice(heap.as_slice(), ebpf::MM_HEAP_START, 0, true),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_INPUT_START, 0, true),
                ],
                &config,
            )
            .unwrap();
            let mut syscall = SyscallAllocFree {
                aligned: true,
                allocator: BpfAllocator::new(heap, ebpf::MM_HEAP_START),
            };
            for _ in 0..12 {
                let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
                syscall.call(1, 0, 0, 0, 0, &memory_mapping, &mut result);
                assert_ne!(result.unwrap(), 0);
            }
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall.call(100, 0, 0, 0, 0, &memory_mapping, &mut result);
            assert_eq!(result.unwrap(), 0);
        }
        // aligned allocs

        fn check_alignment<T>() {
            let heap = AlignedMemory::new_with_size(100, HOST_ALIGN);
            let config = Config::default();
            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_PROGRAM_START, 0, false),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_STACK_START, 4096, true),
                    MemoryRegion::new_from_slice(heap.as_slice(), ebpf::MM_HEAP_START, 0, true),
                    MemoryRegion::new_from_slice(&[], ebpf::MM_INPUT_START, 0, true),
                ],
                &config,
            )
            .unwrap();
            let mut syscall = SyscallAllocFree {
                aligned: true,
                allocator: BpfAllocator::new(heap, ebpf::MM_HEAP_START),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
            syscall.call(
                size_of::<u8>() as u64,
                0,
                0,
                0,
                0,
                &memory_mapping,
                &mut result,
            );
            let address = result.unwrap();
            assert_ne!(address, 0);
            assert_eq!((address as *const u8).align_offset(align_of::<u8>()), 0);
        }
        check_alignment::<u8>();
        check_alignment::<u16>();
        check_alignment::<u32>();
        check_alignment::<u64>();
        check_alignment::<u128>();
    }

    #[test]
    fn test_syscall_sha256() {
        let bytes1 = "Gaggablaghblagh!";
        let bytes2 = "flurbos";

        let mock_slice1 = MockSlice {
            vm_addr: 0x300000000,
            len: bytes1.len(),
        };
        let mock_slice2 = MockSlice {
            vm_addr: 0x400000000,
            len: bytes2.len(),
        };
        let bytes_to_hash = [mock_slice1, mock_slice2];
        let hash_result = [0; HASH_BYTES];
        let ro_len = bytes_to_hash.len() as u64;
        let ro_va = 0x100000000;
        let rw_va = 0x200000000;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new::<UserError>(
            vec![
                MemoryRegion::default(),
                MemoryRegion {
                    host_addr: bytes_to_hash.as_ptr() as *const _ as u64,
                    vm_addr: ro_va,
                    len: 32,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
                MemoryRegion {
                    host_addr: hash_result.as_ptr() as *const _ as u64,
                    vm_addr: rw_va,
                    len: HASH_BYTES as u64,
                    vm_gap_shift: 63,
                    is_writable: true,
                },
                MemoryRegion {
                    host_addr: bytes1.as_ptr() as *const _ as u64,
                    vm_addr: bytes_to_hash[0].vm_addr,
                    len: bytes1.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
                MemoryRegion {
                    host_addr: bytes2.as_ptr() as *const _ as u64,
                    vm_addr: bytes_to_hash[1].vm_addr,
                    len: bytes2.len() as u64,
                    vm_gap_shift: 63,
                    is_writable: false,
                },
            ],
            &config,
        )
        .unwrap();
        let compute_meter = ThisComputeMeter::new_ref((bytes1.len() + bytes2.len()) as u64);
        let mut syscall = SyscallSha256 {
            sha256_base_cost: 0,
            sha256_byte_cost: 2,
            compute_meter,
            loader_id: &bpf_loader_deprecated::id(),
        };

        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall.call(ro_va, ro_len, rw_va, 0, 0, &memory_mapping, &mut result);
        result.unwrap();

        let hash_local = hashv(&[bytes1.as_ref(), bytes2.as_ref()]).to_bytes();
        assert_eq!(hash_result, hash_local);
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall.call(
            ro_va - 1, // AccessViolation
            ro_len,
            rw_va,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_access_violation!(result, ro_va - 1, 32);
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall.call(
            ro_va,
            ro_len + 1, // AccessViolation
            rw_va,
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_access_violation!(result, ro_va, 48);
        let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);
        syscall.call(
            ro_va,
            ro_len,
            rw_va - 1, // AccessViolation
            0,
            0,
            &memory_mapping,
            &mut result,
        );
        assert_access_violation!(result, rw_va - 1, HASH_BYTES as u64);

        syscall.call(ro_va, ro_len, rw_va, 0, 0, &memory_mapping, &mut result);
        assert_eq!(
            Err(EbpfError::UserError(BpfError::SyscallError(
                SyscallError::InstructionError(InstructionError::ComputationalBudgetExceeded)
            ))),
            result
        );
    }

    #[test]
    fn test_syscall_get_sysvar() {
        let config = Config::default();
        // Test clock sysvar
        {
            let got_clock = Clock::default();
            let got_clock_va = 0x100000000;

            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion {
                        host_addr: &got_clock as *const _ as u64,
                        vm_addr: got_clock_va,
                        len: size_of::<Clock>() as u64,
                        vm_gap_shift: 63,
                        is_writable: true,
                    },
                ],
                &config,
            )
            .unwrap();

            let src_clock = Clock {
                slot: 1,
                epoch_start_timestamp: 2,
                epoch: 3,
                leader_schedule_epoch: 4,
                unix_timestamp: 5,
            };
            let mut data = vec![];
            bincode::serialize_into(&mut data, &src_clock).unwrap();
            let sysvars = [(sysvar::clock::id(), data)];
            let mut invoke_context = ThisInvokeContext::new_mock_with_sysvars_and_features(
                &[],
                &[],
                &sysvars,
                Arc::new(FeatureSet::all_enabled()),
            );

            let mut syscall = SyscallGetClockSysvar {
                invoke_context: Rc::new(RefCell::new(&mut invoke_context)),
                loader_id: &bpf_loader::id(),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);

            syscall.call(got_clock_va, 0, 0, 0, 0, &memory_mapping, &mut result);
            result.unwrap();
            assert_eq!(got_clock, src_clock);
        }

        // Test epoch_schedule sysvar
        {
            let got_epochschedule = EpochSchedule::default();
            let got_epochschedule_va = 0x100000000;

            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion {
                        host_addr: &got_epochschedule as *const _ as u64,
                        vm_addr: got_epochschedule_va,
                        len: size_of::<EpochSchedule>() as u64,
                        vm_gap_shift: 63,
                        is_writable: true,
                    },
                ],
                &config,
            )
            .unwrap();

            let src_epochschedule = EpochSchedule {
                slots_per_epoch: 1,
                leader_schedule_slot_offset: 2,
                warmup: false,
                first_normal_epoch: 3,
                first_normal_slot: 4,
            };
            let mut data = vec![];
            bincode::serialize_into(&mut data, &src_epochschedule).unwrap();
            let sysvars = [(sysvar::epoch_schedule::id(), data)];
            let mut invoke_context = ThisInvokeContext::new_mock_with_sysvars_and_features(
                &[],
                &[],
                &sysvars,
                Arc::new(FeatureSet::all_enabled()),
            );

            let mut syscall = SyscallGetEpochScheduleSysvar {
                invoke_context: Rc::new(RefCell::new(&mut invoke_context)),
                loader_id: &bpf_loader::id(),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);

            syscall.call(
                got_epochschedule_va,
                0,
                0,
                0,
                0,
                &memory_mapping,
                &mut result,
            );
            result.unwrap();
            assert_eq!(got_epochschedule, src_epochschedule);
        }

        // Test fees sysvar
        #[allow(deprecated)]
        {
            let got_fees = Fees::default();
            let got_fees_va = 0x100000000;

            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion {
                        host_addr: &got_fees as *const _ as u64,
                        vm_addr: got_fees_va,
                        len: size_of::<Fees>() as u64,
                        vm_gap_shift: 63,
                        is_writable: true,
                    },
                ],
                &config,
            )
            .unwrap();

            let src_fees = Fees {
                fee_calculator: FeeCalculator {
                    lamports_per_signature: 1,
                },
            };
            let mut data = vec![];
            bincode::serialize_into(&mut data, &src_fees).unwrap();
            let sysvars = [(sysvar::fees::id(), data)];
            let mut invoke_context = ThisInvokeContext::new_mock_with_sysvars_and_features(
                &[],
                &[],
                &sysvars,
                Arc::new(FeatureSet::all_enabled()),
            );

            let mut syscall = SyscallGetFeesSysvar {
                invoke_context: Rc::new(RefCell::new(&mut invoke_context)),
                loader_id: &bpf_loader::id(),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);

            syscall.call(got_fees_va, 0, 0, 0, 0, &memory_mapping, &mut result);
            result.unwrap();
            assert_eq!(got_fees, src_fees);
        }

        // Test rent sysvar
        {
            let got_rent = Rent::default();
            let got_rent_va = 0x100000000;

            let memory_mapping = MemoryMapping::new::<UserError>(
                vec![
                    MemoryRegion::default(),
                    MemoryRegion {
                        host_addr: &got_rent as *const _ as u64,
                        vm_addr: got_rent_va,
                        len: size_of::<Rent>() as u64,
                        vm_gap_shift: 63,
                        is_writable: true,
                    },
                ],
                &config,
            )
            .unwrap();

            let src_rent = Rent {
                lamports_per_byte_year: 1,
                exemption_threshold: 2.0,
                burn_percent: 3,
            };
            let mut data = vec![];
            bincode::serialize_into(&mut data, &src_rent).unwrap();
            let sysvars = [(sysvar::rent::id(), data)];
            let mut invoke_context = ThisInvokeContext::new_mock_with_sysvars_and_features(
                &[],
                &[],
                &sysvars,
                Arc::new(FeatureSet::all_enabled()),
            );

            let mut syscall = SyscallGetRentSysvar {
                invoke_context: Rc::new(RefCell::new(&mut invoke_context)),
                loader_id: &bpf_loader::id(),
            };
            let mut result: Result<u64, EbpfError<BpfError>> = Ok(0);

            syscall.call(got_rent_va, 0, 0, 0, 0, &memory_mapping, &mut result);
            result.unwrap();
            assert_eq!(got_rent, src_rent);
        }
    }

    #[test]
    fn test_overlapping() {
        assert!(!check_overlapping(10, 7, 3));
        assert!(check_overlapping(10, 8, 3));
        assert!(check_overlapping(10, 9, 3));
        assert!(check_overlapping(10, 10, 3));
        assert!(check_overlapping(10, 11, 3));
        assert!(check_overlapping(10, 12, 3));
        assert!(!check_overlapping(10, 13, 3));
    }

    fn call_program_address_common(
        seeds: &[&[u8]],
        program_id: &Pubkey,
        syscall: &mut dyn SyscallObject<BpfError>,
    ) -> Result<(Pubkey, u8), EbpfError<BpfError>> {
        const SEEDS_VA: u64 = 0x100000000;
        const PROGRAM_ID_VA: u64 = 0x200000000;
        const ADDRESS_VA: u64 = 0x300000000;
        const BUMP_SEED_VA: u64 = 0x400000000;
        const SEED_VA: u64 = 0x500000000;

        let config = Config::default();
        let address = Pubkey::default();
        let bump_seed = 0;
        let mut mock_slices = Vec::with_capacity(seeds.len());
        let mut regions = vec![
            MemoryRegion::default(),
            MemoryRegion {
                host_addr: mock_slices.as_ptr() as u64,
                vm_addr: SEEDS_VA,
                len: (seeds.len() * size_of::<MockSlice>()) as u64,
                vm_gap_shift: 63,
                is_writable: false,
            },
            MemoryRegion {
                host_addr: program_id.as_ref().as_ptr() as u64,
                vm_addr: PROGRAM_ID_VA,
                len: 32,
                vm_gap_shift: 63,
                is_writable: false,
            },
            MemoryRegion {
                host_addr: address.as_ref().as_ptr() as u64,
                vm_addr: ADDRESS_VA,
                len: 32,
                vm_gap_shift: 63,
                is_writable: true,
            },
            MemoryRegion {
                host_addr: &bump_seed as *const u8 as u64,
                vm_addr: BUMP_SEED_VA,
                len: 32,
                vm_gap_shift: 63,
                is_writable: true,
            },
        ];

        for (i, seed) in seeds.iter().enumerate() {
            let vm_addr = SEED_VA + (i as u64 * 0x100000000);
            let mock_slice = MockSlice {
                vm_addr,
                len: seed.len(),
            };
            mock_slices.push(mock_slice);
            regions.push(MemoryRegion {
                host_addr: seed.as_ptr() as u64,
                vm_addr,
                len: seed.len() as u64,
                vm_gap_shift: 63,
                is_writable: false,
            });
        }
        let memory_mapping = MemoryMapping::new::<UserError>(regions, &config).unwrap();

        let mut result = Ok(0);
        syscall.call(
            SEEDS_VA,
            seeds.len() as u64,
            PROGRAM_ID_VA,
            ADDRESS_VA,
            BUMP_SEED_VA,
            &memory_mapping,
            &mut result,
        );
        let _ = result?;
        Ok((address, bump_seed))
    }

    fn create_program_address(
        seeds: &[&[u8]],
        program_id: &Pubkey,
        remaining: u64,
    ) -> Result<Pubkey, EbpfError<BpfError>> {
        let mut syscall = SyscallCreateProgramAddress {
            cost: 1,
            compute_meter: ThisComputeMeter::new_ref(remaining),
            loader_id: &bpf_loader::id(),
        };
        let (address, _) = call_program_address_common(seeds, program_id, &mut syscall)?;
        Ok(address)
    }

    fn try_find_program_address(
        seeds: &[&[u8]],
        program_id: &Pubkey,
        remaining: u64,
    ) -> Result<(Pubkey, u8), EbpfError<BpfError>> {
        let mut syscall = SyscallTryFindProgramAddress {
            cost: 1,
            compute_meter: ThisComputeMeter::new_ref(remaining),
            loader_id: &bpf_loader::id(),
        };
        call_program_address_common(seeds, program_id, &mut syscall)
    }

    #[test]
    fn test_create_program_address() {
        // These tests duplicate the direct tests in solana_program::pubkey

        let program_id = Pubkey::from_str("BPFLoaderUpgradeab1e11111111111111111111111").unwrap();

        let exceeded_seed = &[127; MAX_SEED_LEN + 1];
        let result = create_program_address(&[exceeded_seed], &program_id, 1);
        assert_eq!(
            result,
            Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into())
        );
        assert_eq!(
            create_program_address(&[b"short_seed", exceeded_seed], &program_id, 1),
            Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into())
        );
        let max_seed = &[0; MAX_SEED_LEN];
        assert!(create_program_address(&[max_seed], &program_id, 1).is_ok());
        let exceeded_seeds: &[&[u8]] = &[
            &[1],
            &[2],
            &[3],
            &[4],
            &[5],
            &[6],
            &[7],
            &[8],
            &[9],
            &[10],
            &[11],
            &[12],
            &[13],
            &[14],
            &[15],
            &[16],
        ];
        assert!(create_program_address(exceeded_seeds, &program_id, 1).is_ok());
        let max_seeds: &[&[u8]] = &[
            &[1],
            &[2],
            &[3],
            &[4],
            &[5],
            &[6],
            &[7],
            &[8],
            &[9],
            &[10],
            &[11],
            &[12],
            &[13],
            &[14],
            &[15],
            &[16],
            &[17],
        ];
        assert_eq!(
            create_program_address(max_seeds, &program_id, 1),
            Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into())
        );
        assert_eq!(
            create_program_address(&[b"", &[1]], &program_id, 0),
            Err(
                SyscallError::InstructionError(InstructionError::ComputationalBudgetExceeded)
                    .into()
            )
        );
        assert_eq!(
            create_program_address(&[b"", &[1]], &program_id, 1),
            Ok("BwqrghZA2htAcqq8dzP1WDAhTXYTYWj7CHxF5j7TDBAe"
                .parse()
                .unwrap())
        );
        assert_eq!(
            create_program_address(&["☉".as_ref(), &[0]], &program_id, 1),
            Ok("13yWmRpaTR4r5nAktwLqMpRNr28tnVUZw26rTvPSSB19"
                .parse()
                .unwrap())
        );
        assert_eq!(
            create_program_address(&[b"Talking", b"Squirrels"], &program_id, 1),
            Ok("2fnQrngrQT4SeLcdToJAD96phoEjNL2man2kfRLCASVk"
                .parse()
                .unwrap())
        );
        let public_key = Pubkey::from_str("SeedPubey1111111111111111111111111111111111").unwrap();
        assert_eq!(
            create_program_address(&[public_key.as_ref(), &[1]], &program_id, 1),
            Ok("976ymqVnfE32QFe6NfGDctSvVa36LWnvYxhU6G2232YL"
                .parse()
                .unwrap())
        );
        assert_ne!(
            create_program_address(&[b"Talking", b"Squirrels"], &program_id, 1).unwrap(),
            create_program_address(&[b"Talking"], &program_id, 1).unwrap(),
        );
    }

    #[test]
    fn test_find_program_address() {
        for _ in 0..1_000 {
            let program_id = Pubkey::new_unique();
            let (address, bump_seed) =
                try_find_program_address(&[b"Lil'", b"Bits"], &program_id, 100).unwrap();
            assert_eq!(
                address,
                create_program_address(&[b"Lil'", b"Bits", &[bump_seed]], &program_id, 1).unwrap()
            );
        }

        let program_id = Pubkey::from_str("BPFLoaderUpgradeab1e11111111111111111111111").unwrap();
        let max_tries = 256; // one per seed
        let seeds: &[&[u8]] = &[b""];
        let (_, bump_seed) = try_find_program_address(seeds, &program_id, max_tries).unwrap();
        let remaining = 256 - bump_seed as u64;
        let _ = try_find_program_address(seeds, &program_id, remaining).unwrap();
        assert_eq!(
            try_find_program_address(seeds, &program_id, remaining - 1),
            Err(
                SyscallError::InstructionError(InstructionError::ComputationalBudgetExceeded)
                    .into()
            )
        );
        let exceeded_seed = &[127; MAX_SEED_LEN + 1];
        assert_eq!(
            try_find_program_address(&[exceeded_seed], &program_id, max_tries - 1),
            Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into())
        );
        let exceeded_seeds: &[&[u8]] = &[
            &[1],
            &[2],
            &[3],
            &[4],
            &[5],
            &[6],
            &[7],
            &[8],
            &[9],
            &[10],
            &[11],
            &[12],
            &[13],
            &[14],
            &[15],
            &[16],
            &[17],
        ];
        assert_eq!(
            try_find_program_address(exceeded_seeds, &program_id, max_tries - 1),
            Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into())
        );
    }
}
