use swisha::swish::cert::read_cert_serial;

const CERT: &str = include_str!("fixtures/cert.pem");

#[test]
fn reads_serial_as_uppercase_hex_with_der_leading_zero() {
    assert_eq!(read_cert_serial(CERT).unwrap(), "00A5FF0F2C7B01");
}

#[test]
fn rejects_input_that_is_not_a_certificate() {
    assert!(read_cert_serial("").is_err());
    assert!(read_cert_serial("-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----").is_err());
    assert!(read_cert_serial("-----BEGIN CERTIFICATE-----\nAAAA\n").is_err());
}

// Runs against Swish's real test bundle when SWISH_TEST_CERT_DIR points at it. Those files
// are 3-certificate chains, so this also proves the leaf is what gets read.
//
//   SWISH_TEST_CERT_DIR=~/Downloads/client_cert cargo test --test cert
#[test]
fn reads_the_leaf_serial_from_swish_chain_certificates() {
    let Ok(dir) = std::env::var("SWISH_TEST_CERT_DIR") else {
        eprintln!("skipped: SWISH_TEST_CERT_DIR is not set");
        return;
    };

    let cases = [
        (
            "Swish_Merchant_TestCertificate_1234679304.pem",
            "743A083EEEE494161C530EE28EF4208A",
        ),
        (
            "Swish_Merchant_TestSigningCertificate_1234679304.pem",
            "4F24C03A0295A0B53596240EA8C0F430",
        ),
    ];

    for (file, expected) in cases {
        let pem = std::fs::read_to_string(format!("{dir}/{file}")).expect("read certificate");
        assert!(
            pem.matches("BEGIN CERTIFICATE").count() > 1,
            "{file} should be a chain, so this test proves the leaf is chosen"
        );
        assert_eq!(read_cert_serial(&pem).unwrap(), expected, "{file}");
    }
}

// The mTLS and signing certificates are different, so signing with the wrong one is a real
// mistake to make. Their serials must never be confused.
#[test]
fn the_mtls_and_signing_serials_differ() {
    let Ok(dir) = std::env::var("SWISH_TEST_CERT_DIR") else {
        return;
    };
    let read = |f: &str| {
        read_cert_serial(&std::fs::read_to_string(format!("{dir}/{f}")).unwrap()).unwrap()
    };
    assert_ne!(
        read("Swish_Merchant_TestCertificate_1234679304.pem"),
        read("Swish_Merchant_TestSigningCertificate_1234679304.pem"),
    );
}

// A certificate and a key that are not a pair are refused by Swish on every payout as PA01, which
// names no key, so the pairing is checked whenever a number is added, replaced or loaded.

// A throwaway certificate and its key as one file, the shape swisha reads. `days` sets the expiry.
fn self_signed(dir: &std::path::Path, name: &str, days: &str) -> (String, String) {
    let key = dir.join(format!("{name}.key"));
    let crt = dir.join(format!("{name}.crt"));
    let out = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", days, "-subj", "/CN=test"])
        .arg("-keyout").arg(&key)
        .arg("-out").arg(&crt)
        .output()
        .expect("openssl is required to generate a throwaway pair");
    assert!(out.status.success(), "openssl req failed");
    (
        std::fs::read_to_string(&crt).expect("read certificate"),
        std::fs::read_to_string(&key).expect("read key"),
    )
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("swisha-{tag}-{}", swisha::swish::random_payout_uuid()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn a_certificate_matches_only_its_own_key() {
    use swisha::swish::cert::certificate_matches_key;

    let dir = temp_dir("pair");
    let (cert_a, key_a) = self_signed(&dir, "a", "1");
    let (cert_b, key_b) = self_signed(&dir, "b", "1");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(certificate_matches_key(&format!("{cert_a}{key_a}")).expect("check"), "a pair must match itself");
    assert!(certificate_matches_key(&format!("{key_b}{cert_b}")).expect("check"), "in either order");
    assert!(!certificate_matches_key(&format!("{cert_a}{key_b}")).expect("check"), "a foreign key must not match");
    assert!(!certificate_matches_key(&format!("{cert_b}{key_a}")).expect("check"));
}

#[test]
fn the_pair_check_rejects_a_file_missing_either_half() {
    use swisha::swish::cert::certificate_matches_key;
    let dir = temp_dir("pair2");
    let (cert, key) = self_signed(&dir, "c", "1");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(certificate_matches_key(&key).is_err(), "no certificate");
    assert!(certificate_matches_key(&cert).is_err(), "no key");
    assert!(certificate_matches_key("not a certificate").is_err());
}

// Against Swish's real test bundle: the signing pair matches, and the merchant key, which is the
// file most likely to be reached for by mistake, does not.
#[test]
fn the_swish_bundle_signing_pair_matches_and_the_merchant_key_does_not() {
    use swisha::swish::cert::certificate_matches_key;
    let Ok(dir) = std::env::var("SWISH_TEST_CERT_DIR") else {
        eprintln!("skipped: SWISH_TEST_CERT_DIR is not set");
        return;
    };
    let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).expect("read");
    let signing_cert = read("Swish_Merchant_TestSigningCertificate_1234679304.pem");
    let signing_key = read("Swish_Merchant_TestSigningCertificate_1234679304.key");
    let merchant_key = read("Swish_Merchant_TestCertificate_1234679304.key");

    assert!(certificate_matches_key(&format!("{signing_cert}\n{signing_key}")).expect("check"));
    assert!(!certificate_matches_key(&format!("{signing_cert}\n{merchant_key}")).expect("check"));
}

// Certificates write dates up to 2049 as UTCTime and from 2050 on as GeneralizedTime, and a
// certificate valid for long enough crosses that line. Both are read against openssl's answer.
#[test]
fn the_expiry_is_read_in_both_time_forms() {
    use swisha::swish::cert::read_not_after;
    let dir = temp_dir("expiry");
    for (name, days) in [("short", "1"), ("long", "10000")] {
        let (cert, _) = self_signed(&dir, name, days);
        let out = std::process::Command::new("openssl")
            .args(["x509", "-noout", "-enddate", "-dateopt", "iso_8601"])
            .arg("-in").arg(dir.join(format!("{name}.crt")))
            .output()
            .expect("openssl x509");
        let text = String::from_utf8(out.stdout).expect("utf8");
        let iso = text.trim().trim_start_matches("notAfter=").trim();
        let parse = |range: std::ops::Range<usize>| iso[range].parse::<u32>().expect(iso);
        let expected = swisha::util::time::unix_from_utc(
            parse(0..4) as i32, parse(5..7), parse(8..10), parse(11..13), parse(14..16), parse(17..19),
        );
        assert_eq!(read_not_after(&cert).expect(name), expected, "{name}: openssl says {iso}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_swish_bundle_expiry_matches_its_certificate() {
    use swisha::swish::cert::read_not_after;
    let Ok(dir) = std::env::var("SWISH_TEST_CERT_DIR") else {
        eprintln!("skipped: SWISH_TEST_CERT_DIR is not set");
        return;
    };
    let pem = std::fs::read_to_string(format!("{dir}/Swish_Merchant_TestCertificate_1234679304.pem")).expect("read");
    // notAfter=Sep 11 13:11:26 2027 GMT, per openssl x509 on the MSS 3.0 bundle.
    assert_eq!(read_not_after(&pem).expect("expiry"), swisha::util::time::unix_from_utc(2027, 9, 11, 13, 11, 26));
}

// Swish issues each certificate to one number and names it as the common name. That is what lets
// swisha refuse another number's certificate at upload rather than as PA01 on every payout.
#[test]
fn the_swish_number_a_certificate_names_is_read_from_its_subject() {
    use swisha::swish::cert::read_subject_number;
    let dir = temp_dir("subject");
    for (cn, expected) in [("1234679304", Some("1234679304")), ("12346793041", Some("12346793041")), ("test", None)] {
        let key = dir.join(format!("{cn}.key"));
        let crt = dir.join(format!("{cn}.crt"));
        let out = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1"])
            .args(["-subj", &format!("/C=SE/O=5560997982/CN={cn}")])
            .arg("-keyout").arg(&key)
            .arg("-out").arg(&crt)
            .output()
            .expect("openssl");
        assert!(out.status.success(), "openssl req failed");
        let pem = std::fs::read_to_string(&crt).expect("read");
        assert_eq!(read_subject_number(&pem).expect(cn).as_deref(), expected, "CN={cn}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(read_subject_number(CERT).expect("fixture"), None, "CN=swisha-test names no number");
    assert!(read_subject_number("no certificate").is_err());
}

#[test]
fn the_swish_bundle_names_its_number() {
    use swisha::swish::cert::read_subject_number;
    let Ok(dir) = std::env::var("SWISH_TEST_CERT_DIR") else {
        eprintln!("skipped: SWISH_TEST_CERT_DIR is not set");
        return;
    };
    for file in ["Swish_Merchant_TestCertificate_1234679304.pem", "Swish_Merchant_TestSigningCertificate_1234679304.pem"] {
        let pem = std::fs::read_to_string(format!("{dir}/{file}")).expect("read");
        assert_eq!(read_subject_number(&pem).expect(file).as_deref(), Some("1234679304"), "{file}");
    }
}
