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

/// Field-splitting state for one line of [`records`].
struct Line<'a, const N: usize> {
    fields: [&'a [u8]; N],
    count: usize,
    bad: bool,
    start: usize,
    quotes: u32,
    opened: bool,
    last_quote: usize,
}

impl<'a, const N: usize> Line<'a, N> {
    fn new() -> Self {
        Line { fields: [&[]; N], count: 0, bad: false, start: 0, quotes: 0, opened: false, last_quote: 0 }
    }

    /// End the current field at `end` (exclusive); the next starts after it.
    #[inline]
    fn close(&mut self, data: &'a [u8], end: usize) {
        if !self.bad {
            let value = if self.quotes == 0 {
                Some(&data[self.start..end])
            } else if self.opened && self.quotes == 2 && self.last_quote + 1 == end && end - self.start >= 2 {
                Some(&data[self.start + 1..end - 1])
            } else {
                None
            };
            match value {
                Some(v) if self.count < N => {
                    self.fields[self.count] = v;
                    self.count += 1;
                }
                _ => self.bad = true,
            }
        }
        self.start = end + 1;
        self.quotes = 0;
        self.opened = false;
    }

    #[inline]
    fn finish(&mut self, f: &mut impl FnMut([&'a [u8]; N])) {
        if !self.bad && self.count == N {
            f(self.fields);
        }
        self.count = 0;
        self.bad = false;
    }
}

/// Call `f` with the `N` field values of every well-formed line in `data` (whole
/// lines, header skipped). One SIMD scan finds every `\n`, `,` and `"`; a field is
/// valid unquoted (no quote at all) or wrapped in exactly one pair of quotes.
#[inline]
pub fn records<'a, const N: usize>(data: &'a [u8], mut f: impl FnMut([&'a [u8]; N])) {
    let mut line = Line::<N>::new();
    for pos in memchr::memchr3_iter(b'\n', b',', b'"', data) {
        match data[pos] {
            b'"' => {
                if pos == line.start && line.quotes == 0 {
                    line.opened = true;
                }
                line.quotes += 1;
                line.last_quote = pos;
            }
            b',' => line.close(data, pos),
            _ => {
                line.close(data, pos);
                line.finish(&mut f);
            }
        }
    }
    if line.start < data.len() {
        line.close(data, data.len());
        line.finish(&mut f);
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

    fn all<const N: usize>(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        records::<N>(data, |f| out.push(f.iter().map(|v| v.to_vec()).collect()));
        out
    }

    #[test]
    fn records_match_fields() {
        let data: &[u8] = b"a,b\n\n\"u1\",\"\"\nu1,u5\"\nx\"y,z\nu1\n\"\"\"\",c\n p , q \r\nlast,line";
        let expected: Vec<Vec<Vec<u8>>> = lines(data)
            .filter_map(|l| fields::<2>(l))
            .map(|f| f.iter().map(|v| v.to_vec()).collect())
            .collect();
        assert_eq!(all::<2>(data), expected);
        assert_eq!(expected.len(), 4);
    }

    #[test]
    fn spaces_and_cr_are_kept() {
        let f = fields::<2>(b" u1,1000\r").unwrap();
        assert_eq!(f, [&b" u1"[..], &b"1000\r"[..]]);
    }
}
