pub struct TraceRng(u64);

impl TraceRng {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    pub fn next_usize(&mut self, upper: usize) -> usize {
        assert!(upper > 0);
        (self.next_u64() as usize) % upper
    }

    pub fn biased_size(&mut self, max: usize) -> usize {
        let cap = self.next_usize(max).max(16);
        self.next_usize(cap).max(1)
    }

    pub fn pareto_size(&mut self, max_exp: u32) -> usize {
        let class = self
            .next_u64()
            .trailing_zeros()
            .min(max_exp.saturating_sub(1));
        let base = 1_usize << class.max(3);
        let range = base.min(1_usize << max_exp);
        base.saturating_add(self.next_usize(range)).max(1)
    }

    pub fn alignment(&mut self) -> usize {
        const ALIGNS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 128, 4096];
        ALIGNS[self.next_usize(ALIGNS.len())]
    }
}
