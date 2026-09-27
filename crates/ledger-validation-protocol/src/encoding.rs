//! The deterministic binary layout shared with commit v2 and request v1 (ADR-0018):
//! big-endian integers, `field` = `u32` length + UTF-8 bytes, `opt` = `0x00` absent or
//! `0x01` + non-empty field. The decoder side is a strict cursor.

use crate::ProtocolError;

pub(crate) const TAG_ABSENT: u8 = 0;
pub(crate) const TAG_PRESENT: u8 = 1;

pub(crate) fn field(out: &mut Vec<u8>, value: &str) -> Result<(), ProtocolError> {
    let len = u32::try_from(value.len())
        .map_err(|_| ProtocolError::Invalid("field exceeds u32".into()))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

pub(crate) fn opt(out: &mut Vec<u8>, value: Option<&str>) -> Result<(), ProtocolError> {
    match value {
        None => {
            out.push(TAG_ABSENT);
            Ok(())
        }
        Some("") => Err(ProtocolError::Invalid(
            "optional fields are absent or non-empty, never empty".into(),
        )),
        Some(present) => {
            out.push(TAG_PRESENT);
            field(out, present)
        }
    }
}

pub(crate) fn u32be(out: &mut Vec<u8>, value: usize) -> Result<(), ProtocolError> {
    let value =
        u32::try_from(value).map_err(|_| ProtocolError::Invalid("count exceeds u32".into()))?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

/// Sort a list of encoded elements bytewise and drop duplicates: the canonical form of a
/// set whose element order must never reach an identity.
pub(crate) fn canonical_set(mut elements: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    elements.sort_unstable();
    elements.dedup();
    elements
}

pub(crate) struct Cursor<'a> {
    rest: &'a [u8],
    what: &'static str,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(
        bytes: &'a [u8],
        header: &[u8],
        what: &'static str,
    ) -> Result<Self, ProtocolError> {
        let rest = bytes
            .strip_prefix(header)
            .ok_or_else(|| ProtocolError::Invalid(format!("{what}: unknown header")))?;
        Ok(Self { rest, what })
    }

    fn invalid(&self, reason: &str) -> ProtocolError {
        ProtocolError::Invalid(format!("{}: {reason}", self.what))
    }

    pub(crate) fn u8(&mut self, name: &str) -> Result<u8, ProtocolError> {
        let Some((first, tail)) = self.rest.split_first() else {
            return Err(self.invalid(&format!("truncated {name}")));
        };
        self.rest = tail;
        Ok(*first)
    }

    pub(crate) fn u32(&mut self, name: &str) -> Result<u32, ProtocolError> {
        if self.rest.len() < 4 {
            return Err(self.invalid(&format!("truncated {name}")));
        }
        let value = u32::from_be_bytes(self.rest[..4].try_into().expect("four bytes"));
        self.rest = &self.rest[4..];
        Ok(value)
    }

    pub(crate) fn field(&mut self, name: &str) -> Result<String, ProtocolError> {
        let len = self.u32(&format!("length of {name}"))? as usize;
        if self.rest.len() < len {
            return Err(self.invalid(&format!("truncated {name}")));
        }
        let value = std::str::from_utf8(&self.rest[..len])
            .map_err(|_| self.invalid(&format!("{name} is not UTF-8")))?
            .to_owned();
        self.rest = &self.rest[len..];
        Ok(value)
    }

    pub(crate) fn opt(&mut self, name: &str) -> Result<Option<String>, ProtocolError> {
        match self.u8(&format!("{name} tag"))? {
            TAG_ABSENT => Ok(None),
            TAG_PRESENT => {
                let value = self.field(name)?;
                if value.is_empty() {
                    return Err(self.invalid(&format!(
                        "{name}: present-but-empty is not a valid encoding"
                    )));
                }
                Ok(Some(value))
            }
            other => Err(self.invalid(&format!("{name}: unknown optional tag {other}"))),
        }
    }

    /// A counted list must arrive in strictly ascending bytewise order of its elements'
    /// own encodings (sorted, unique). Elements are decoded by `element`, which must
    /// consume exactly one element and return its logical value; `encode` re-encodes it so
    /// the order rule can be checked without trusting the decoder's view.
    pub(crate) fn set<T>(
        &mut self,
        name: &str,
        max: usize,
        mut element: impl FnMut(&mut Self) -> Result<T, ProtocolError>,
        encode: impl Fn(&T) -> Result<Vec<u8>, ProtocolError>,
    ) -> Result<Vec<T>, ProtocolError> {
        let count = self.u32(&format!("{name} count"))? as usize;
        if count > max {
            return Err(self.invalid(&format!("more than {max} {name}")));
        }
        let mut out: Vec<T> = Vec::with_capacity(count);
        let mut previous: Option<Vec<u8>> = None;
        for _ in 0..count {
            let value = element(self)?;
            let encoded = encode(&value)?;
            if let Some(prev) = &previous
                && prev >= &encoded
            {
                return Err(self.invalid(&format!(
                    "{name} must be strictly ascending (sorted, unique)"
                )));
            }
            previous = Some(encoded);
            out.push(value);
        }
        Ok(out)
    }

    pub(crate) fn finish(self) -> Result<(), ProtocolError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(self.invalid("trailing bytes"))
        }
    }
}
