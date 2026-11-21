//! Fast line splitting for the rules' CSV dialect.
//!
//! Each 64-byte block gives two bitmasks with SIMD compares: separators (`,` and
//! `\n`) and quotes. Only separators are visited. Quotes are counted, never visited:
//! a line is well formed exactly when its quote count is twice the number of fields
//! that start and end with a quote and are at least two bytes long, because each
//! such field holds two of the line's quotes at its ends, so any other quote would
//! make the count larger.

const BLOCK: usize = 64;

/// (separator mask, quote mask) for 64 bytes.
#[inline(always)]
fn masks(b: &[u8; BLOCK]) -> (u64, u64) {
    #[cfg(target_arch = "x86_64")]
    {
        sse2(b)
    }
    #[cfg(target_arch = "aarch64")]
    {
        neon(b)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        scalar(b)
    }
}

#[allow(dead_code)]
fn scalar(b: &[u8; BLOCK]) -> (u64, u64) {
    let (mut sep, mut quote) = (0u64, 0u64);
    for (i, &c) in b.iter().enumerate() {
        sep |= ((c == b',' || c == b'\n') as u64) << i;
        quote |= ((c == b'"') as u64) << i;
    }
    (sep, quote)
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn sse2(b: &[u8; BLOCK]) -> (u64, u64) {
    use std::arch::x86_64::*;
    // SSE2 is part of every x86_64 CPU.
    unsafe {
        let (comma, nl, q) = (_mm_set1_epi8(b',' as i8), _mm_set1_epi8(b'\n' as i8), _mm_set1_epi8(b'"' as i8));
        let (mut sep, mut quote) = (0u64, 0u64);
        for k in 0..4 {
            let v = _mm_loadu_si128(b.as_ptr().add(16 * k) as *const __m128i);
            let s = _mm_or_si128(_mm_cmpeq_epi8(v, comma), _mm_cmpeq_epi8(v, nl));
            sep |= (_mm_movemask_epi8(s) as u16 as u64) << (16 * k);
            quote |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, q)) as u16 as u64) << (16 * k);
        }
        (sep, quote)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn neon(b: &[u8; BLOCK]) -> (u64, u64) {
    use std::arch::aarch64::*;
    unsafe {
        let weights: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let w = vld1q_u8(weights.as_ptr());
        let bits = |m: uint8x16_t| -> u64 {
            let x = vandq_u8(m, w);
            (vaddv_u8(vget_low_u8(x)) as u64) | ((vaddv_u8(vget_high_u8(x)) as u64) << 8)
        };
        let (comma, nl, q) = (vdupq_n_u8(b','), vdupq_n_u8(b'\n'), vdupq_n_u8(b'"'));
        let (mut sep, mut quote) = (0u64, 0u64);
        for k in 0..4 {
            let v = vld1q_u8(b.as_ptr().add(16 * k));
            sep |= bits(vorrq_u8(vceqq_u8(v, comma), vceqq_u8(v, nl))) << (16 * k);
            quote |= bits(vceqq_u8(v, q)) << (16 * k);
        }
        (sep, quote)
    }
}

/// Bits strictly below `i` (i in 0..=64).
#[inline(always)]
fn below(i: u32) -> u64 {
    if i >= 64 {
        u64::MAX
    } else {
        (1u64 << i) - 1
    }
}

struct Line<'a, const N: usize> {
    fields: [&'a [u8]; N],
    count: usize,
    wrapped: u32,
    quotes: u32,
    start: usize,
}

impl<'a, const N: usize> Line<'a, N> {
    #[inline(always)]
    fn field(&mut self, data: &'a [u8], end: usize) {
        let (start, n) = (self.start, self.count);
        self.start = end + 1;
        self.count = n + 1;
        if n >= N {
            return;
        }
        let v = &data[start..end];
        self.fields[n] = if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
            self.wrapped += 1;
            &v[1..v.len() - 1]
        } else {
            v
        };
    }

    #[inline(always)]
    fn finish(&mut self, f: &mut impl FnMut([&'a [u8]; N])) {
        if self.count == N && self.quotes == 2 * self.wrapped {
            f(self.fields);
        }
        self.count = 0;
        self.wrapped = 0;
        self.quotes = 0;
    }
}

/// Call `f` with the `N` field values of every well-formed line in `data` (whole
/// lines, header skipped; the last line may lack its `\n`).
#[inline]
pub fn records<'a, const N: usize>(data: &'a [u8], mut f: impl FnMut([&'a [u8]; N])) {
    let mut line = Line::<'a, N> { fields: [&[]; N], count: 0, wrapped: 0, quotes: 0, start: 0 };
    let mut base = 0usize;
    let mut tail = [0u8; BLOCK];
    while base < data.len() {
        let (sep, quote) = if base + BLOCK <= data.len() {
            masks(data[base..base + BLOCK].try_into().unwrap())
        } else {
            let n = data.len() - base;
            tail[..n].copy_from_slice(&data[base..]);
            tail[n..].fill(0);
            masks(&tail)
        };
        let mut bits = sep;
        let mut done = 0u32; // quote bits below this position are counted
        while bits != 0 {
            let i = bits.trailing_zeros();
            bits &= bits - 1;
            line.quotes += (quote & below(i) & !below(done)).count_ones();
            done = i + 1;
            let pos = base + i as usize;
            line.field(data, pos);
            if data[pos] == b'\n' {
                line.finish(&mut f);
            }
        }
        line.quotes += (quote & !below(done)).count_ones();
        base += BLOCK;
    }
    if data.last().is_some_and(|&b| b != b'\n') {
        line.field(data, data.len());
        line.finish(&mut f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv;

    fn reference<const N: usize>(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
        csv::lines(data).filter_map(|l| csv::fields::<N>(l)).map(|f| f.iter().map(|v| v.to_vec()).collect()).collect()
    }

    fn fast<const N: usize>(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        records::<N>(data, |f| out.push(f.iter().map(|v| v.to_vec()).collect()));
        out
    }

    #[test]
    fn matches_reference_on_random_lines() {
        let alphabet = [b'a', b'b', b',', b'"', b' ', b'\r', b'\n', b'1'];
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 300) as usize;
            let data: Vec<u8> = (0..len).map(|_| alphabet[(next() % 8) as usize]).collect();
            assert_eq!(fast::<2>(&data), reference::<2>(&data), "{:?}", String::from_utf8_lossy(&data));
            assert_eq!(fast::<3>(&data), reference::<3>(&data), "{:?}", String::from_utf8_lossy(&data));
        }
    }

    #[test]
    fn quoted_fields() {
        let data = b"\"a\",b\n\"\",\"\"\n\"\"\"\",x\n\",\"\nlast,\"q\"";
        assert_eq!(fast::<2>(data), reference::<2>(data));
        assert_eq!(fast::<2>(data).len(), 3);
    }
}
