//! `membarrier(2)` for the RSEQ expedited command.

const PRIVATE_EXPEDITED_RSEQ: libc::c_int = 1 << 7;
const REGISTER_PRIVATE_EXPEDITED_RSEQ: libc::c_int = 1 << 8;
const FLAG_CPU: libc::c_int = 1 << 0;

pub(crate) fn register() -> bool {
    membarrier(REGISTER_PRIVATE_EXPEDITED_RSEQ, 0, 0) == 0
}

pub(crate) fn fence(cpu: u32) -> bool {
    let Ok(cpu) = libc::c_int::try_from(cpu) else {
        return false;
    };
    membarrier(PRIVATE_EXPEDITED_RSEQ, FLAG_CPU, cpu) == 0
}

fn membarrier(cmd: libc::c_int, flags: libc::c_int, cpu: libc::c_int) -> libc::c_long {
    // SAFETY: `SYS_membarrier` takes (cmd, flags, cpuid). No memory operands.
    unsafe { libc::syscall(libc::SYS_membarrier, cmd, flags, cpu) }
}
