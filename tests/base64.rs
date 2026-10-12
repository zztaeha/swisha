use swisha::util::base64::{decode, encode};

// RFC 4648 section 10.
const VECTORS: [(&str, &str); 7] = [
    ("", ""),
    ("f", "Zg=="),
    ("fo", "Zm8="),
    ("foo", "Zm9v"),
    ("foob", "Zm9vYg=="),
    ("fooba", "Zm9vYmE="),
    ("foobar", "Zm9vYmFy"),
];

#[test]
fn matches_rfc4648_vectors() {
    for (plain, encoded) in VECTORS {
        assert_eq!(encode(plain.as_bytes()), encoded, "encoding {plain:?}");
        assert_eq!(decode(encoded).unwrap(), plain.as_bytes(), "decoding {encoded:?}");
    }
}

#[test]
fn round_trips_every_byte_length_up_to_a_kilobyte() {
    for len in 0..1024usize {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
        assert_eq!(decode(&encode(&data)).unwrap(), data, "length {len}");
    }
}

#[test]
fn round_trips_all_byte_values() {
    let all: Vec<u8> = (0..=255u8).collect();
    assert_eq!(decode(&encode(&all)).unwrap(), all);
}

#[test]
fn skips_whitespace_so_pem_line_breaks_are_accepted() {
    assert_eq!(decode("Zm9v\nYmFy").unwrap(), b"foobar");
    assert_eq!(decode("Zm9v YmFy\r\n").unwrap(), b"foobar");
    assert_eq!(decode("  Zm9vYmE=  ").unwrap(), b"fooba");
}

#[test]
fn rejects_malformed_input() {
    assert!(decode("Zm9vYmF").is_err());       // unpadded, 7 symbols
    assert!(decode("Zg=").is_err());           // truncated padding
    assert!(decode("Zm9=vYmA").is_err());      // padding before the end
    assert!(decode("Zm9v!!!!").is_err());      // invalid symbol
    assert!(decode("Zm9vYmFy-_-_").is_err());  // URL-safe alphabet is not accepted
}

// Unpadded input is refused: accepting it would quietly widen what counts as a valid PEM body.
#[test]
fn requires_padding() {
    assert!(decode("Zg").is_err());
    assert!(decode("Zm8").is_err());
    assert_eq!(decode("Zg==").unwrap(), b"f");
    assert_eq!(decode("Zm8=").unwrap(), b"fo");
}
