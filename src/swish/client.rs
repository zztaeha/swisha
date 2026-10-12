//! The mTLS HTTP client, and which Swish host to use.

use std::sync::{Arc, OnceLock};
use std::time::Duration;
use crate::error::{BoxError, Context, Result, err};

/// Builds the mTLS client for one payer, from its TLS file: the certificate and its key.
///
/// Swish's servers, the simulator included, present certificates from a public authority, so the
/// system trust store verifies them and no Swish CA is configured. Requests time out after 55
/// seconds, and a connection that is not open, TLS included, after 5. A redirect is answered as it
/// came, never followed: following one would resend a signed payout, personnummer included, to
/// whatever host it named.
///
/// # Errors
///
/// When the file is not a certificate and its PKCS#8 key, with the reason the TLS library gave.
///
/// # Examples
///
/// ```no_run
/// let tls = std::fs::read_to_string("/var/lib/swisha/1231234567/tls-1231234567-sthlm.pem")?;
/// let client = swisha::swish::client::build_swish_client(&tls)?;
/// # Ok::<(), swisha::error::BoxError>(())
/// ```
pub fn build_swish_client(tls_pem: &str) -> Result<reqwest::Client> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

    let unreadable = |e: &dyn std::error::Error| with_cause("Failed to read the Swish mTLS identity", e);
    let chain = CertificateDer::pem_slice_iter(tls_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| unreadable(&e))?;
    let key = PrivateKeyDer::from_pem_slice(tls_pem.as_bytes()).map_err(|e| unreadable(&e))?;

    // dangerous() is only rustls's door to a verifier of one's own: this one is the platform
    // verifier reqwest itself builds, so certificates are checked exactly as reqwest checks them.
    let (provider, verifier) = trust()?;
    // As with_client_auth_cert does, the certificate is checked against the key, which then signs
    // its handshakes off the async workers.
    let identity = rustls::sign::CertifiedKey::from_der(chain, key, &provider).map_err(|e| unreadable(&e))?;
    let identity = rustls::sign::CertifiedKey::new(identity.cert, Arc::new(OffWorker(identity.key)));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| with_cause("Failed to build the Swish HTTPS client", &e))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(identity)));
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    // Each client reaches one host, so two of rustls's server slots keep its session: the default
    // preallocated 32, 14 KB a number, and one slot keeps nothing, as rustls evicts on insert.
    tls.resumption = rustls::client::Resumption::in_memory_sessions(16);

    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(Duration::from_secs(55))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| with_cause("Failed to build the Swish HTTPS client", &e))
}

// A number's TLS key. Each new connection to Swish signs with it, about 4 ms of CPU at 4096 bits,
// and a burst of them held a worker 140 ms, so the signature runs where the worker is not needed.
#[derive(Debug)]
struct OffWorker(Arc<dyn rustls::sign::SigningKey>);

impl rustls::sign::SigningKey for OffWorker {
    fn choose_scheme(&self, offered: &[rustls::SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        let signer = self.0.choose_scheme(offered)?;
        Some(Box::new(OffWorkerSigner(signer)))
    }

    fn public_key(&self) -> Option<rustls::pki_types::SubjectPublicKeyInfoDer<'_>> {
        self.0.public_key()
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        self.0.algorithm()
    }
}

#[derive(Debug)]
struct OffWorkerSigner(Box<dyn rustls::sign::Signer>);

impl rustls::sign::Signer for OffWorkerSigner {
    // block_in_place hands the worker's other tasks on while this thread signs. Only the
    // multi-thread runtime, which the http feature brings, allows it; elsewhere it signs in place.
    fn sign(&self, message: &[u8]) -> std::result::Result<Vec<u8>, rustls::Error> {
        #[cfg(feature = "http")]
        if tokio::runtime::Handle::try_current()
            .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        {
            return tokio::task::block_in_place(|| self.0.sign(message));
        }
        self.0.sign(message)
    }

    fn scheme(&self) -> rustls::SignatureScheme {
        self.0.scheme()
    }
}

type Trust = (Arc<rustls::crypto::CryptoProvider>, Arc<rustls_platform_verifier::Verifier>);

// The system trust store, as reqwest's own verifier reads it, read once for every number's client.
// Read per client on Linux it costs 3.6 ms and 111 KB a number, and fails once files run out.
fn trust() -> Result<Trust> {
    static TRUST: OnceLock<Trust> = OnceLock::new();
    if let Some(trust) = TRUST.get() {
        return Ok(trust.clone());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|e| with_cause("Failed to read the system's trusted certificates", &e))?;
    Ok(TRUST.get_or_init(|| (provider, Arc::new(verifier))).clone())
}

// reqwest renders a builder failure as the bare words "builder error". The reason a certificate
// was refused sits one level down, so without the chain an operator has nothing to act on.
fn with_cause(prefix: &str, error: &dyn std::error::Error) -> BoxError {
    let mut message = format!("{prefix}: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    err(message)
}

/// The Swish host for an environment: production, or the MSS simulator for anything else.
pub fn swish_base_url(swish_env: &str) -> &'static str {
    if swish_env == "production" {
        "https://cpc.getswish.net"
    } else {
        "https://mss.cpc.getswish.net"
    }
}

/// A JSON POST to Swish.
///
/// # Errors
///
/// When the request cannot be sent or no answer arrives. An answer of any status is `Ok`.
pub async fn swish_post(url: &str, body: &impl serde::Serialize, client: &reqwest::Client) -> Result<reqwest::Response> {
    client.post(url)
        .json(body)
        .send()
        .await
        .context("Swish POST request failed")
}

/// A GET from Swish.
///
/// # Errors
///
/// When the request cannot be sent or no answer arrives. An answer of any status is `Ok`.
pub async fn swish_get(url: &str, client: &reqwest::Client) -> Result<reqwest::Response> {
    client.get(url)
        .send()
        .await
        .context("Swish GET request failed")
}

/// Whether an answer from Swish shows that a number reaches it: any answer does but `401` and
/// `403`, with which Swish refuses the number's certificate or the number itself.
pub fn reaches(status: reqwest::StatusCode) -> bool {
    !matches!(status, reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
}

/// Asks Swish whether it answers a number's client, waiting at most 5 seconds. Any answer that
/// [`reaches`] counts, since the question is whether the connection and the certificate get
/// through.
pub async fn probe(client: &reqwest::Client, base_url: &str) -> crate::state::SwishProbe {
    let url = format!("{base_url}/swish-cpcapi/api/v1/payouts/ping");
    // A probe goes only to a number nothing proved for ten minutes, so its connection would sit
    // in the pool 90 s unused: it closes with the answer.
    let request = client.get(&url).header(reqwest::header::CONNECTION, "close");
    let answered = tokio::time::timeout(Duration::from_secs(5), request.send()).await;
    crate::state::SwishProbe {
        reachable: matches!(answered, Ok(Ok(response)) if reaches(response.status())),
        at: std::time::Instant::now(),
    }
}
