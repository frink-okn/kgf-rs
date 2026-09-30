//! Lowercase hexadecimal: the one spelling of every digest and identifier this
//! server prints — entity tags, skolem IRIs, pseudonyms, request ids.
//!
//! One implementation so the spellings cannot drift apart: a tag whose digest
//! is written in a different case from the manifest's would name different
//! bytes to anything comparing the two as text.

use std::fmt::Write as _;

/// Append `bytes` to `text` as lowercase hex.
pub(crate) fn push(text: &mut String, bytes: &[u8]) {
    text.reserve(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
}

/// `bytes` as lowercase hex.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    push(&mut text, bytes);
    text
}

/// Exactly `N` bytes written as `2N` lowercase hex digits, or `None`.
///
/// Uppercase is refused rather than accepted, because the text is also used as
/// written: a manifest's checksum that decoded here but printed differently
/// would be two spellings of one digest.
pub(crate) fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
    if text.len() != N * 2 {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0u8; N];
    for (index, pair) in text.chunks_exact(2).enumerate() {
        bytes[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_hex_round_trips_and_nothing_else_decodes() {
        let bytes = [0x00, 0x0f, 0xa5, 0xff];
        assert_eq!(encode(&bytes), "000fa5ff");
        assert_eq!(decode::<4>("000fa5ff"), Some(bytes));
        assert_eq!(decode::<4>("000FA5FF"), None);
        assert_eq!(decode::<4>("000fa5f"), None);
        assert_eq!(decode::<4>("000fa5fg"), None);
        let mut text = "id-".to_owned();
        push(&mut text, &[0xab]);
        assert_eq!(text, "id-ab");
    }
}
