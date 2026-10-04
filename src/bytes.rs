//! Big-endian integers and varints, read without panicking: every reader
//! returns `None` when the bytes run out.

/// A varint is at most nine bytes.
const VARINT_MAX_LEN: usize = 9;

pub(crate) fn u8_at(bytes: &[u8], at: usize) -> Option<u8> {
    bytes.get(at).copied()
}

pub(crate) fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(array_at(bytes, at)?))
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(array_at(bytes, at)?))
}

fn array_at<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// A big-endian two's-complement integer of 1 to 8 bytes, sign-extended
/// (0 for any other length).
pub(crate) fn signed_be(bytes: &[u8]) -> i64 {
    if bytes.is_empty() || bytes.len() > 8 {
        return 0;
    }
    let unsigned = bytes
        .iter()
        .fold(0u64, |value, &byte| (value << 8) | u64::from(byte));
    let unused_bits = 64 - 8 * bytes.len() as u32;
    // Shift the sign bit to the top, then back down arithmetically.
    ((unsigned << unused_bits) as i64) >> unused_bits
}

/// The varint at `at`, and its length in bytes.
///
/// Up to eight bytes with the high bit set contribute seven bits each; the
/// ninth, if reached, contributes all eight. Big-endian.
pub(crate) fn varint_at(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for index in 0..VARINT_MAX_LEN {
        let byte = *bytes.get(at.checked_add(index)?)?;
        if index == VARINT_MAX_LEN - 1 {
            return Some(((value << 8) | u64::from(byte), VARINT_MAX_LEN));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_of_every_length() {
        assert_eq!(varint_at(&[0x00], 0), Some((0, 1)));
        assert_eq!(varint_at(&[0x7f], 0), Some((127, 1)));
        assert_eq!(varint_at(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint_at(&[0xff; 9], 0), Some((u64::MAX, 9)));
        assert_eq!(varint_at(&[0x81], 0), None);
    }

    #[test]
    fn signed_values_are_sign_extended() {
        assert_eq!(signed_be(&[0xff]), -1);
        assert_eq!(signed_be(&[0x7f, 0xff]), 32767);
        assert_eq!(signed_be(&[0x80, 0, 0]), -8_388_608);
        assert_eq!(signed_be(&[0xff; 8]), -1);
    }
}
