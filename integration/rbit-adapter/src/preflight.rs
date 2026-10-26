//! Allocation-free admission before rbit's recursive/allocating bencode parser.
//! Canonical dictionaries are required so duplicate privacy/path keys cannot
//! mean different things to the validation pass and the upstream parser.

pub(crate) const MAX_DEPTH: usize = 64;
/// About 30 per file for 4096 files with deep paths, with headroom.
pub(crate) const MAX_ITEMS: usize = 262_144;

struct Scan<'a> {
    input: &'a [u8],
    at: usize,
    items: usize,
}

impl<'a> Scan<'a> {
    fn item(&mut self) -> Result<(), &'static str> {
        self.items += 1;
        if self.items > MAX_ITEMS {
            return Err("bencode item limit exceeded");
        }
        Ok(())
    }

    fn string(&mut self) -> Result<&'a [u8], &'static str> {
        self.item()?;
        let start = self.at;
        let mut length = 0usize;
        while let Some(digit @ b'0'..=b'9') = self.input.get(self.at) {
            length = length
                .checked_mul(10)
                .and_then(|v| v.checked_add((digit - b'0') as usize))
                .ok_or("bencode string length overflow")?;
            self.at += 1;
        }
        if self.at == start
            || self.input.get(self.at) != Some(&b':')
            || (self.at > start + 1 && self.input[start] == b'0')
        {
            return Err("invalid bencode string length");
        }
        self.at += 1;
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.input.len())
            .ok_or("truncated bencode string")?;
        let bytes = &self.input[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn value(&mut self, depth: usize) -> Result<(), &'static str> {
        if depth > MAX_DEPTH {
            return Err("bencode depth limit exceeded");
        }
        match self.input.get(self.at) {
            Some(b'0'..=b'9') => {
                self.string()?;
            }
            Some(b'i') => {
                self.item()?;
                self.at += 1;
                let start = self.at;
                if self.input.get(self.at) == Some(&b'-') {
                    self.at += 1;
                }
                let digits = self.at;
                while matches!(self.input.get(self.at), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
                if self.at == digits
                    || self.input.get(self.at) != Some(&b'e')
                    || (self.at > digits + 1 && self.input[digits] == b'0')
                    || (digits > start && self.input[digits] == b'0')
                {
                    return Err("invalid bencode integer");
                }
                std::str::from_utf8(&self.input[start..self.at])
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok())
                    .ok_or("bencode integer overflow")?;
                self.at += 1;
            }
            Some(b'l' | b'd') => {
                self.item()?;
                let dict = self.input[self.at] == b'd';
                self.at += 1;
                let mut previous: Option<&[u8]> = None;
                while self.input.get(self.at) != Some(&b'e') {
                    if dict {
                        let key = self.string()?;
                        if previous.is_some_and(|prior| prior >= key) {
                            return Err("bencode keys must be sorted and unique");
                        }
                        previous = Some(key);
                    }
                    self.value(depth + 1)?;
                }
                self.at += 1;
            }
            _ => return Err("invalid or truncated bencode"),
        }
        Ok(())
    }
}

/// Return the first value's extent and item count. Used for BEP-9's bencoded
/// header followed by raw piece bytes, which must never enter the parser.
pub(crate) fn prefix(input: &[u8]) -> Result<(usize, usize), &'static str> {
    let mut scan = Scan {
        input,
        at: 0,
        items: 0,
    };
    scan.value(0)?;
    Ok((scan.at, scan.items))
}

pub(crate) fn validate(input: &[u8]) -> Result<usize, &'static str> {
    let (end, items) = prefix(input)?;
    if end != input.len() {
        return Err("trailing bencode data");
    }
    Ok(items)
}

// Borrowed accessors are used only after validate/prefix; each still bounds
// traversal so a future caller cannot accidentally bypass the defensive scan.
pub(crate) fn field<'a>(dict: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    if dict.first() != Some(&b'd') {
        return None;
    }
    let mut scan = Scan {
        input: dict,
        at: 1,
        items: 0,
    };
    while dict.get(scan.at) != Some(&b'e') {
        let name = scan.string().ok()?;
        let start = scan.at;
        scan.value(1).ok()?;
        if name == key {
            return Some(&dict[start..scan.at]);
        }
    }
    None
}

pub(crate) fn string(value: &[u8]) -> Option<&[u8]> {
    let mut scan = Scan {
        input: value,
        at: 0,
        items: 0,
    };
    let bytes = scan.string().ok()?;
    (scan.at == value.len()).then_some(bytes)
}

pub(crate) fn integer(value: &[u8]) -> Option<i64> {
    if value.first() != Some(&b'i') || value.last() != Some(&b'e') {
        return None;
    }
    std::str::from_utf8(&value[1..value.len() - 1])
        .ok()?
        .parse()
        .ok()
}

pub(crate) fn list(value: &[u8], cap: usize) -> Result<Vec<&[u8]>, &'static str> {
    if value.first() != Some(&b'l') {
        return Err("expected bencode list");
    }
    let mut scan = Scan {
        input: value,
        at: 1,
        items: 0,
    };
    let mut values = Vec::new();
    while value.get(scan.at) != Some(&b'e') {
        if values.len() >= cap {
            return Err("bencode list exceeds admission limit");
        }
        let start = scan.at;
        scan.value(1)?;
        values.push(&value[start..scan.at]);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_hostile_shapes_without_allocating_their_claimed_data() {
        for input in [
            &b"99999999999999999999999999999999999999999:x"[..],
            b"3:a",
            b"i01e",
            b"i-0e",
            b"i9223372036854775808e",
            b"d1:ai1e1:ai2ee",
            b"d1:bi1e1:ai2ee",
            b"deextra",
        ] {
            assert!(validate(input).is_err());
        }
        let mut deep = vec![b'l'; MAX_DEPTH + 2];
        deep.extend(vec![b'e'; MAX_DEPTH + 2]);
        assert!(validate(&deep).is_err());
        let mut many = b"l".to_vec();
        for _ in 0..MAX_ITEMS {
            many.extend_from_slice(b"0:");
        }
        many.push(b'e');
        assert!(validate(&many).is_err());
    }
    #[test]
    fn bounded_views_preserve_exact_raw_components_and_header_extent() {
        let value = b"d1:ali1e3:a/bee";
        validate(value).unwrap();
        let children = list(field(value, b"a").unwrap(), 2).unwrap();
        assert_eq!(integer(children[0]), Some(1));
        assert_eq!(string(children[1]), Some(&b"a/b"[..]));
        assert_eq!(prefix(b"dearbitrary raw").unwrap().0, 2);
    }
}
