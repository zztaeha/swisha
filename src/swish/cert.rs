//! Reading certificate files: the leaf's serial and expiry, and whether the key beside it is its own.
//!
//! swisha takes each certificate as one PEM file holding the certificate, optionally its chain,
//! and its private key, so a certificate can never be paired with another certificate's key.

use crate::error::{Result, err};

use crate::util::{base64, hex};

/// The leaf certificate's serial number, as Swish expects it in `signingCertificateSerialNumber`:
/// uppercase hex with no separators, the DER leading zero kept.
///
/// Swish's PEM files can be three-certificate chains; only the first is the merchant's own.
///
/// # Errors
///
/// When the file holds no certificate, or its first one is not valid DER.
///
/// # Examples
///
/// ```no_run
/// let pem = std::fs::read_to_string("/var/lib/swisha/1231234567/signing-1231234567-sthlm.pem")?;
/// let serial = swisha::swish::cert::read_cert_serial(&pem)?;
/// assert!(serial.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase()));
/// # Ok::<(), swisha::error::BoxError>(())
/// ```
pub fn read_cert_serial(cert_pem: &str) -> Result<String> {
    let der = first_certificate_der(cert_pem)?;
    Ok(hex::upper(serial_from_der(&der)?))
}

/// Whether the leaf certificate in a file and the private key beside it are a pair.
///
/// Checked whenever a number's certificates are added, replaced or loaded. A TLS pair that does
/// not match fails every handshake with Swish, and a signing pair that does not match produces
/// signatures Swish rejects as `PA01` on every payout, with nothing in either message pointing at
/// the cause.
///
/// The certificate embeds the key's PKCS#1 `RSAPublicKey` verbatim, so containment settles it
/// and no X.509 parsing is needed beyond finding the leaf.
///
/// # Errors
///
/// When the file holds no certificate, or no usable PKCS#8 RSA key, as
/// [`rsa_key_pair`](crate::swish::sign::rsa_key_pair) describes.
pub fn certificate_matches_key(pem: &str) -> Result<bool> {
    let certificate = first_certificate_der(pem)?;
    let pair = crate::swish::sign::rsa_key_pair(pem)?;

    let public = pair.public().as_ref();
    Ok(certificate.windows(public.len()).any(|window| window == public))
}

/// When the leaf certificate expires, as seconds since the Unix epoch.
///
/// # Errors
///
/// When the file holds no certificate, or its validity is not a UTC time RFC 5280 allows.
pub fn read_not_after(pem: &str) -> Result<i64> {
    let der = first_certificate_der(pem)?;
    not_after_from_der(&der)
}

/// The Swish number the leaf certificate names as its common name, or `None` when its common
/// name is not one.
///
/// Swish issues each certificate to one number, and both certificates in its test bundle name it,
/// as `CN=1234679304`. A certificate whose common name is not a number, such as an older one,
/// says nothing either way.
///
/// # Errors
///
/// When the file holds no certificate, or its first one is not valid DER.
pub fn read_subject_number(pem: &str) -> Result<Option<String>> {
    let der = first_certificate_der(pem)?;
    let common_name = common_name_from_der(&der)?;
    Ok(common_name.filter(|name| crate::domain::validate::is_valid_swish_number(name)))
}

// The base64 bodies of every block carrying exactly this label, in order. The label is matched
// whole, so "PRIVATE KEY" never matches "RSA PRIVATE KEY" or "ENCRYPTED PRIVATE KEY".
pub(crate) fn pem_bodies<'a>(pem: &'a str, label: &str) -> Result<Vec<&'a str>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut bodies = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let stop = after
            .find(&end)
            .ok_or_else(|| err(format!("unterminated {label} block in PEM")))?;
        bodies.push(&after[..stop]);
        rest = &after[stop + end.len()..];
    }
    Ok(bodies)
}

// Takes only the first block, so a PEM holding a full chain still yields the leaf.
fn first_certificate_der(pem: &str) -> Result<Vec<u8>> {
    let body = *pem_bodies(pem, "CERTIFICATE")?
        .first()
        .ok_or_else(|| err("no CERTIFICATE block found in PEM"))?;
    base64::decode(body).map_err(|e| err(format!("certificate base64 is invalid: {e}")))
}

// Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL,
// serialNumber INTEGER, ... }, ... }. The INTEGER contents are returned verbatim so the
// DER leading-zero pad survives, since that pad is part of the serial Swish expects.
fn serial_from_der(der: &[u8]) -> Result<&[u8]> {
    let (tag, certificate) = read_tlv(der, &mut 0)?;
    if tag != 0x30 {
        return Err(err("certificate is not a DER SEQUENCE"));
    }

    let (tag, tbs) = read_tlv(certificate, &mut 0)?;
    if tag != 0x30 {
        return Err(err("tbsCertificate is not a DER SEQUENCE"));
    }

    let mut pos = 0;
    if tbs.first() == Some(&0xa0) {
        read_tlv(tbs, &mut pos)?;
    }

    let (tag, serial) = read_tlv(tbs, &mut pos)?;
    if tag != 0x02 {
        return Err(err("serialNumber is not a DER INTEGER"));
    }
    Ok(serial)
}

// tbsCertificate continues: signature AlgorithmIdentifier, issuer Name, then
// validity SEQUENCE { notBefore Time, notAfter Time }, each Time a UTCTime or GeneralizedTime.
fn not_after_from_der(der: &[u8]) -> Result<i64> {
    let (_, certificate) = read_tlv(der, &mut 0)?;
    let (_, tbs) = read_tlv(certificate, &mut 0)?;

    let mut pos = 0;
    if tbs.first() == Some(&0xa0) {
        read_tlv(tbs, &mut pos)?;
    }
    for _ in ["serialNumber", "signature", "issuer"] {
        read_tlv(tbs, &mut pos)?;
    }
    let (tag, validity) = read_tlv(tbs, &mut pos)?;
    if tag != 0x30 {
        return Err(err("validity is not a DER SEQUENCE"));
    }

    let mut pos = 0;
    read_tlv(validity, &mut pos)?;
    let (tag, not_after) = read_tlv(validity, &mut pos)?;
    parse_time(tag, not_after)
}

// tbsCertificate continues past validity to the subject: a SEQUENCE of SETs, each holding
// SEQUENCE { type OID, value }. commonName is OID 2.5.4.3.
fn common_name_from_der(der: &[u8]) -> Result<Option<String>> {
    const COMMON_NAME: [u8; 3] = [0x55, 0x04, 0x03];
    let (_, certificate) = read_tlv(der, &mut 0)?;
    let (_, tbs) = read_tlv(certificate, &mut 0)?;

    let mut pos = 0;
    if tbs.first() == Some(&0xa0) {
        read_tlv(tbs, &mut pos)?;
    }
    for _ in ["serialNumber", "signature", "issuer", "validity"] {
        read_tlv(tbs, &mut pos)?;
    }
    let (tag, subject) = read_tlv(tbs, &mut pos)?;
    if tag != 0x30 {
        return Err(err("subject is not a DER SEQUENCE"));
    }

    let mut next_set = 0;
    while next_set < subject.len() {
        let (_, set) = read_tlv(subject, &mut next_set)?;
        let mut next_attribute = 0;
        while next_attribute < set.len() {
            let (_, attribute) = read_tlv(set, &mut next_attribute)?;
            let mut field = 0;
            let (oid_tag, oid) = read_tlv(attribute, &mut field)?;
            let (_, value) = read_tlv(attribute, &mut field)?;
            if oid_tag == 0x06 && oid == COMMON_NAME {
                return Ok(std::str::from_utf8(value).ok().map(str::to_string));
            }
        }
    }
    Ok(None)
}

// RFC 5280 fixes both forms to whole seconds in UTC: YYMMDDHHMMSSZ, where YY below 50 is 20YY,
// and YYYYMMDDHHMMSSZ for dates from 2050 on.
fn parse_time(tag: u8, value: &[u8]) -> Result<i64> {
    if !value.is_ascii() {
        return Err(err("certificate time is not ASCII"));
    }
    let text = std::str::from_utf8(value).map_err(|_| err("certificate time is not ASCII"))?;
    let (year, rest) = match (tag, text.len()) {
        (0x17, 13) => {
            let yy: i32 = number(&text[..2])?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &text[2..])
        }
        (0x18, 15) => (number(&text[..4])?, &text[4..]),
        _ => return Err(err(format!("unsupported certificate time: {text}"))),
    };
    if !rest.ends_with('Z') {
        return Err(err(format!("certificate time is not UTC: {text}")));
    }
    let field = |i: usize| number::<u32>(&rest[i..i + 2]);
    Ok(crate::util::time::unix_from_utc(year, field(0)?, field(2)?, field(4)?, field(6)?, field(8)?))
}

fn number<T: std::str::FromStr>(digits: &str) -> Result<T> {
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err(format!("certificate time has a non-digit: {digits}")));
    }
    digits.parse().map_err(|_| err(format!("certificate time is out of range: {digits}")))
}

fn read_tlv<'a>(buf: &'a [u8], pos: &mut usize) -> Result<(u8, &'a [u8])> {
    let tag = *buf.get(*pos).ok_or_else(|| err("truncated DER: missing tag"))?;
    *pos += 1;

    let len = read_length(buf, pos)?;
    let start = *pos;
    let end = start
        .checked_add(len)
        .ok_or_else(|| err("DER length overflows"))?;
    if end > buf.len() {
        return Err(err("DER length runs past the end of the buffer"));
    }

    *pos = end;
    Ok((tag, &buf[start..end]))
}

fn read_length(buf: &[u8], pos: &mut usize) -> Result<usize> {
    let first = *buf.get(*pos).ok_or_else(|| err("truncated DER: missing length"))?;
    *pos += 1;
    if first < 0x80 {
        return Ok(first as usize);
    }

    let count = (first & 0x7f) as usize;
    if count == 0 || count > 4 {
        return Err(err("unsupported DER length encoding"));
    }

    let mut len = 0usize;
    for _ in 0..count {
        let b = *buf.get(*pos).ok_or_else(|| err("truncated DER: short length"))?;
        *pos += 1;
        len = (len << 8) | b as usize;
    }
    Ok(len)
}
