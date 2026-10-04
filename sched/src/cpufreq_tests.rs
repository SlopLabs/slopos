//! Frequency control and topology: every CPU records itself, a settings change
//! reaches every CPU, and HWP operations refuse a machine without HWP.

use slopos_abi::errno::Errno;
use slopos_abi::syscall::{
    CPU_PERF_F_HWP, CPU_PERF_F_HWP_EPP, CPU_PERF_F_HYBRID, CPU_PERF_OP_EPP, CPU_PERF_OP_HWP,
    CPU_PERF_OP_LIMITS, CPU_PERF_OP_PLACEMENT,
};
use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use crate::cpufreq;

fn online_cpus() -> impl Iterator<Item = usize> {
    (0..slopos_arch::pcr::get_cpu_count()).filter(|&cpu| slopos_arch::pcr::is_cpu_online(cpu))
}

/// Each online CPU ran its own bring-up: it is recorded, under its own APIC ID,
/// and a machine of identical cores without SMT siblings is not ranked.
pub fn test_every_online_cpu_records_itself() -> TestResult {
    let mut seen = [u32::MAX; 64];
    for (n, cpu) in online_cpus().enumerate() {
        let record = cpufreq::cpu_record(cpu);
        if record.online != 1 || record.cpu as usize != cpu {
            klog_info!("CPUFREQ_TEST: cpu {} never recorded its state", cpu);
            return TestResult::Fail;
        }
        if seen.contains(&record.apic_id) {
            klog_info!(
                "CPUFREQ_TEST: cpu {} shares APIC ID {}",
                cpu,
                record.apic_id
            );
            return TestResult::Fail;
        }
        if let Some(slot) = seen.get_mut(n) {
            *slot = record.apic_id;
        }
    }
    let homogeneous = cpufreq::info().flags & CPU_PERF_F_HYBRID == 0
        && online_cpus().all(|cpu| {
            let core = cpufreq::cpu_record(cpu).core_id;
            online_cpus()
                .filter(|&other| cpufreq::cpu_record(other).core_id == core)
                .count()
                == 1
        });
    if homogeneous && cpufreq::ranking_active() {
        klog_info!("CPUFREQ_TEST: identical cores without siblings are being ranked");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A change is applied by every online CPU, idle ones included: they are woken
/// for it rather than left until their next tick, which a tickless idle CPU
/// may not take.
pub fn test_a_settings_change_reaches_every_cpu() -> TestResult {
    let placement = cpufreq::info().placement as u64;
    let generation = match cpufreq::control(CPU_PERF_OP_PLACEMENT, placement) {
        Ok(generation) => generation,
        Err(errno) => {
            klog_info!("CPUFREQ_TEST: placement change refused: {:?}", errno);
            return TestResult::Fail;
        }
    };
    if cpufreq::info().generation != generation {
        klog_info!("CPUFREQ_TEST: the answered generation is not the machine's");
        return TestResult::Fail;
    }
    let deadline = slopos_kernel_services::clock::monotonic_ns() + 1_000_000_000;
    loop {
        let behind = online_cpus()
            .filter(|&cpu| cpufreq::cpu_record(cpu).applied != generation)
            .count();
        if behind == 0 {
            return TestResult::Pass;
        }
        let now = slopos_kernel_services::clock::monotonic_ns();
        if now == 0 || now >= deadline {
            klog_info!(
                "CPUFREQ_TEST: {} CPU(s) never applied generation {}",
                behind,
                generation
            );
            return TestResult::Fail;
        }
        core::hint::spin_loop();
    }
}

/// An operation outside the table, or a value outside its range, is `EINVAL`;
/// an HWP operation on a CPU without HWP is `EOPNOTSUPP`, and changes nothing.
pub fn test_operations_refuse_what_the_machine_lacks() -> TestResult {
    let before = cpufreq::info();
    if cpufreq::control(0, 0) != Err(Errno::EINVAL) || cpufreq::control(99, 0) != Err(Errno::EINVAL)
    {
        klog_info!("CPUFREQ_TEST: an unknown operation was not EINVAL");
        return TestResult::Fail;
    }
    if cpufreq::control(CPU_PERF_OP_PLACEMENT, 2) != Err(Errno::EINVAL) {
        klog_info!("CPUFREQ_TEST: an unknown placement was not EINVAL");
        return TestResult::Fail;
    }
    if before.flags & CPU_PERF_F_HWP == 0 {
        for op in [CPU_PERF_OP_HWP, CPU_PERF_OP_LIMITS] {
            if cpufreq::control(op, 0) != Err(Errno::EOPNOTSUPP) {
                klog_info!("CPUFREQ_TEST: HWP op {} accepted without HWP", op);
                return TestResult::Fail;
            }
        }
    }
    if before.flags & CPU_PERF_F_HWP_EPP == 0
        && cpufreq::control(CPU_PERF_OP_EPP, 0) != Err(Errno::EOPNOTSUPP)
    {
        klog_info!("CPUFREQ_TEST: an EPP was accepted without the EPP field");
        return TestResult::Fail;
    }
    if before.flags & CPU_PERF_F_HWP_EPP != 0
        && cpufreq::control(CPU_PERF_OP_EPP, 256) != Err(Errno::EINVAL)
    {
        klog_info!("CPUFREQ_TEST: an EPP past 255 was accepted");
        return TestResult::Fail;
    }
    let after = cpufreq::info();
    if after.generation != before.generation
        || after.epp != before.epp
        || after.limits != before.limits
        || after.policy != before.policy
        || after.placement != before.placement
    {
        klog_info!("CPUFREQ_TEST: a refused operation changed the settings");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_every_online_cpu_records_itself, suite = cpufreq);
slopos_testing::stest!(
    name = test_a_settings_change_reaches_every_cpu,
    suite = cpufreq
);
slopos_testing::stest!(
    name = test_operations_refuse_what_the_machine_lacks,
    suite = cpufreq
);
