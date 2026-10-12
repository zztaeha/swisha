//! Hexadecimal rendering.

// Uppercase hex with no separators. Used for the Swish signing certificate serial and for
// payout instruction UUIDs, both of which Swish expects in exactly this form.
/// Renders bytes as uppercase hexadecimal.
pub fn upper(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
