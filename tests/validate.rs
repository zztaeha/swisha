use swisha::domain::validate::{
    is_valid_message, is_valid_personnummer,
    is_whole_ore, normalize_phone, normalize_ssn, personnummer_luhn_valid, validate_phone,
};

#[test]
fn phone_normalize_strips_leading_zero() {
    assert_eq!(normalize_phone("0701234567"), "46701234567");
}

#[test]
fn phone_normalize_discards_separators_and_symbols() {
    for written in ["070-123 45 67", "+46 70 123 45 67", "(070) 1234567", "070.123.4567"] {
        assert_eq!(normalize_phone(written), "46701234567", "input {written:?}");
    }
}

// Only a leading zero is a national prefix. Stripping any other leading digit would silently
// send the payout to a different number.
#[test]
fn phone_normalize_only_treats_a_leading_zero_as_a_prefix() {
    assert_eq!(normalize_phone("46701234567"), "46701234567");
    assert_eq!(normalize_phone("00701234567"), "460701234567");
    assert_eq!(normalize_phone(""), "");
}

// Swedes write the trunk 0 in brackets after the country code, and it is dropped, as dialing drops
// it. Any other 0 after 46 is no Swedish number, and is refused rather than sent.
#[test]
fn phone_the_trunk_zero_after_46_is_dropped_and_nothing_else_starts_with_it() {
    for written in ["+46 (0)70-123 45 67", "+46 0701234567", "460701234567"] {
        assert_eq!(normalize_phone(written), "46701234567", "input {written:?}");
    }
    assert!(!validate_phone("460701234567"));
    assert!(!validate_phone(&normalize_phone("00701234567")), "a doubled 0 is no number");
}

// 00 is how Swedes dial abroad, so 0046 is Sweden written internationally, trunk 0 or not. 00
// before any other country code is no Swedish number, and is refused rather than sent.
#[test]
fn phone_the_international_prefix_before_46_is_dropped() {
    for written in ["0046 70 123 45 67", "0046701234567", "0046 (0)70-123 45 67"] {
        assert_eq!(normalize_phone(written), "46701234567", "input {written:?}");
    }
    assert!(!validate_phone(&normalize_phone("0047 412 34 567")), "another country is no Swish number");
}

#[test]
fn phone_validate_accepts_valid() {
    assert!(validate_phone("46701234567"));
    assert!(validate_phone("467012345678")); // 10-digit subscriber
}

#[test]
fn phone_validate_rejects_short() {
    assert!(!validate_phone("467012345")); // 8-digit subscriber
}

#[test]
fn phone_validate_rejects_wrong_prefix_length_and_non_digits() {
    assert!(!validate_phone(""));
    assert!(!validate_phone("47701234567"));      // not Sweden
    assert!(!validate_phone("4670123456789"));    // 11-digit subscriber
    assert!(!validate_phone("4670123456a"));      // letter
    assert!(!validate_phone("46701234 67"));      // space
    assert!(!validate_phone("+46701234567"));     // unnormalized
    assert!(!validate_phone("4670123456\u{0f}")); // control character
}

// The century is part of the identity. Inferring it from a 10-digit number would silently produce
// a real personnummer belonging to a different person.
#[test]
fn ssn_normalize_strips_separators_and_never_invents_a_century() {
    assert_eq!(normalize_ssn("19640823-3234"), "196408233234");
    assert_eq!(normalize_ssn("196408233234"), "196408233234");
    assert_eq!(normalize_ssn("19640823 3234"), "196408233234");

    // Ten digits stay ten digits, so the caller is told rather than guessed at.
    assert_eq!(normalize_ssn("640823-3234"), "6408233234");
    assert_eq!(normalize_ssn("250101+1234"), "2501011234");
    assert_eq!(normalize_ssn(""), "");
}

// The doubling step folds a two-digit product with `m > 9`. A nine sitting at an undoubled
// position is the only case where `>` and `>=` disagree, so it needs its own fixture: none of
// the other numbers here would notice the difference.
#[test]
fn luhn_folds_only_products_above_nine() {
    for valid in ["194008230049", "194008230189", "194008230239"] {
        assert!(
            personnummer_luhn_valid(valid),
            "{valid} carries a 9 in an undoubled position and must stay valid"
        );
    }
}

#[test]
fn luhn_requires_exactly_twelve_digits() {
    assert!(!personnummer_luhn_valid("6408233234"), "ten digits is not enough");
    assert!(!personnummer_luhn_valid("1964082332345"), "thirteen is too many");
    assert!(!personnummer_luhn_valid(""));
}

// Anything but twelve digits is refused outright, so a separator that survived normalization
// never reaches the checksum.
#[test]
fn luhn_rejects_a_separator_or_a_letter() {
    assert!(!personnummer_luhn_valid("19640823-3234"));
    assert!(!personnummer_luhn_valid("1964082332ab"));
}

// Two decimals is all Swish takes. What a person typed with two passes even after float drift;
// a third decimal, however small, does not.
#[test]
fn an_amount_is_whole_ore_or_it_is_refused() {
    for whole in [1.0, 0.01, 99.9, 1234.5, 49_999.99, 150_000.0, 1.0 + 0.1 + 0.2, 0.1 + 0.2] {
        assert!(is_whole_ore(whole), "{whole} has at most two decimals");
    }
    for fraction in [1.005, 2.675, 100.004, 100.006, 0.001, 100.0000001] {
        assert!(!is_whole_ore(fraction), "{fraction} is a fraction of an öre");
    }
    for broken in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(!is_whole_ore(broken), "{broken} is not an amount");
    }
}

// Swish refuses a message character outside its documented set with RP02, which spends the
// reference. The table is what MSS answered for each character on 2026-10-07.
#[test]
fn a_message_is_held_to_the_characters_swish_takes() {
    for good in ["Utbetalning: INV-1042 (klar), tack!", "Åsa Öberg; är det klart? Ja.", "Week 34", ""] {
        assert!(is_valid_message(good), "{good:?}");
    }
    for bad in ["é", "ü", "æ", "ø", "'", "/", "+", "%", "=", "_", "@", "*", "&", "#", "<", ">", "€", "$", "´", "~", "\t", "\n", "😀", "\""] {
        assert!(!is_valid_message(&format!("Test {bad} ok")), "{bad:?} must be refused");
    }
}

// The full 12 digits, a date that exists and a valid Luhn digit. Swish refused each of these with
// PA06 against the simulator, spending the reference: month 13, day 32 and a coordination
// number, day plus 60. February 30 it accepted, but nobody is born on it.
#[test]
fn a_personnummer_is_twelve_digits_a_real_date_and_a_luhn_digit() {
    assert!(is_valid_personnummer("196408233234"));
    assert!(is_valid_personnummer("200002293231"), "29 February in a leap year");
    for (wrong, why) in [
        ("196413233237", "month 13"),
        ("196408323233", "day 32"),
        ("196408833231", "coordination number, day + 60"),
        ("196402303231", "30 February"),
        ("190002293231", "29 February in 1900, not a leap year, though its Luhn digit is right"),
        ("196400233236", "month 00"),
        ("196408003236", "day 00"),
        ("196408233235", "wrong check digit"),
        ("6408233234", "10 digits"),
        ("1964082332340", "13 digits"),
        ("19640823-3234", "a separator"),
    ] {
        assert!(!is_valid_personnummer(wrong), "{wrong} ({why}) must be refused");
    }
}

// The name travels in bodies, paths, channel names and the certificate folder, so it is kept to
// characters that mean the same in all four, and can never name another path.
#[test]
fn a_payer_name_is_short_lowercase_and_plain() {
    use swisha::domain::validate::is_valid_payer_name;
    for right in ["sthlm", "gbg-2", "city_9", &"a".repeat(32)] {
        assert!(is_valid_payer_name(right), "{right}");
    }
    for wrong in ["", "Sthlm", "st hlm", "sthlm/2", "göteborg", "..", "../x", ".hidden", &"a".repeat(33)] {
        assert!(!is_valid_payer_name(wrong), "{wrong}");
    }
}

// payerAlias is numeric, 10 or 11 digits, per Swish's payouts reference.
#[test]
fn a_swish_number_is_ten_or_eleven_digits() {
    use swisha::domain::validate::is_valid_swish_number;
    for right in ["1231111111", "12311111111"] {
        assert!(is_valid_swish_number(right), "{right}");
    }
    for wrong in ["", "123 111 1111", "+461231111111", "123-1111111", "123111111", "123111111111", "123111111a"] {
        assert!(!is_valid_swish_number(wrong), "{wrong}");
    }
}

// A line or paragraph separator forges a line as a newline does, and a bidi control or a
// zero-width character makes what a person typed read as something else in a log or a list.
#[test]
fn text_a_person_types_holds_nothing_invisible() {
    use swisha::domain::validate::{MAX_BY, plain_text};
    for hidden in ['\u{2028}', '\u{2029}', '\u{202E}', '\u{2066}', '\u{200B}', '\u{200D}', '\u{FEFF}', '\u{AD}', '\u{E0041}'] {
        let typed = format!("Anna{hidden}Berg");
        assert_eq!(
            plain_text(&typed, "by", MAX_BY),
            Err("by must be 1 to 100 characters, with no control or invisible characters.".to_string()),
            "U+{:04X}",
            hidden as u32
        );
    }
    for typed in ["Anna Öberg", "Åsa-Lena (kassa 2)", "Bank: 100 kr, 2026-10-08!", "Zoë", "Ünal Çelik"] {
        assert_eq!(plain_text(typed, "by", MAX_BY), Ok(typed.to_string()));
    }
}
