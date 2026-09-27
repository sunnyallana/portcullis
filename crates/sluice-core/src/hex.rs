//! Lowercase hex encoding.
//!
//! `sha2` returns a generic array that does not implement `LowerHex`, and a
//! dependency for sixteen lines of code is not worth the supply-chain surface.

/// Encode bytes as lowercase hexadecimal.
pub(crate) fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_known_bytes() {
        assert_eq!(super::encode(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        assert_eq!(super::encode(&[]), "");
    }
}
