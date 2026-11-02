include!(concat!(env!("OUT_DIR"), "/tables.rs"));

/// Add two GF(2^8) elements (XOR).
#[inline(always)]
pub fn add(a: u8, b: u8) -> u8 {
    a ^ b
}

/// Multiply two GF(2^8) elements using lookup table.
#[inline(always)]
pub fn mul(a: u8, b: u8) -> u8 {
    MUL_TABLE[a as usize][b as usize]
}

/// Divide a by b in GF(2^8). Panics if b is 0.
#[inline(always)]
pub fn div(a: u8, b: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    assert!(b != 0, "Division by zero in GF(2^8)");
    let log_a = LOG_TABLE[a as usize] as isize;
    let log_b = LOG_TABLE[b as usize] as isize;
    let mut log_result = log_a - log_b;
    if log_result < 0 {
        log_result += 255;
    }
    EXP_TABLE[log_result as usize]
}

/// Compute a^n in GF(2^8).
#[inline(always)]
pub fn exp(a: u8, n: usize) -> u8 {
    if n == 0 {
        return 1;
    }
    if a == 0 {
        return 0;
    }
    let log_a = LOG_TABLE[a as usize] as usize;
    let log_result = log_a * (n % 255) % 255;
    EXP_TABLE[log_result]
}

/// Multiply each element of `input` by `c` and write to `out`, on the best kernel this CPU
/// has ([`detect_mul_slice`]).
#[inline]
pub fn mul_slice(c: u8, input: &[u8], out: &mut [u8]) {
    assert_eq!(input.len(), out.len());
    if input.is_empty() || c == 0 {
        out.iter_mut().for_each(|o| *o = 0);
        return;
    }
    if c == 1 {
        out.copy_from_slice(input);
        return;
    }
    detect_mul_slice().0(c, input, out)
}

/// Multiply each element of `input` by `c` and XOR into `out`, on the best kernel this CPU
/// has ([`detect_mul_slice`]).
#[inline]
pub fn mul_slice_xor(c: u8, input: &[u8], out: &mut [u8]) {
    assert_eq!(input.len(), out.len());
    if input.is_empty() || c == 0 {
        return;
    }
    if c == 1 {
        for (o, i) in out.iter_mut().zip(input.iter()) {
            *o ^= *i;
        }
        return;
    }
    detect_mul_slice().1(c, input, out)
}

/// Function pointer types for bulk GF(2^8) operations.
pub type MulSliceFn = fn(u8, &[u8], &mut [u8]);

/// `(mul_slice, mul_slice_xor)` for the best instruction set this CPU has, in priority order:
/// GFNI+AVX2, AVX2, GFNI+SSE, SSSE3, scalar.
///
/// Neither handles `c == 0` or `c == 1`; the caller does, as `mul_slice` and `mul_slice_xor`
/// do. Each kernel's entry re-checks its feature (one cached load) before the instructions, so
/// the pair stays sound as plain `fn`s whoever calls them.
pub fn detect_mul_slice() -> (MulSliceFn, MulSliceFn) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("gfni") && is_x86_feature_detected!("avx2") {
            return (x86::gfni_avx2::<false>, x86::gfni_avx2::<true>);
        }
        if is_x86_feature_detected!("avx2") {
            return (x86::avx2::<false>, x86::avx2::<true>);
        }
        if is_x86_feature_detected!("gfni") {
            return (x86::gfni_sse::<false>, x86::gfni_sse::<true>);
        }
        if is_x86_feature_detected!("ssse3") {
            return (x86::ssse3::<false>, x86::ssse3::<true>);
        }
    }
    (mul_slice_scalar, mul_slice_xor_scalar)
}

// ── Scalar fallback ──────────────────────────────────────────────────────

fn mul_slice_scalar(c: u8, input: &[u8], out: &mut [u8]) {
    let mt = &MUL_TABLE[c as usize];
    for (o, &i) in out.iter_mut().zip(input.iter()) {
        *o = mt[i as usize];
    }
}

fn mul_slice_xor_scalar(c: u8, input: &[u8], out: &mut [u8]) {
    let mt = &MUL_TABLE[c as usize];
    for (o, &i) in out.iter_mut().zip(input.iter()) {
        *o ^= mt[i as usize];
    }
}

// ── x86_64 kernels ───────────────────────────────────────────────────────
//
// Each kernel is compiled for one instruction set, which is the whole of its contract: the safe
// entry in front of it checks the CPU and takes the scalar path otherwise, so no caller carries
// an obligation. Both slices are walked as `chunks_exact`, with the tails on the scalar table,
// so a length never reaches a pointer.

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{
        mul_slice_scalar, mul_slice_xor_scalar, GFNI_TABLE, MUL_TABLE_HIGH, MUL_TABLE_LOW,
    };
    use core::arch::x86_64::*;

    /// `out = c * input`, or `out ^= c * input` with `XOR`, on the scalar table.
    fn scalar<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        if XOR {
            mul_slice_xor_scalar(c, input, out)
        } else {
            mul_slice_scalar(c, input, out)
        }
    }

    /// 32 bytes per `vgf2p8affineqb`.
    pub(super) fn gfni_avx2<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        assert_eq!(input.len(), out.len());
        if is_x86_feature_detected!("gfni") && is_x86_feature_detected!("avx2") {
            // SAFETY: the kernel's instruction set was detected just above.
            unsafe { gfni_avx2_kernel::<XOR>(c, input, out) }
        } else {
            scalar::<XOR>(c, input, out)
        }
    }

    #[target_feature(enable = "gfni,avx2")]
    unsafe fn gfni_avx2_kernel<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        let mat = _mm256_set1_epi64x(GFNI_TABLE[c as usize] as i64);
        let (mut ins, mut outs) = (input.chunks_exact(32), out.chunks_exact_mut(32));
        for (i, o) in (&mut ins).zip(&mut outs) {
            // SAFETY: `i` and `o` are 32-byte chunks; the unaligned load and store stay inside.
            unsafe {
                let mut v =
                    _mm256_gf2p8affine_epi64_epi8(_mm256_loadu_si256(i.as_ptr().cast()), mat, 0);
                if XOR {
                    v = _mm256_xor_si256(v, _mm256_loadu_si256(o.as_ptr().cast()));
                }
                _mm256_storeu_si256(o.as_mut_ptr().cast(), v);
            }
        }
        scalar::<XOR>(c, ins.remainder(), outs.into_remainder());
    }

    /// 16 bytes per `vgf2p8affineqb`.
    pub(super) fn gfni_sse<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        assert_eq!(input.len(), out.len());
        if is_x86_feature_detected!("gfni") {
            // SAFETY: the kernel's instruction set was detected just above.
            unsafe { gfni_sse_kernel::<XOR>(c, input, out) }
        } else {
            scalar::<XOR>(c, input, out)
        }
    }

    #[target_feature(enable = "gfni")]
    unsafe fn gfni_sse_kernel<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        let mat = _mm_set1_epi64x(GFNI_TABLE[c as usize] as i64);
        let (mut ins, mut outs) = (input.chunks_exact(16), out.chunks_exact_mut(16));
        for (i, o) in (&mut ins).zip(&mut outs) {
            // SAFETY: `i` and `o` are 16-byte chunks; the unaligned load and store stay inside.
            unsafe {
                let mut v = _mm_gf2p8affine_epi64_epi8(_mm_loadu_si128(i.as_ptr().cast()), mat, 0);
                if XOR {
                    v = _mm_xor_si128(v, _mm_loadu_si128(o.as_ptr().cast()));
                }
                _mm_storeu_si128(o.as_mut_ptr().cast(), v);
            }
        }
        scalar::<XOR>(c, ins.remainder(), outs.into_remainder());
    }

    /// 32 bytes per pair of `vpshufb` nibble lookups.
    pub(super) fn avx2<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        assert_eq!(input.len(), out.len());
        if is_x86_feature_detected!("avx2") {
            // SAFETY: the kernel's instruction set was detected just above.
            unsafe { avx2_kernel::<XOR>(c, input, out) }
        } else {
            scalar::<XOR>(c, input, out)
        }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn avx2_kernel<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        let (low, high) = (&MUL_TABLE_LOW[c as usize], &MUL_TABLE_HIGH[c as usize]);
        // SAFETY: both table rows are 16 bytes, the width of one unaligned load.
        let (low, high) = unsafe {
            (
                _mm256_broadcastsi128_si256(_mm_loadu_si128(low.as_ptr().cast())),
                _mm256_broadcastsi128_si256(_mm_loadu_si128(high.as_ptr().cast())),
            )
        };
        let mask = _mm256_set1_epi8(0x0F);
        let (mut ins, mut outs) = (input.chunks_exact(32), out.chunks_exact_mut(32));
        for (i, o) in (&mut ins).zip(&mut outs) {
            // SAFETY: `i` and `o` are 32-byte chunks; the unaligned load and store stay inside.
            unsafe {
                let data = _mm256_loadu_si256(i.as_ptr().cast());
                let lo = _mm256_shuffle_epi8(low, _mm256_and_si256(data, mask));
                let hi =
                    _mm256_shuffle_epi8(high, _mm256_and_si256(_mm256_srli_epi64(data, 4), mask));
                let mut v = _mm256_xor_si256(lo, hi);
                if XOR {
                    v = _mm256_xor_si256(v, _mm256_loadu_si256(o.as_ptr().cast()));
                }
                _mm256_storeu_si256(o.as_mut_ptr().cast(), v);
            }
        }
        scalar::<XOR>(c, ins.remainder(), outs.into_remainder());
    }

    /// 16 bytes per pair of `pshufb` nibble lookups.
    pub(super) fn ssse3<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        assert_eq!(input.len(), out.len());
        if is_x86_feature_detected!("ssse3") {
            // SAFETY: the kernel's instruction set was detected just above.
            unsafe { ssse3_kernel::<XOR>(c, input, out) }
        } else {
            scalar::<XOR>(c, input, out)
        }
    }

    #[target_feature(enable = "ssse3")]
    unsafe fn ssse3_kernel<const XOR: bool>(c: u8, input: &[u8], out: &mut [u8]) {
        let (low, high) = (&MUL_TABLE_LOW[c as usize], &MUL_TABLE_HIGH[c as usize]);
        // SAFETY: both table rows are 16 bytes, the width of one unaligned load.
        let (low, high) = unsafe {
            (
                _mm_loadu_si128(low.as_ptr().cast()),
                _mm_loadu_si128(high.as_ptr().cast()),
            )
        };
        let mask = _mm_set1_epi8(0x0F);
        let (mut ins, mut outs) = (input.chunks_exact(16), out.chunks_exact_mut(16));
        for (i, o) in (&mut ins).zip(&mut outs) {
            // SAFETY: `i` and `o` are 16-byte chunks; the unaligned load and store stay inside.
            unsafe {
                let data = _mm_loadu_si128(i.as_ptr().cast());
                let lo = _mm_shuffle_epi8(low, _mm_and_si128(data, mask));
                let hi = _mm_shuffle_epi8(high, _mm_and_si128(_mm_srli_epi64(data, 4), mask));
                let mut v = _mm_xor_si128(lo, hi);
                if XOR {
                    v = _mm_xor_si128(v, _mm_loadu_si128(o.as_ptr().cast()));
                }
                _mm_storeu_si128(o.as_mut_ptr().cast(), v);
            }
        }
        scalar::<XOR>(c, ins.remainder(), outs.into_remainder());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gfni_table() {
        // Verify GFNI_TABLE by emulating vgf2p8affineqb in software:
        //   result_bit[i] = popcount(x AND qword_byte[7-i]) mod 2
        for c in 0u16..256 {
            let matrix = GFNI_TABLE[c as usize];
            for b in 0u16..256 {
                let expected = MUL_TABLE[c as usize][b as usize];
                let x = b as u8;
                let mut result: u8 = 0;
                for i in 0..8u32 {
                    let row_byte = ((matrix >> ((7 - i) * 8)) & 0xFF) as u8;
                    let dot = (row_byte & x).count_ones() % 2;
                    result |= (dot as u8) << i;
                }
                assert_eq!(
                    result, expected,
                    "GFNI table mismatch: c={c}, b={b}, got={result}, expected={expected}"
                );
            }
        }
    }

    #[test]
    fn test_add() {
        assert_eq!(add(0, 0), 0);
        assert_eq!(add(1, 0), 1);
        assert_eq!(add(0, 1), 1);
        assert_eq!(add(1, 1), 0);
        assert_eq!(add(0xFF, 0xFF), 0);
        assert_eq!(add(0xAA, 0x55), 0xFF);
    }

    #[test]
    fn test_mul() {
        assert_eq!(mul(0, 0), 0);
        assert_eq!(mul(1, 0), 0);
        assert_eq!(mul(0, 1), 0);
        assert_eq!(mul(1, 1), 1);
        // a * 1 = a
        for a in 0u8..=255 {
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(1, a), a);
        }
        // a * 0 = 0
        for a in 0u8..=255 {
            assert_eq!(mul(a, 0), 0);
        }
    }

    #[test]
    fn test_div() {
        // a / 1 = a
        for a in 0u8..=255 {
            assert_eq!(div(a, 1), a);
        }
        // a / a = 1 (for a != 0)
        for a in 1u8..=255 {
            assert_eq!(div(a, a), 1);
        }
        // (a * b) / b = a
        for a in 1u8..=255 {
            for b in 1u8..=255 {
                assert_eq!(div(mul(a, b), b), a);
            }
        }
    }

    #[test]
    fn test_exp() {
        assert_eq!(exp(0, 0), 1);
        assert_eq!(exp(1, 0), 1);
        assert_eq!(exp(5, 0), 1);
        assert_eq!(exp(0, 1), 0);
        assert_eq!(exp(0, 100), 0);
        // a^1 = a
        for a in 0u8..=255 {
            assert_eq!(exp(a, 1), a);
        }
        // a^2 = a * a
        for a in 0u8..=255 {
            assert_eq!(exp(a, 2), mul(a, a));
        }
    }

    #[test]
    fn test_mul_slice_basic() {
        let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut out = [0u8; 8];
        mul_slice(3, &input, &mut out);
        for i in 0..input.len() {
            assert_eq!(out[i], mul(3, input[i]));
        }
    }

    #[test]
    fn test_mul_slice_xor_basic() {
        let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut out = [10u8; 8];
        let original = out;
        mul_slice_xor(3, &input, &mut out);
        for i in 0..input.len() {
            assert_eq!(out[i], original[i] ^ mul(3, input[i]));
        }
    }

    #[test]
    fn test_mul_slice_large() {
        // Test with a buffer large enough to exercise SIMD paths
        let input: Vec<u8> = (0..256).map(|i| i as u8).collect();
        let mut out = vec![0u8; 256];
        let mut expected = vec![0u8; 256];

        for c in [2u8, 7, 42, 128, 255] {
            mul_slice_scalar(c, &input, &mut expected);
            mul_slice(c, &input, &mut out);
            assert_eq!(out, expected, "mul_slice mismatch for c={c}");
        }
    }

    #[test]
    fn test_mul_slice_xor_large() {
        let input: Vec<u8> = (0..256).map(|i| i as u8).collect();

        for c in [2u8, 7, 42, 128, 255] {
            let mut out_expected = vec![0xABu8; 256];
            let mut out_simd = out_expected.clone();
            mul_slice_xor_scalar(c, &input, &mut out_expected);
            mul_slice_xor(c, &input, &mut out_simd);
            assert_eq!(out_simd, out_expected, "mul_slice_xor mismatch for c={c}");
        }
    }

    #[test]
    fn test_mul_slice_unaligned_sizes() {
        // Test sizes that don't align to SIMD width
        for size in [1, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100] {
            let input: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let mut out = vec![0u8; size];
            let mut expected = vec![0u8; size];

            mul_slice_scalar(42, &input, &mut expected);
            mul_slice(42, &input, &mut out);
            assert_eq!(out, expected, "mul_slice mismatch for size={size}");
        }
    }

    /// Every kernel this CPU has against the scalar table, for every multiplier and tails of
    /// every length. A kernel the CPU lacks is reported, not silently passed on its fallback.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn kernels_match_scalar() {
        let kernels: [(&str, bool, MulSliceFn, MulSliceFn); 4] = [
            (
                "gfni+avx2",
                is_x86_feature_detected!("gfni") && is_x86_feature_detected!("avx2"),
                x86::gfni_avx2::<false>,
                x86::gfni_avx2::<true>,
            ),
            (
                "avx2",
                is_x86_feature_detected!("avx2"),
                x86::avx2::<false>,
                x86::avx2::<true>,
            ),
            (
                "gfni",
                is_x86_feature_detected!("gfni"),
                x86::gfni_sse::<false>,
                x86::gfni_sse::<true>,
            ),
            (
                "ssse3",
                is_x86_feature_detected!("ssse3"),
                x86::ssse3::<false>,
                x86::ssse3::<true>,
            ),
        ];
        let input: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for (name, present, mul, mul_xor) in kernels {
            if !present {
                eprintln!("{name}: not on this CPU, skipped");
                continue;
            }
            for c in 0..=255u8 {
                for len in [0usize, 1, 15, 16, 17, 31, 32, 33, 47, 63, 64, 65, 100, 1000] {
                    let input = &input[..len];
                    let (mut want, mut got) = (vec![0xABu8; len], vec![0xABu8; len]);
                    mul_slice_scalar(c, input, &mut want);
                    mul(c, input, &mut got);
                    assert_eq!(got, want, "{name} mul c={c} len={len}");
                    let (mut want, mut got) = (vec![0xABu8; len], vec![0xABu8; len]);
                    mul_slice_xor_scalar(c, input, &mut want);
                    mul_xor(c, input, &mut got);
                    assert_eq!(got, want, "{name} xor c={c} len={len}");
                }
            }
            eprintln!("{name}: matches scalar");
        }
    }
}
