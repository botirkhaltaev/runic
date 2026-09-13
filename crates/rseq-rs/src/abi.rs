/// Kernel `struct rseq` prefix. glibc on this host registers 20 bytes; do not
/// read `node_id` / `mm_cid` unless the registered size is 32.
/// Packed so the type is 20 bytes — `repr(C)` would pad to 24.
#[repr(C, packed)]
pub(crate) struct Area {
    pub cpu_id_start: u32,
    pub cpu_id: u32,
    pub rseq_cs: u64,
    pub flags: u32,
}

/// Minimum glibc area (`cpu_id_start` .. `flags`).
pub(crate) const AREA_MIN: usize = 20;
pub(crate) const CPU_UNINIT: u32 = u32::MAX;
/// `RSEQ_CPU_ID_REGISTRATION_FAILED` (`-2`).
pub(crate) const CPU_REG_FAILED: u32 = u32::MAX - 1;
/// glibc `RSEQ_SIG` on x86-64.
pub(crate) const SIG: u32 = 0x5305_3053;
pub(crate) const CPU_ID_OFF: usize = 4;
pub(crate) const CS_OFF: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn area_abi() {
        assert_eq!(offset_of!(Area, cpu_id_start), 0);
        assert_eq!(offset_of!(Area, cpu_id), 4);
        assert_eq!(offset_of!(Area, rseq_cs), 8);
        assert_eq!(offset_of!(Area, flags), 16);
        assert_eq!(size_of::<Area>(), AREA_MIN);
        assert_eq!(SIG, 0x5305_3053);
        assert_eq!(CPU_ID_OFF, 4);
        assert_eq!(CS_OFF, 8);
    }
}
