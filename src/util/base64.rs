//! Base64, encode and decode.

// Standard base64 (RFC 4648) with padding. Only the alphabet Swish and PEM use, no
// URL-safe variant.
const ENCODE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Debug)]
/// Why a base64 string could not be decoded.
pub struct DecodeError(&'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Encodes bytes as standard base64 with padding.
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;

        out.push(ENCODE[(n >> 18 & 0x3f) as usize] as char);
        out.push(ENCODE[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ENCODE[(n >> 6 & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ENCODE[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

// Whitespace is skipped so PEM bodies can be passed in with their line breaks intact.
/// Decodes standard base64, rejecting anything malformed.
pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
    let mut symbols: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();

    // Padding is mandatory, as RFC 4648 requires and every PEM body carries.
    if !symbols.len().is_multiple_of(4) {
        return Err(DecodeError("base64 input is not a multiple of four symbols"));
    }
    let mut padding = 0;
    while padding < 2 && symbols.last() == Some(&b'=') {
        symbols.pop();
        padding += 1;
    }
    if symbols.contains(&b'=') {
        return Err(DecodeError("base64 padding appears before the end of the input"));
    }

    let mut out = Vec::with_capacity(symbols.len() / 4 * 3 + 2);
    for chunk in symbols.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (symbol_value(b)? as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

fn symbol_value(b: u8) -> Result<u8, DecodeError> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(DecodeError("invalid base64 symbol")),
    }
}
