use swisha::util::hex::upper;

// Swish expects the signing certificate serial and the payout instruction UUID as uppercase hex
// with no separators. A lowercase or zero-trimmed rendering is rejected at the API.
#[test]
fn every_byte_renders_as_two_uppercase_digits() {
    assert_eq!(upper(&[0x00]), "00", "a zero byte keeps both digits");
    assert_eq!(upper(&[0x0f]), "0F", "the high nibble is not trimmed");
    assert_eq!(upper(&[0xff]), "FF");
    assert_eq!(upper(&[0xde, 0xad, 0xbe, 0xef]), "DEADBEEF");
    assert_eq!(upper(&[]), "");
}

#[test]
fn the_whole_byte_range_round_trips_to_the_expected_text() {
    let all: Vec<u8> = (0..=255).collect();
    let rendered = upper(&all);
    assert_eq!(rendered.len(), 512, "two characters per byte, always");
    assert!(
        rendered.chars().all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
        "no lowercase and no separators"
    );
    for (i, b) in all.iter().enumerate() {
        assert_eq!(&rendered[i * 2..i * 2 + 2], &format!("{b:02X}"), "byte {b}");
    }
}

#[test]
fn nibbles_are_not_swapped() {
    // 0x1F and 0xF1 differ only in nibble order, so a swap would render them identically.
    assert_eq!(upper(&[0x1f]), "1F");
    assert_eq!(upper(&[0xf1]), "F1");
}
