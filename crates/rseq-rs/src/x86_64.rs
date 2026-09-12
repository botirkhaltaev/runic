//! Private RSEQ word ops. One committing store through the caller word.

use core::ptr::NonNull;

use crate::abi::{Area, CPU_ID_OFF, CS_OFF, SIG};

pub(crate) enum Attempt {
    Ok(usize),
    Miss(usize),
    Abort,
}

impl Attempt {
    #[inline]
    fn from_status(status: u64, value: usize) -> Self {
        match status {
            0 => Self::Ok(value),
            1 => Self::Miss(value),
            _ => Self::Abort,
        }
    }
}

/// # Safety
/// `area` is this thread's rseq TLS and `word` is a live `usize`.
#[inline]
pub(crate) unsafe fn compare_exchange(
    area: NonNull<Area>,
    word: *mut usize,
    cpu: u32,
    expect: usize,
    new: usize,
) -> Attempt {
    let current: usize;
    let status: u64;
    // SAFETY: `area` is this thread's rseq TLS; `word` is a live usize.
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
            "mov {got:e}, dword ptr [{rseq} + {cpu_id_off}]",
            "cmp {got:e}, {cpu:e}",
            "jne 92f",
            "mov {current}, qword ptr [{word}]",
            "cmp {current}, {expect}",
            "jne 90f",
            "mov qword ptr [{word}], {new}",
            "6:",
            "xor {status:e}, {status:e}",
            "jmp 20f",
            "90:",
            "mov {status:e}, 1",
            "jmp 20f",
            "92:",
            "xor {current:e}, {current:e}",
            "mov {status:e}, 2",
            "20:",
            rseq = in(reg) area.as_ptr(),
            word = in(reg) word,
            cpu = in(reg) cpu,
            expect = in(reg) expect,
            new = in(reg) new,
            got = out(reg) _,
            tmp = out(reg) _,
            current = out(reg) current,
            status = out(reg) status,
            sig = const SIG,
            cpu_id_off = const CPU_ID_OFF,
            cs_off = const CS_OFF,
            options(nostack),
        );
    }
    Attempt::from_status(status, current)
}

/// # Safety
/// `area` is this thread's rseq TLS and `word` is a live `usize`.
#[inline]
pub(crate) unsafe fn fetch_add(
    area: NonNull<Area>,
    word: *mut usize,
    cpu: u32,
    count: usize,
) -> Attempt {
    let prev: usize;
    let status: u64;
    // SAFETY: `area` is this thread's rseq TLS; `word` is a live usize.
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
            "mov {got:e}, dword ptr [{rseq} + {cpu_id_off}]",
            "cmp {got:e}, {cpu:e}",
            "jne 92f",
            "mov {prev}, qword ptr [{word}]",
            "mov {sum}, {prev}",
            "add {sum}, {count}",
            "mov qword ptr [{word}], {sum}",
            "6:",
            "xor {status:e}, {status:e}",
            "jmp 20f",
            "92:",
            "xor {prev:e}, {prev:e}",
            "mov {status:e}, 2",
            "20:",
            rseq = in(reg) area.as_ptr(),
            word = in(reg) word,
            cpu = in(reg) cpu,
            count = in(reg) count,
            got = out(reg) _,
            tmp = out(reg) _,
            sum = out(reg) _,
            prev = out(reg) prev,
            status = out(reg) status,
            sig = const SIG,
            cpu_id_off = const CPU_ID_OFF,
            cs_off = const CS_OFF,
            options(nostack),
        );
    }
    Attempt::from_status(status, prev)
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
