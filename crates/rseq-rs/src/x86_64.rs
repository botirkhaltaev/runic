//! Private RSEQ pop / push. One committing store of `current`.

use core::ptr::NonNull;

use crate::abi::{Area, CPU_ID_OFF, CS_OFF, SIG};

/// # Safety
/// `area` is this thread's rseq TLS and `base` is a live per-CPU region.
#[inline]
pub(crate) unsafe fn pop(
    area: NonNull<Area>,
    base: *mut u8,
    shift: u8,
    ncpus: u32,
) -> crate::stacks::Fast {
    let obj: *mut u8;
    let status: u64;
    // SAFETY: `area` is this thread's rseq TLS; `base` is a live per-CPU region.
    unsafe {
        core::arch::asm!(
            ".pushsection __rseq_cs, \"aw\"",
            ".balign 32",
            "99:",
            ".long 0",
            ".long 0",
            ".quad 2f",
            ".quad (6f - 2f)",
            ".quad 7f",
            ".popsection",
            "jmp 7f",
            ".long {sig}",
            "7:",
            "lea {tmp}, [rip + 99b]",
            "mov qword ptr [{rseq} + {cs_off}], {tmp}",
            "2:",
            "mov {cpu:e}, dword ptr [{rseq} + {cpu_id_off}]",
            "cmp {cpu:e}, {ncpus:e}",
            "jae 92f",
            "mov {blk}, {cpu}",
            "shl {blk}, cl",
            "add {blk}, {base}",
            "mov {cur:e}, dword ptr [{blk}]",
            "mov {cap:e}, dword ptr [{blk} + 4]",
            "test {cap:e}, {cap:e}",
            "jz 90f",
            "test {cur:e}, {cur:e}",
            "jz 90f",
            "dec {cur:e}",
            "mov {obj}, qword ptr [{blk} + 8 + {cur} * 8]",
            "mov dword ptr [{blk}], {cur:e}",
            "6:",
            "xor {status:e}, {status:e}",
            "jmp 20f",
            "90:",
            "mov {status:e}, 1",
            "jmp 20f",
            "92:",
            "xor {obj:e}, {obj:e}",
            "mov {status:e}, 2",
            "20:",
            rseq = in(reg) area.as_ptr(),
            base = in(reg) base,
            ncpus = in(reg) ncpus,
            in("cl") shift,
            cpu = out(reg) _,
            blk = out(reg) _,
            cur = out(reg) _,
            cap = out(reg) _,
            tmp = out(reg) _,
            obj = out(reg) obj,
            status = out(reg) status,
            sig = const SIG,
            cpu_id_off = const CPU_ID_OFF,
            cs_off = const CS_OFF,
            options(nostack),
        );
    }
    crate::stacks::Fast::from_status(status, obj)
}

/// # Safety
/// `area` is this thread's rseq TLS and `base` is a live per-CPU region.
#[inline]
pub(crate) unsafe fn push(
    area: NonNull<Area>,
    base: *mut u8,
    shift: u8,
    ncpus: u32,
    obj: *mut u8,
) -> crate::stacks::Fast {
    let status: u64;
    // SAFETY: `area` is this thread's rseq TLS; `base` is a live per-CPU region.
    unsafe {
        core::arch::asm!(
            ".pushsection __rseq_cs, \"aw\"",
            ".balign 32",
            "99:",
            ".long 0",
            ".long 0",
            ".quad 2f",
            ".quad (6f - 2f)",
            ".quad 7f",
            ".popsection",
            "jmp 7f",
            ".long {sig}",
            "7:",
            "lea {tmp}, [rip + 99b]",
            "mov qword ptr [{rseq} + {cs_off}], {tmp}",
            "2:",
            "mov {cpu:e}, dword ptr [{rseq} + {cpu_id_off}]",
            "cmp {cpu:e}, {ncpus:e}",
            "jae 92f",
            "mov {blk}, {cpu}",
            "shl {blk}, cl",
            "add {blk}, {base}",
            "mov {cur:e}, dword ptr [{blk}]",
            "mov {cap:e}, dword ptr [{blk} + 4]",
            "cmp {cur:e}, {cap:e}",
            "jae 90f",
            "mov qword ptr [{blk} + 8 + {cur} * 8], {obj}",
            "inc {cur:e}",
            "mov dword ptr [{blk}], {cur:e}",
            "6:",
            "xor {status:e}, {status:e}",
            "jmp 20f",
            "90:",
            "mov {status:e}, 1",
            "jmp 20f",
            "92:",
            "mov {status:e}, 2",
            "20:",
            rseq = in(reg) area.as_ptr(),
            base = in(reg) base,
            ncpus = in(reg) ncpus,
            obj = in(reg) obj,
            in("cl") shift,
            cpu = out(reg) _,
            blk = out(reg) _,
            cur = out(reg) _,
            cap = out(reg) _,
            tmp = out(reg) _,
            status = out(reg) status,
            sig = const SIG,
            cpu_id_off = const CPU_ID_OFF,
            cs_off = const CS_OFF,
            options(nostack),
        );
    }
    crate::stacks::Fast::from_status(status, core::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use crate::abi::SIG;

    #[test]
    fn signature_precedes_abort() {
        let seen: u32;
        // SAFETY: the block only reads four bytes immediately before a local label.
        unsafe {
            core::arch::asm!(
                "lea {p}, [rip + 7f]",
                "mov {seen:e}, dword ptr [{p} - 4]",
                "jmp 8f",
                "jmp 7f",
                ".long {sig}",
                "7:",
                "8:",
                p = out(reg) _,
                seen = out(reg) seen,
                sig = const SIG,
                options(nostack),
            );
        }
        assert_eq!(seen, SIG);
    }
}
