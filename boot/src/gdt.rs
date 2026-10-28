use slopos_hermetic::KernelStackTop;
use slopos_ostd::arch::x86_64::gdt::IstSlot;
use slopos_ostd::arch::x86_64::msr::{install_syscall_msrs, star_from_selectors};
use slopos_ostd::cpu::x86_64::pcr::{MAX_CPUS, current_cpu_id};
use slopos_ostd::klog_debug;

/// The round trip owns `RSP0` once a task runs; this is the gdt tests' probe.
#[cfg(feature = "test-hooks")]
pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    let cpu_id = current_cpu_id();
    gdt_set_kernel_rsp0_for_cpu(cpu_id, rsp0);
}

#[cfg(feature = "test-hooks")]
fn gdt_set_kernel_rsp0_for_cpu(cpu_id: usize, rsp0: u64) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    if let Some(pcr) = slopos_ostd::cpu::x86_64::pcr::get_pcr_mut_via_token(cpu_id) {
        pcr.kernel_rsp = rsp0;
        pcr.sync_tss_rsp0();
    }
}

/// Bind an IST slot on the current CPU to a real kernel stack.
///
/// The `&mut BootCtx` gates the call to a CPU-init scope; BSP-init, AP-init and
/// hermetic test scopes all bind IST slots, hence the kind-polymorphism.
pub fn gdt_set_ist<'b, K: slopos_hermetic::CpuInitKind>(
    _ctx: &mut slopos_hermetic::BootCtx<'b, K>,
    slot: IstSlot,
    stack_top: KernelStackTop<'_>,
) {
    let cpu_id = current_cpu_id();
    gdt_set_ist_for_cpu(_ctx, cpu_id, slot, stack_top);
}

pub fn gdt_set_ist_for_cpu<'b, K: slopos_hermetic::CpuInitKind>(
    _ctx: &mut slopos_hermetic::BootCtx<'b, K>,
    cpu_id: usize,
    slot: IstSlot,
    stack_top: KernelStackTop<'_>,
) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let addr = stack_top.as_u64();
    if let Some(pcr) = slopos_ostd::cpu::x86_64::pcr::get_pcr_mut_via_token(cpu_id) {
        pcr.set_ist(slot.as_index(), addr);
    }
}

/// Program the SYSCALL/SYSRET MSRs (`STAR`, `LSTAR`, `SFMASK`). The witness
/// gates it to a boot-init scope.
pub fn syscall_msr_init<W: slopos_ostd::sync::CpuInitWitness>(witness: &W) {
    use slopos_ostd::arch::x86_64::gdt::SegmentSelector;

    klog_debug!("SYSCALL: Initializing MSRs for fast syscall path");

    let star_value = star_from_selectors(SegmentSelector::KERNEL_CODE, SegmentSelector::USER_DATA);
    let lstar_value = slopos_ostd::user::mode::user_return_trampoline_addr();
    let sfmask_value: u64 = 0x0000_0000_0004_7700;

    install_syscall_msrs(witness, star_value, lstar_value, sfmask_value);

    klog_debug!(
        "SYSCALL: STAR=0x{:016x} LSTAR=0x{:016x} SFMASK=0x{:016x}",
        star_value,
        lstar_value,
        sfmask_value
    );
}
