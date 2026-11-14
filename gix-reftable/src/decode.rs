use gix_error::{OptionExt, Result, ResultExt, corruption};

pub(crate) fn varint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0_u64;
    loop {
        let (&byte, rest) = input
            .split_first()
            .ok_or_raise(|| corruption("Truncated variable-length integer"))?;
        *input = rest;
        value = value
            .checked_mul(128)
            .and_then(|value| value.checked_add(u64::from(byte & 0x7f)))
            .ok_or_raise(|| corruption("Variable-length integer overflow"))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        value = value
            .checked_add(1)
            .ok_or_raise(|| corruption("Variable-length integer overflow"))?;
    }
}

pub(crate) fn size(value: u64) -> Result<usize> {
    usize::try_from(value).or_raise(|| corruption("Record length does not fit this address space"))
}

pub(crate) fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let (value, rest) = input
        .split_at_checked(len)
        .ok_or_raise(|| corruption("Truncated record data"))?;
    *input = rest;
    Ok(value)
}

pub(crate) fn string<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = size(varint(input)?)?;
    take(input, len)
}

pub(crate) fn object_id<'a>(input: &mut &'a [u8], kind: gix_hash::Kind) -> Result<&'a gix_hash::oid> {
    gix_hash::oid::try_from_bytes(take(input, kind.len_in_bytes())?)
        .or_raise(|| corruption("Could not decode the record's object ID"))
}

#[cfg(test)]
mod tests {
    use super::varint;

    #[test]
    fn integers_use_the_git_offset_encoding() -> gix_error::TestResult {
        for (bytes, expected) in [
            (b"\0".as_slice(), 0),
            (b"\x7f", 127),
            (b"\x80\0", 128),
            (b"\x80\x7f", 255),
            (b"\x81\0", 256),
            (b"\xff\x7f", 16_511),
            (b"\x80\x80\0", 16_512),
            (b"\x80\xfe\xfe\xfe\xfe\xfe\xfe\xfe\xfe\x7f", u64::MAX),
        ] {
            let mut input = bytes;
            assert_eq!(varint(&mut input)?, expected);
            assert!(input.is_empty(), "only the encoded integer is consumed");
        }
        Ok(())
    }

    #[test]
    fn truncated_and_overflowing_integers_are_errors() {
        for bytes in [
            b"".as_slice(),
            b"\x80",
            b"\x80\x80",
            b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\x7f",
        ] {
            assert!(varint(&mut &*bytes).is_err(), "{bytes:?} cannot encode a u64");
        }
    }
}
