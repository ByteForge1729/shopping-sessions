//! Timestamps: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 the same with `+HH:MM`/`-HH:MM`, and F3
//! ASCII digits giving epoch milliseconds. Anything else is bad (None).

pub const MS_PER_DAY: i64 = 86_400_000;

#[inline]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let (x, y) = (b[i].wrapping_sub(b'0'), b[i + 1].wrapping_sub(b'0'));
    (x < 10 && y < 10).then(|| (x * 10 + y) as i64)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
#[inline]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[inline]
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `YYYY-MM-DDTHH:MM:SS` (the first 19 bytes) as epoch milliseconds, or None.
#[inline]
fn civil(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let (mo, d) = (d2(b, 5)?, d2(b, 8)?);
    let (h, mi, s) = (d2(b, 11)?, d2(b, 14)?, d2(b, 17)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * MS_PER_DAY + ((h * 60 + mi) * 60 + s) * 1000)
}

/// Cumulative days before each month (non-leap year), index 1..=12.
const BEFORE_MONTH: [i64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

/// F1 for years 1970..=2399 via one SWAR digit check; None hands the bytes to
/// `civil`, which decides every case this skips.
#[inline]
fn fast_f1(b: &[u8]) -> Option<i64> {
    let lo = u64::from_le_bytes(b[0..8].try_into().unwrap()); // YYYY-MM-
    let hi = u64::from_le_bytes(b[8..16].try_into().unwrap()); // DDTHH:MM
    let digits_lo = lo.wrapping_sub(0x2D30_302D_3030_3030); // "0000-00-" layout
    let digits_hi = hi.wrapping_sub(0x3030_3A30_3054_3030); // "00T00:00" layout
    // every digit byte must now be 0..=9 and the punctuation exactly 0
    const DIGITS_LO: u64 = 0x00FF_FF00_FFFF_FFFF;
    const DIGITS_HI: u64 = 0xFFFF_00FF_FF00_FFFF;
    let bad = |v: u64, digit: u64| -> bool {
        (v & !digit) != 0 || ((v | v.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 & digit) != 0
    };
    if bad(digits_lo, DIGITS_LO) || bad(digits_hi, DIGITS_HI) || b[16] != b':' {
        return None;
    }
    let d = |v: u64, i: u32| ((v >> (8 * i)) & 0xFF) as i64;
    let y = d(digits_lo, 0) * 1000 + d(digits_lo, 1) * 100 + d(digits_lo, 2) * 10 + d(digits_lo, 3);
    let mo = d(digits_lo, 5) * 10 + d(digits_lo, 6);
    let day = d(digits_hi, 0) * 10 + d(digits_hi, 1);
    let h = d(digits_hi, 3) * 10 + d(digits_hi, 4);
    let mi = d(digits_hi, 6) * 10 + d(digits_hi, 7);
    let (s1, s2) = (b[17].wrapping_sub(b'0'), b[18].wrapping_sub(b'0'));
    if s1 > 9 || s2 > 9 || !(1970..=2399).contains(&y) || !(1..=12).contains(&mo) || h > 23 || mi > 59 {
        return None;
    }
    let s = (s1 * 10 + s2) as i64;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    if s > 59 || day < 1 || day > days_in_month(y, mo) {
        return None;
    }
    let yd = y - 1970;
    let days = yd * 365 + (yd + 1) / 4 - (yd + 69) / 100 + (yd + 369) / 400 + BEFORE_MONTH[mo as usize]
        + (mo > 2 && leap) as i64
        + day
        - 1;
    Some(days * MS_PER_DAY + ((h * 60 + mi) * 60 + s) * 1000)
}

/// Parse any of the three forms. F3 values too large for i64 are None (they are
/// outside every window anyway).
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => fast_f1(b).or_else(|| civil(b)),
        25 if b[19] == b'+' || b[19] == b'-' => {
            if b[22] != b':' {
                return None;
            }
            let (oh, om) = (d2(b, 20)?, d2(b, 23)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let offset = (oh * 60 + om) * 60_000;
            let local = civil(b)?;
            Some(if b[19] == b'+' { local - offset } else { local + offset })
        }
        0 => None,
        _ => digits(b),
    }
}

/// ASCII digits only, as an integer (leading zeros fine); None on any other byte or overflow.
#[inline]
pub fn digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in b {
        let x = c.wrapping_sub(b'0');
        if x >= 10 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(x as i64)?;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_agree() {
        let f1 = parse(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse(b"1791071890000"), Some(f1));
        assert_eq!(parse(b"0001791071890000"), Some(f1));
        assert_eq!(parse(b"2026-10-03T23:58:10-00:00"), Some(f1));
    }

    #[test]
    fn fast_path_agrees_with_civil() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let y = 1970 + state % 430;
            let mo = 1 + (state >> 9) % 13;
            let d = (state >> 13) % 32;
            let (h, mi, s) = ((state >> 18) % 25, (state >> 23) % 61, (state >> 29) % 61);
            let text = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
            let b = text.as_bytes();
            if let Some(v) = fast_f1(b) {
                assert_eq!(Some(v), civil(b), "{text}");
            } else {
                assert_eq!(civil(b).filter(|_| (1970..=2399).contains(&(y as i64))), None, "{text}");
            }
        }
    }

    #[test]
    fn bad_forms() {
        for bad in [
            &b"2026-10-03 23:58:10Z"[..], b"2026-10-03t23:58:10Z", b"2026-10-03T23:58:10z",
            b"2026-10-03T23:58:10.5Z", b"2026-13-03T23:58:10Z", b"2026-02-30T00:00:00Z",
            b"2026-02-29T00:00:00Z", b"2026-10-03T24:00:00Z", b"2026-10-03T23:59:60Z",
            b"2026-10-03T23:58:10+24:00", b"2026-10-03T23:58:10+05:60", b"2026-10-03T23:58:10+0200",
            b"1791071890000.0", b"1.79e12", b"-5", b"", b"2026-10-03T23:58:10",
            "２０２6-10-03T23:58:10Z".as_bytes(), "٢٥٠".as_bytes(), b"123456789012345678901",
        ] {
            assert_eq!(parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(parse(b"2028-02-29T00:00:00Z").is_some());
        assert!(parse(b"86400000").is_some());
    }
}
