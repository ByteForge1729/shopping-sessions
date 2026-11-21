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

/// Parse any of the three forms. F3 values too large for i64 are None (they are
/// outside every window anyway).
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => civil(b),
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
