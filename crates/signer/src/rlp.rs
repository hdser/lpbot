//! Recursive length prefix, the encoding Ethereum transactions are signed
//! and sent in: just what a type-2 transaction needs, strict on reading.

use thiserror::Error;

/// Why bytes are not the RLP that was expected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RlpError {
    #[error("the RLP ends early")]
    Short,
    #[error("the RLP has {0} byte(s) after its last item")]
    Trailing(usize),
    #[error("the RLP is not in its shortest form")]
    NotCanonical,
    #[error("expected {0} in the RLP")]
    Expected(&'static str),
}

/// One decoded item: a byte string or a list of items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item<'a> {
    Bytes(&'a [u8]),
    List(Vec<Item<'a>>),
}

impl<'a> Item<'a> {
    pub fn bytes(&self) -> Result<&'a [u8], RlpError> {
        match self {
            Item::Bytes(bytes) => Ok(bytes),
            Item::List(_) => Err(RlpError::Expected("a byte string")),
        }
    }

    pub fn list(&self) -> Result<&[Item<'a>], RlpError> {
        match self {
            Item::List(items) => Ok(items),
            Item::Bytes(_) => Err(RlpError::Expected("a list")),
        }
    }

    /// An unsigned integer: big-endian, no leading zero byte, at most
    /// `max_bytes` long.
    pub fn uint(&self, max_bytes: usize) -> Result<&'a [u8], RlpError> {
        let bytes = self.bytes()?;
        if bytes.first() == Some(&0) {
            return Err(RlpError::NotCanonical);
        }
        if bytes.len() > max_bytes {
            return Err(RlpError::Expected("a smaller integer"));
        }
        Ok(bytes)
    }
}

/// The big-endian bytes of `value` without leading zeros: what RLP encodes
/// for an integer. Zero is the empty string.
pub fn trimmed(value: &[u8]) -> &[u8] {
    let first = value.iter().position(|b| *b != 0).unwrap_or(value.len());
    &value[first..]
}

fn length_prefix(out: &mut Vec<u8>, short: u8, long: u8, len: usize) {
    if len <= 55 {
        out.push(short + len as u8);
    } else {
        let len_bytes = len.to_be_bytes();
        let len_bytes = trimmed(&len_bytes);
        out.push(long + len_bytes.len() as u8);
        out.extend_from_slice(len_bytes);
    }
}

/// Appends `bytes` as an RLP byte string.
pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        out.push(bytes[0]);
    } else {
        length_prefix(out, 0x80, 0xb7, bytes.len());
        out.extend_from_slice(bytes);
    }
}

/// Appends an unsigned integer given as big-endian bytes.
pub fn put_uint(out: &mut Vec<u8>, big_endian: &[u8]) {
    put_bytes(out, trimmed(big_endian));
}

/// Wraps already encoded items as one RLP list.
pub fn list(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 9);
    length_prefix(&mut out, 0xc0, 0xf7, payload.len());
    out.extend_from_slice(payload);
    out
}

/// Decodes exactly one item, which must take all of `input`.
pub fn decode(input: &[u8]) -> Result<Item<'_>, RlpError> {
    let (item, rest) = item(input)?;
    if !rest.is_empty() {
        return Err(RlpError::Trailing(rest.len()));
    }
    Ok(item)
}

fn long_length(input: &[u8], count: usize) -> Result<(usize, &[u8]), RlpError> {
    if input.len() < count {
        return Err(RlpError::Short);
    }
    let (bytes, rest) = input.split_at(count);
    if bytes.first() == Some(&0) || count > std::mem::size_of::<usize>() {
        return Err(RlpError::NotCanonical);
    }
    let len = bytes
        .iter()
        .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
    if len <= 55 {
        return Err(RlpError::NotCanonical);
    }
    Ok((len, rest))
}

fn take(input: &[u8], len: usize) -> Result<(&[u8], &[u8]), RlpError> {
    if input.len() < len {
        return Err(RlpError::Short);
    }
    Ok(input.split_at(len))
}

fn item(input: &[u8]) -> Result<(Item<'_>, &[u8]), RlpError> {
    let (&first, rest) = input.split_first().ok_or(RlpError::Short)?;
    match first {
        0x00..=0x7f => Ok((Item::Bytes(&input[..1]), rest)),
        0x80..=0xb7 => {
            let (bytes, rest) = take(rest, usize::from(first - 0x80))?;
            if bytes.len() == 1 && bytes[0] < 0x80 {
                return Err(RlpError::NotCanonical);
            }
            Ok((Item::Bytes(bytes), rest))
        }
        0xb8..=0xbf => {
            let (len, rest) = long_length(rest, usize::from(first - 0xb7))?;
            let (bytes, rest) = take(rest, len)?;
            Ok((Item::Bytes(bytes), rest))
        }
        0xc0..=0xf7 => {
            let (payload, rest) = take(rest, usize::from(first - 0xc0))?;
            Ok((Item::List(items(payload)?), rest))
        }
        0xf8..=0xff => {
            let (len, rest) = long_length(rest, usize::from(first - 0xf7))?;
            let (payload, rest) = take(rest, len)?;
            Ok((Item::List(items(payload)?), rest))
        }
    }
}

fn items(mut payload: &[u8]) -> Result<Vec<Item<'_>>, RlpError> {
    let mut out = Vec::new();
    while !payload.is_empty() {
        let (next, rest) = item(payload)?;
        out.push(next);
        payload = rest;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_bytes(&mut out, b);
        out
    }

    #[test]
    fn strings_and_lists_encode_as_the_yellow_paper_examples() {
        // The examples of the Ethereum RLP specification.
        assert_eq!(bytes(b"dog"), [0x83, b'd', b'o', b'g']);
        assert_eq!(bytes(b""), [0x80]);
        assert_eq!(bytes(&[0x0f]), [0x0f]);
        assert_eq!(bytes(&[0x04, 0x00]), [0x82, 0x04, 0x00]);
        let mut cat_dog = bytes(b"cat");
        cat_dog.extend(bytes(b"dog"));
        assert_eq!(
            list(&cat_dog),
            [0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']
        );
        assert_eq!(list(&[]), [0xc0]);
        let lorem = b"Lorem ipsum dolor sit amet, consectetur adipisicing elit";
        let encoded = bytes(lorem);
        assert_eq!(&encoded[..2], &[0xb8, 0x38]);
        assert_eq!(&encoded[2..], lorem);
        let mut uint = Vec::new();
        put_uint(&mut uint, &1024u64.to_be_bytes());
        assert_eq!(uint, [0x82, 0x04, 0x00]);
        let mut zero = Vec::new();
        put_uint(&mut zero, &0u64.to_be_bytes());
        assert_eq!(zero, [0x80]);
    }

    #[test]
    fn decoding_reads_back_what_was_written_and_refuses_what_is_not_shortest() {
        let mut payload = bytes(b"cat");
        payload.extend(list(&bytes(b"dog")));
        payload.extend(bytes(&[0u8; 60]));
        let encoded = list(&payload);
        let item = decode(&encoded).unwrap();
        let parts = item.list().unwrap();
        assert_eq!(parts[0].bytes().unwrap(), b"cat");
        assert_eq!(parts[1].list().unwrap()[0].bytes().unwrap(), b"dog");
        assert_eq!(parts[2].bytes().unwrap(), &[0u8; 60]);

        assert_eq!(decode(&[0x81, 0x05]), Err(RlpError::NotCanonical));
        assert_eq!(
            decode(&[0xb8, 0x05, 1, 2, 3, 4, 5]),
            Err(RlpError::NotCanonical)
        );
        assert_eq!(decode(&[0x83, b'd', b'o']), Err(RlpError::Short));
        assert_eq!(decode(&[0x05, 0x06]), Err(RlpError::Trailing(1)));
        assert_eq!(Item::Bytes(&[0, 1]).uint(32), Err(RlpError::NotCanonical));
        assert!(Item::Bytes(&[1; 33]).uint(32).is_err());
        assert_eq!(trimmed(&[0, 0, 7, 0]), &[7, 0]);
    }
}
