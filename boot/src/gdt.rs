#![allow(static_mut_refs)]

use core::arch::asm;

use slopos_abi::arch::x86_64::gdt::{
    GDT_STANDARD_ENTRIES, GdtDescriptor, GdtLayout, SegmentSelector, Tss64,
};
use slopos_abi::arch::x86_64::msr::Msr;
use slopos_lib::{MAX_CPUS, get_current_cpu, klog_debug};

#[repr(C)]
struct PerCpuSyscallData {
    user_rsp_scratch: u64,
    kernel_rsp: u64,
}

const EMPTY_SYSCALL_DATA: PerCpuSyscallData = PerCpuSyscallData {
    user_rsp_scratch: 0,
    kernel_rsp: 0,
};

static mut PER_CPU_GDT: [GdtLayout; MAX_CPUS] = [GdtLayout::new(); MAX_CPUS];
static mut PER_CPU_TSS: [Tss64; MAX_CPUS] = [Tss64::new(); MAX_CPUS];
static mut PER_CPU_SYSCALL_DATA: [PerCpuSyscallData; MAX_CPUS] = [EMPTY_SYSCALL_DATA; MAX_CPUS];

#[unsafe(no_mangle)]
static mut SYSCALL_CPU_DATA_PTR: u64 = 0;

unsafe extern "C" {
    static kernel_stack_top: u8;
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn load_gdt(descriptor: &GdtDescriptor) {
    unsafe { asm!("lgdt [{0}]", in(reg) descriptor, options(nostack, preserves_flags)) };

    unsafe {
        asm!(
            "pushq ${code}",
            "lea 2f(%rip), %rax",
            "pushq %rax",
            "lretq",
            "2:",
            "movw ${data}, %ax",
            "movw %ax, %ds",
            "movw %ax, %es",
            "movw %ax, %ss",
            "movw %ax, %fs",
            "movw %ax, %gs",
            code = const SegmentSelector::KERNEL_CODE.bits() as usize,
            data = const SegmentSelector::KERNEL_DATA.bits() as usize,
            out("rax") _,
            options(att_syntax, nostack)
        );
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn load_tss() {
    let selector = SegmentSelector::TSS.bits();
    unsafe { asm!("ltr {0:x}", in(reg) selector, options(nostack, preserves_flags)) };
}
pub fn gdt_init() {
    gdt_init_for_cpu(0);
}

pub fn gdt_init_for_cpu(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }

    if slopos_lib::pcr::is_pcr_initialized() {
        klog_debug!("GDT: Skipped - using PCR-based GDT for CPU {}", cpu_id);
        return;
    }

    klog_debug!("GDT: Initializing descriptor tables for CPU {}", cpu_id);

    unsafe {
        PER_CPU_GDT[cpu_id].entries = GDT_STANDARD_ENTRIES;
        PER_CPU_GDT[cpu_id].load_tss(&PER_CPU_TSS[cpu_id]);

        PER_CPU_TSS[cpu_id].iomap_base = core::mem::size_of::<Tss64>() as u16;
        if cpu_id == 0 {
            PER_CPU_TSS[cpu_id].rsp0 = (&kernel_stack_top as *const u8) as u64;
        }

        let descriptor = GdtDescriptor::from_layout(&PER_CPU_GDT[cpu_id]);

        load_gdt(&descriptor);
        load_tss();
    }

    klog_debug!("GDT: Initialized with TSS loaded for CPU {}", cpu_id);
}
pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    let cpu_id = get_current_cpu();
    gdt_set_kernel_rsp0_for_cpu(cpu_id, rsp0);
}

pub fn gdt_set_kernel_rsp0_for_cpu(cpu_id: usize, rsp0: u64) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        PER_CPU_TSS[cpu_id].rsp0 = rsp0;
        PER_CPU_SYSCALL_DATA[cpu_id].kernel_rsp = rsp0;
    }
    if let Some(pcr) = unsafe { slopos_lib::pcr::get_pcr_mut(cpu_id) } {
        pcr.kernel_rsp = rsp0;
        pcr.sync_tss_rsp0();
    }
}

pub fn gdt_set_ist(index: u8, stack_top: u64) {
    let cpu_id = get_current_cpu();
    gdt_set_ist_for_cpu(cpu_id, index, stack_top);
}

pub fn gdt_set_ist_for_cpu(cpu_id: usize, index: u8, stack_top: u64) {
    if cpu_id >= MAX_CPUS || index == 0 || index > 7 {
        return;
    }
    unsafe {
        PER_CPU_TSS[cpu_id].ist[(index - 1) as usize] = stack_top;
    }
    if let Some(pcr) = unsafe { slopos_lib::pcr::get_pcr_mut(cpu_id) } {
        pcr.set_ist(index, stack_top);
    }
}

unsafe extern "C" {
    fn syscall_entry();
}

#[inline(always)]
fn wrmsr(msr: Msr, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr.address(),
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags)
        );
    }
}

fn rdmsr(msr: Msr) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr.address(),
            out("eax") low,
            out("edx") high,
            options(nostack, preserves_flags)
        );
    }
    ((high as u64) << 32) | (low as u64)
}

const EFER_SCE: u64 = 1 << 0;

pub fn syscall_msr_init() {
    klog_debug!("SYSCALL: Initializing MSRs for fast syscall path");

    let efer = rdmsr(Msr::EFER);
    if (efer & EFER_SCE) == 0 {
        wrmsr(Msr::EFER, efer | EFER_SCE);
        klog_debug!("SYSCALL: Enabled SCE bit in EFER");
    }

    let star_value: u64 = ((SegmentSelector::USER_DATA.bits() as u64 - 8) << 48)
        | ((SegmentSelector::KERNEL_CODE.bits() as u64) << 32);

    let lstar_value = syscall_entry as *const () as u64;

    let sfmask_value: u64 = 0x0000_0000_0004_7700;

    wrmsr(Msr::STAR, star_value);
    wrmsr(Msr::LSTAR, lstar_value);
    wrmsr(Msr::SFMASK, sfmask_value);

    klog_debug!(
        "SYSCALL: STAR=0x{:016x} LSTAR=0x{:016x} SFMASK=0x{:016x}",
        star_value,
        lstar_value,
        sfmask_value
    );

    syscall_gs_base_init();
}

fn syscall_gs_base_init() {
    if slopos_lib::pcr::is_pcr_initialized() {
        klog_debug!("SYSCALL: Skipped GS_BASE init - using PCR");
        return;
    }
    let cpu_id = get_current_cpu();
    syscall_gs_base_init_for_cpu(cpu_id);
}

fn syscall_gs_base_init_for_cpu(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        PER_CPU_SYSCALL_DATA[cpu_id].kernel_rsp = PER_CPU_TSS[cpu_id].rsp0;
        let cpu_data_ptr = &PER_CPU_SYSCALL_DATA[cpu_id] as *const _ as u64;
        if cpu_id == 0 {
            SYSCALL_CPU_DATA_PTR = cpu_data_ptr;
        }
        wrmsr(Msr::KERNEL_GS_BASE, cpu_data_ptr);
        klog_debug!(
            "SYSCALL: CPU {} KERNEL_GS_BASE=0x{:016x}",
            cpu_id,
            cpu_data_ptr
        );
    }
}

pub fn syscall_update_kernel_rsp(rsp: u64) {
    let cpu_id = get_current_cpu();
    syscall_update_kernel_rsp_for_cpu(cpu_id, rsp);
}

pub fn syscall_update_kernel_rsp_for_cpu(cpu_id: usize, rsp: u64) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        PER_CPU_SYSCALL_DATA[cpu_id].kernel_rsp = rsp;
    }
}
