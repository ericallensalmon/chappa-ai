//! Fast single-byte search for the ESC byte (0x1B), the scanner's Ground
//! hot path. Plain text rarely contains ESC, so this search dominates the
//! cost of a stream with nothing to extract — it must be near memcpy speed
//! or the pre-scan would slow the parser pipeline.
//!
//! Ladder, first match wins, all with runtime feature detection:
//!   1. AVX-512 (64B/iter) on x86-64 when `avx512bw` is available —
//!      `_mm512_cmpeq_epi8_mask` yields the match offset directly.
//!   2. AVX2 (32B/iter) when `avx2` is available.
//!   3. `memchr` otherwise (SSE2 on x86, NEON on aarch64, scalar
//!      fallback) — the same crate vte itself uses.
//!
//! The `#[target_feature]` functions may only be called when the detected
//! feature is present; each dispatch checks its own prerequisite.

/// Position of the first 0x1B byte in `data`, if any.
#[inline]
pub fn find_esc(data: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512bw") {
            return unsafe { find_esc_avx512(data) };
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            return unsafe { find_esc_avx2(data) };
        }
    }
    memchr::memchr(0x1B, data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
unsafe fn find_esc_avx512(data: &[u8]) -> Option<usize> {
    use std::arch::x86_64::*;
    let ptr = data.as_ptr();
    let len = data.len();
    let needle = _mm512_set1_epi8(0x1B);
    let mut i = 0usize;
    while i + 64 <= len {
        let v = _mm512_loadu_si512(ptr.add(i) as *const __m512i);
        let cmp = _mm512_cmpeq_epi8_mask(v, needle);
        if cmp != 0 {
            return Some(i + cmp.trailing_zeros() as usize);
        }
        i += 64;
    }
    (i..len).find(|&j| *data.get_unchecked(j) == 0x1B)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_esc_avx2(data: &[u8]) -> Option<usize> {
    use std::arch::x86_64::*;
    let ptr = data.as_ptr();
    let len = data.len();
    let needle = _mm256_set1_epi8(0x1B);
    let mut i = 0usize;
    while i + 32 <= len {
        let v = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let cmp = _mm256_cmpeq_epi8(v, needle);
        let mask = _mm256_movemask_epi8(cmp) as u32;
        if mask != 0 {
            return Some(i + mask.trailing_zeros() as usize);
        }
        i += 32;
    }
    (i..len).find(|&j| *data.get_unchecked(j) == 0x1B)
}
