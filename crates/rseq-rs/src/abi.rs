/// Kernel `struct rseq`. glibc on this host registers 20 bytes; do not read
/// `node_id` / `mm_cid` unless the registered size is 32.
#[repr(C, align(32))]
pub(crate) struct Area {
    pub cpu_id_start: u32,
    pub cpu_id: u32,
    pub rseq_cs: u64,
    pub flags: u32,
    pub node_id: u32,
    pub mm_cid: u32,
}

/// Minimum glibc area (`cpu_id_start` .. `flags`).
pub(crate) const AREA_MIN: usize = 20;
pub(crate) const CPU_UNINIT: u32 = u32::MAX;
/// glibc `RSEQ_SIG` on x86-64.
pub(crate) const SIG: u32 = 0x5305_3053;
pub(crate) const CPU_ID_OFF: usize = 4;
pub(crate) const CS_OFF: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn area_abi() {
        assert_eq!(offset_of!(Area, cpu_id_start), 0);
        assert_eq!(offset_of!(Area, cpu_id), 4);
        assert_eq!(offset_of!(Area, rseq_cs), 8);
        assert_eq!(offset_of!(Area, flags), 16);
        assert_eq!(offset_of!(Area, node_id), 20);
        assert_eq!(offset_of!(Area, mm_cid), 24);
        assert_eq!(align_of::<Area>(), 32);
        assert!(size_of::<Area>() >= AREA_MIN);
        assert_eq!(SIG, 0x5305_3053);
        assert_eq!(CPU_ID_OFF, 4);
        assert_eq!(CS_OFF, 8);
    }
}
