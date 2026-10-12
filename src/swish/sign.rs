//! Double SHA-512 RSA signing, as the Swish CPC reference requires.

use crate::error::{Result, err};
use ring::{digest::{digest, SHA512}, rand::SystemRandom, signature::{RsaKeyPair, RSA_PKCS1_SHA512}};

use crate::swish::cert::pem_bodies;
use crate::util::base64;

/// Signs a payout payload the way the CPC reference requires.
///
/// Swish hashes the payload **twice**. This pre-hashes with SHA-512, then hands that digest to
/// ring's `RSA_PKCS1_SHA512`, which hashes again before signing, giving
/// `SHA512(SHA512(payload))`. Getting this wrong produces an error from Swish that says nothing
/// about signatures, which is most of why this crate exists.
///
/// Sign exactly the bytes that are sent: the serialized
/// [`PayoutPayload`](crate::swish::payload::PayoutPayload), unmodified. The result is base64.
///
/// # Errors
///
/// When the RSA operation fails, which a key [`rsa_key_pair`] accepted does not do in practice.
///
/// # Examples
///
/// ```no_run
/// use swisha::swish::sign::{rsa_key_pair, sign_payload};
///
/// let pem = std::fs::read_to_string("/var/lib/swisha/1231234567/signing-1231234567-sthlm.pem")?;
/// let key = rsa_key_pair(&pem)?;
/// let signature = sign_payload(r#"{"payoutInstructionUUID":"..."}"#, &key)?;
/// # Ok::<(), swisha::error::BoxError>(())
/// ```
pub fn sign_payload(payload: &str, key_pair: &RsaKeyPair) -> Result<String> {
    let pre_hash = digest(&SHA512, payload.as_bytes());

    let rng = SystemRandom::new();
    let mut sig = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA512, &rng, pre_hash.as_ref(), &mut sig)
        .map_err(|e| err(format!("RSA signing failed: {e}")))?;

    Ok(base64::encode(&sig))
}

/// The private key in a PEM file, decoded, from a file that may also hold certificates.
///
/// ring reads PKCS#8 only, and plenty of tooling still emits PKCS#1: the same key in a different
/// wrapper. A PKCS#1 or an encrypted key is refused with the `openssl` command that converts it,
/// rather than ring's bare `InvalidEncoding`, and a file with no key or two is refused outright.
///
/// # Errors
///
/// For a PKCS#1 or encrypted key, no key, more than one, or a key whose base64 is broken.
///
/// # Examples
///
/// ```
/// let pkcs1 = "-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----\n";
/// let error = swisha::swish::sign::private_key_der(pkcs1).unwrap_err().to_string();
/// assert!(error.contains("openssl pkcs8 -topk8"), "the refusal says how to convert it");
/// ```
pub fn private_key_der(pem: &str) -> Result<Vec<u8>> {
    if pem.contains("-----BEGIN RSA PRIVATE KEY-----") {
        return Err(err("the private key is in PKCS#1 format and swisha needs PKCS#8. Convert it with: \
                        openssl pkcs8 -topk8 -nocrypt -in swish.key -out swish.pkcs8.key"));
    }
    if pem.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        return Err(err("the private key is encrypted and swisha needs it unencrypted, readable only \
                        by swisha. Decrypt it with: openssl pkcs8 -topk8 -nocrypt -in swish.key \
                        -out swish.pkcs8.key"));
    }
    match pem_bodies(pem, "PRIVATE KEY")?.as_slice() {
        [key] => base64::decode(key).map_err(|e| err(format!("private key base64 is invalid: {e}"))),
        [] => Err(err("no private key found. The file must hold the certificate and its key \
                       together: cat cert.pem cert.key > bundle.pem")),
        _ => Err(err("more than one private key found. The file must hold one certificate and \
                      its own key")),
    }
}

/// Parses the private key in a PEM file, which may also hold its certificate.
///
/// # Errors
///
/// As [`private_key_der`] describes, or when the key is not an RSA key ring can sign with: 2048
/// to 4096 bits. Swish issues 4096.
pub fn rsa_key_pair(pem: &str) -> Result<RsaKeyPair> {
    let der = private_key_der(pem)?;
    RsaKeyPair::from_pkcs8(&der).map_err(|e| err(format!("the private key is not a usable RSA key: {e}")))
}
