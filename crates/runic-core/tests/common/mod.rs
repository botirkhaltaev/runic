//! Shared fixtures for the integration suites: the size-class table as the
//! public contract states it, a seeded generator, and a checksummed block.

use core::alloc::Layout;

/// Small block sizes, 8 bytes to 32 KiB. Anything larger is an extent.
pub const CLASS_SIZES: [usize; 27] = [
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

/// Extent sizes that cross the interesting boundaries: just past the largest
/// class, one run, several pages past a run, and a multi-megabyte mapping.
pub const EXTENT_SIZES: [usize; 4] = [32768 + 1, 64 * 1024, 300 * 1024, 3 * 1024 * 1024];

/// `SplitMix64`. Deterministic per seed so a failure names its trace.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, upper: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(upper).unwrap()).unwrap()
    }
}

/// A live allocation with a per-block byte pattern so any overlap, lost
/// write, or bad copy is visible when the block is checked or freed.
pub struct Block {
    pub ptr: *mut u8,
    pub layout: Layout,
    pub seed: u64,
}

// SAFETY: a `Block` is the unique owner of its allocation, and the allocator
// under test supports free and realloc from any thread.
unsafe impl Send for Block {}

impl Block {
    pub fn fill(&self) {
        let (words, tail) = Self::split(self.layout.size());
        for word in 0..words {
            // SAFETY: `ptr` is live for `layout.size()` bytes; unaligned write.
            unsafe {
                self.ptr
                    .add(word * 8)
                    .cast::<u64>()
                    .write_unaligned(self.word_at(word));
            }
        }
        let bytes = self.word_at(words).to_le_bytes();
        for (offset, byte) in bytes[..tail].iter().enumerate() {
            // SAFETY: the tail lies inside `layout.size()`.
            unsafe { self.ptr.add(words * 8 + offset).write(*byte) };
        }
    }

    pub fn check(&self) {
        self.check_prefix(self.ptr, self.layout.size());
    }

    /// The first `len` bytes at `ptr` carry this block's pattern.
    pub fn check_prefix(&self, ptr: *mut u8, len: usize) {
        let (words, tail) = Self::split(len);
        for word in 0..words {
            // SAFETY: the caller vouches `ptr` is live for `len` bytes.
            let found = unsafe { ptr.add(word * 8).cast::<u64>().read_unaligned() };
            assert_eq!(
                found,
                self.word_at(word),
                "word {word} of a {}-byte block at {:p}",
                self.layout.size(),
                self.ptr
            );
        }
        let bytes = self.word_at(words).to_le_bytes();
        for (offset, byte) in bytes[..tail].iter().enumerate() {
            // SAFETY: the tail lies inside `len`.
            let found = unsafe { ptr.add(words * 8 + offset).read() };
            assert_eq!(
                found,
                *byte,
                "tail byte {offset} of a {}-byte block at {:p}",
                self.layout.size(),
                self.ptr
            );
        }
    }

    fn split(len: usize) -> (usize, usize) {
        (len / 8, len % 8)
    }

    fn word_at(&self, word: usize) -> u64 {
        Rng::new(self.seed ^ u64::try_from(word).unwrap().rotate_left(17)).next()
    }
}

pub fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}
