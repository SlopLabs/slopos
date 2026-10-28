use slopos_abi::addr::VirtAddr;
use slopos_ostd::mm::tlb::LocalTlbFlush;

pub struct LocalTlbFlushImpl;

pub static LOCAL_TLB: LocalTlbFlushImpl = LocalTlbFlushImpl;

pub static LOCAL_TLB_DYN: &dyn LocalTlbFlush = &LOCAL_TLB;

impl LocalTlbFlush for LocalTlbFlushImpl {
    fn invlpg(&self, vaddr: VirtAddr) {
        slopos_ostd::cpu::x86_64::tlb::invlpg(vaddr.0);
    }
}
