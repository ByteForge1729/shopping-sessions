//! The rules' CSV dialect: `\n` ends a line, commas always separate fields, a quote
//! may only wrap a whole field, an empty field is null, and nothing is trimmed.

/// Split `data` (whole lines, header already skipped) into lines; the last line may
/// lack its `\n`. Empty lines are yielded too (the caller drops them as malformed).
pub fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match memchr(b'\n', rest) {
            Some(i) => {
                let line = &rest[..i];
                rest = &rest[i + 1..];
                Some(line)
            }
            None => {
                let line = rest;
                rest = &[];
                Some(line)
            }
        }
    })
}

#[inline]
fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// Parse one line into exactly `N` field values (quotes removed). None if malformed:
/// empty line, wrong field count, or a quote anywhere except around a whole field.
#[inline]
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.is_empty() {
        return None;
    }
    let mut out: [&[u8]; N] = [&[]; N];
    let mut count = 0;
    let mut start = 0;
    let len = line.len();
    let mut i = 0;
    loop {
        if i == len || line[i] == b',' {
            if count == N {
                return None;
            }
            out[count] = unquote(&line[start..i])?;
            count += 1;
            if i == len {
                break;
            }
            start = i + 1;
        }
        i += 1;
    }
    (count == N).then_some(out)
}

#[inline]
fn unquote(field: &[u8]) -> Option<&[u8]> {
    match field.iter().position(|&b| b == b'"') {
        None => Some(field),
        Some(0) => {
            let n = field.len();
            if n >= 2 && field[n - 1] == b'"' && !field[1..n - 1].contains(&b'"') {
                Some(&field[1..n - 1])
            } else {
                None
            }
        }
        Some(_) => None,
    }
}

/// Everything after the header line of a file (empty if the file has only a header).
pub fn body(file: &[u8]) -> &[u8] {
    match memchr(b'\n', file) {
        Some(i) => &file[i + 1..],
        None => &[],
    }
}

/// Split a body into roughly `target`-byte pieces that end on line boundaries.
pub fn chunks(body: &[u8], target: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = body;
    while rest.len() > target {
        let cut = match memchr(b'\n', &rest[target..]) {
            Some(i) => target + i + 1,
            None => rest.len(),
        };
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_are_syntax() {
        let f = fields::<3>(br#""u1",,"""#).unwrap();
        assert_eq!(f, [&b"u1"[..], &b""[..], &b""[..]]);
    }

    #[test]
    fn malformed_lines() {
        assert!(fields::<2>(b"").is_none());
        assert!(fields::<2>(b"a,b,c").is_none());
        assert!(fields::<2>(b"a").is_none());
        assert!(fields::<2>(br#"u1,u5""#).is_none());
        assert!(fields::<2>(br#"a"b,c"#).is_none());
        assert!(fields::<2>(br#"""",c"#).is_none());
    }

    #[test]
    fn spaces_and_cr_are_kept() {
        let f = fields::<2>(b" u1,1000\r").unwrap();
        assert_eq!(f, [&b" u1"[..], &b"1000\r"[..]]);
    }
}
