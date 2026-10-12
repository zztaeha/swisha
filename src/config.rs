//! Every setting swisha reads, and the guards that refuse an unsafe combination.
//!
//! [`Config::from_env`] is how the service builds a [`Config`]. It refuses to return one for a
//! configuration that would leave the payout endpoint unguarded, keep private keys where other
//! users can read them, or quietly run with a setting nobody chose.

use std::net::IpAddr;
use std::path::PathBuf;
use crate::env::Env;
use crate::error::{Result, err};
use crate::domain::errors::Language;
use crate::domain::validate::{MESSAGE_CHARACTERS, is_valid_message, is_whole_ore};

/// Where the internal API listens.
///
/// A Unix socket is authenticated by the kernel from its file permissions, so it needs no
/// shared secret. A network address does, unless it is loopback.
#[derive(Clone, Debug)]
pub enum InternalListener {
    /// A network address, as `host:port`.
    Tcp(String),
    /// A Unix domain socket, created with mode `0660`.
    Unix(PathBuf),
}

impl InternalListener {
    /// A form suitable for logging, with socket paths prefixed `unix:`.
    pub fn describe(&self) -> String {
        match self {
            InternalListener::Tcp(addr) => addr.clone(),
            InternalListener::Unix(path) => format!("unix:{}", path.display()),
        }
    }
}

/// A password or key that must never reach a log: Debug output shows `<redacted>`.
///
/// ```
/// use swisha::config::Secret;
///
/// let password = Secret::from("hunter2");
/// assert_eq!(format!("{password:?}"), "<redacted>");
/// assert_eq!(password.expose(), "hunter2");
/// ```
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    /// The value itself, for the one place that has to send it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where PostgreSQL is, as [`Config::db_address`] reads `DB_HOST`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbAddress {
    /// A host name or IP address, and a port.
    Tcp(String, u16),
    /// A directory holding PostgreSQL's Unix socket.
    Socket(PathBuf),
}

/// How the connection to PostgreSQL uses TLS, as `DB_SSLMODE` names it with libpq's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbTls {
    /// `disable`: plaintext only.
    Disable,
    /// `prefer`: TLS when the server offers it, plaintext otherwise. The default on loopback or a
    /// socket, where nothing crosses a network.
    Prefer,
    /// `require`: TLS or no connection, without verifying the server's certificate. The default
    /// for a database on another host.
    Require,
    /// `verify-ca`: TLS, with the server's certificate signed by a trusted root.
    VerifyCa,
    /// `verify-full`: `verify-ca`, and the certificate names the host connected to.
    VerifyFull,
}

/// Everything swisha needs to run, resolved once at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the internal API listens.
    pub internal:             InternalListener,
    /// Where the public callback listener binds. `SWISH_CALLBACK_ADDR`.
    pub callback_addr:        String,
    /// Database host and port, or a socket directory. `DB_HOST`, read by
    /// [`db_address`](Self::db_address).
    pub db_host:              String,
    /// Database name. `DB_NAME`, required.
    pub db_name:              String,
    /// Database user. `DB_USER`, required.
    pub db_user:              String,
    /// Database password. `DB_PASS`, blank for peer authentication.
    pub db_pass:              Secret,
    /// How the database connection uses TLS. `DB_SSLMODE`; by default [`DbTls::Require`] for a
    /// database on another host and [`DbTls::Prefer`] on loopback or a socket.
    pub db_tls:               DbTls,
    /// The root certificate that verifies the database server, for a private CA. `DB_SSLROOTCERT`;
    /// without it, the roots sqlx bundles from Mozilla's store are used.
    pub db_ssl_root_cert:     Option<PathBuf>,
    /// How many connections to PostgreSQL this instance holds at most. `DB_POOL_SIZE`, by default
    /// [`DB_POOL_SIZE`], 1 to 1,000. Every instance's pool counts against `max_connections`.
    pub db_pool_size:         u32,
    /// Payouts table, always `swisha_payouts` when read from the environment. Set in code only,
    /// as the test suite does to run in parallel against one database.
    pub table_payouts:        String,
    /// Service log table, always `swisha_logs` when read from the environment.
    pub table_logs:           String,
    /// Payout event table, always `swisha_events` when read from the environment.
    pub table_events:         String,
    /// Swish number table, always `swisha_payers` when read from the environment.
    pub table_payers:         String,
    /// The proxies in front of the callback, whose `x-forwarded-for` header is believed there.
    /// Anything else setting it is ignored, which is what keeps the callback allowlist meaningful.
    /// The internal API never reads it.
    pub trusted_proxies:      Vec<IpAddr>,
    /// The `x-api-secret` value, when one is configured. Compared in constant time.
    pub api_secret:           Option<Secret>,
    /// `production` or `test`. Selects the Swish host and switches the callback allowlist on.
    pub swish_env:            String,
    /// The Swish host, resolved once from [`swish_env`](Self::swish_env).
    ///
    /// A field rather than a lookup per request, so a test can point the client at a local
    /// server without reaching for the network.
    pub swish_base_url:       String,
    /// The largest payout swisha will accept, in SEK. `SWISH_MAX_PAYOUT`, by default
    /// [`SWISH_LIMIT`], and refused at startup outside 1 to that.
    pub swish_max_payout:     f64,
    /// How many payouts and redos each number may send a minute, per caller, on this instance.
    /// `SWISH_PAYOUTS_PER_MINUTE`, by default [`PAYOUTS_PER_MINUTE`], 1 to 100,000.
    pub payouts_per_minute:   u32,
    /// Where Swish reports outcomes. `SWISH_CALLBACK_URL`, required, and refused at startup
    /// unless it is an `https://` URL Swish accepts.
    pub swish_callback_url:   String,
    /// Swish's status page summary, read for the health endpoint. A field rather than a
    /// constant so a test can point it at a local server.
    pub swish_status_url:     String,
    /// The folder holding every number's certificates. `SWISH_CERT_DIR`, required, closed to
    /// other users and writable by swisha.
    pub cert_dir:             PathBuf,
    /// Template for the recipient's message, with `{reference}` substituted.
    pub payout_message:       String,
    /// Which language error messages are rendered in. `SWISH_ERROR_LANG`.
    pub error_language:       Language,
}

/// How many days before a certificate expires swisha starts warning, as `degraded` on the health
/// endpoint.
pub const CERTIFICATE_WARNING_DAYS: i64 = 30;

/// The most Swish moves in one payout, in SEK, for business and private accounts alike.
///
/// The API's `amount` field accepts far more, but the infrastructure behind it does not, so a
/// ceiling above this could only ever produce refusals.
pub const SWISH_LIMIT: f64 = 150_000.0;

/// Connections to PostgreSQL each instance holds at most, unless `DB_POOL_SIZE` sets another. Each
/// payout runs 11 to 13 statements, each holding a connection at least one round trip: with the
/// database 0.55 ms away, 10 carried about 400 payouts a second when measured.
pub const DB_POOL_SIZE: u32 = 10;

// The pool, whole and from 1 to 1,000. A value it cannot use stops the service rather than quietly
// becoming the default.
fn parse_pool_size(env: &Env) -> Result<u32> {
    let raw = env.optional("DB_POOL_SIZE", "");
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(DB_POOL_SIZE);
    }
    match raw.parse::<u32>() {
        Ok(size) if (1..=1_000).contains(&size) => Ok(size),
        _ => Err(err(format!("DB_POOL_SIZE must be a whole number of connections from 1 to 1000, got: '{raw}'"))),
    }
}

/// Each number's payout budget a minute, per caller, unless `SWISH_PAYOUTS_PER_MINUTE` sets
/// another.
pub const PAYOUTS_PER_MINUTE: u32 = 30;

// A number's budget, whole and from 1 to 100,000. A value it cannot use stops the service rather
// than quietly becoming the default.
fn parse_payouts_per_minute(env: &Env) -> Result<u32> {
    let raw = env.optional("SWISH_PAYOUTS_PER_MINUTE", "");
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(PAYOUTS_PER_MINUTE);
    }
    match raw.parse::<u32>() {
        Ok(per_minute) if (1..=100_000).contains(&per_minute) => Ok(per_minute),
        _ => Err(err(format!(
            "SWISH_PAYOUTS_PER_MINUTE must be a whole number of payouts from 1 to 100000, got: '{raw}'"
        ))),
    }
}

// The ceiling a payout may not exceed, Swish's own limit unless set lower. A value it cannot use is
// refused rather than defaulted, and NaN would switch the ceiling off entirely.
fn parse_max_payout(env: &Env) -> Result<f64> {
    let raw = env.optional("SWISH_MAX_PAYOUT", "");
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(SWISH_LIMIT);
    }
    match raw.parse::<f64>() {
        Ok(max) if (1.0..=SWISH_LIMIT).contains(&max) && is_whole_ore(max) => Ok(max),
        _ => Err(err(format!(
            "SWISH_MAX_PAYOUT must be an amount in SEK from 1 to {SWISH_LIMIT}, Swish's own limit, \
             with at most two decimals, got: '{raw}'"
        ))),
    }
}

// Swish refuses every payout whose callback is not HTTPS with RP03, so a URL it would refuse is
// refused here instead. 684 characters is Swish's own limit.
fn validate_callback_url(url: &str) -> Result<()> {
    let https = url.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    let length = url.chars().count();
    if https && length > 8 && length <= 684 && !url.contains(char::is_whitespace) {
        return Ok(());
    }
    Err(err(format!(
        "SWISH_CALLBACK_URL must be an https:// URL of at most 684 characters with no spaces: \
         Swish refuses any other with RP03 on every payout. Got: '{url}'"
    )))
}

// A socket path wins when both are set: it is the more restrictive of the two.
fn internal_listener(env: &Env) -> InternalListener {
    let socket = env.optional("SWISH_SERVER_SOCKET", "");
    if socket.trim().is_empty() {
        InternalListener::Tcp(env.optional("SWISH_SERVER_ADDR", "127.0.0.1:8083"))
    } else {
        InternalListener::Unix(PathBuf::from(socket.trim()))
    }
}

fn is_loopback(addr: &str) -> bool {
    addr.rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(addr)
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

// A typo is refused rather than dropped: a dropped proxy is one whose forwarded callbacks are
// all refused with 403 in production, with nothing at startup to say why.
fn parse_trusted_proxies(s: &str) -> Result<Vec<IpAddr>> {
    s.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry.parse().map_err(|_| {
                err(format!("TRUSTED_PROXY must be IP addresses separated by commas, and '{entry}' is not one"))
            })
        })
        .collect()
}

// host, host:port, a bare IPv6 address, [IPv6] or [IPv6]:port, or a socket directory. A port that
// is not one is refused rather than replaced with 5432.
fn parse_db_host(value: &str) -> Option<DbAddress> {
    const DEFAULT_PORT: u16 = 5432;
    let port = |text: &str| text.parse::<u16>().ok().filter(|port| *port > 0);

    if value.starts_with('/') {
        return Some(DbAddress::Socket(PathBuf::from(value)));
    }
    if let Some(rest) = value.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        host.parse::<std::net::Ipv6Addr>().ok()?;
        let port = match after {
            "" => DEFAULT_PORT,
            _ => port(after.strip_prefix(':')?)?,
        };
        return Some(DbAddress::Tcp(host.to_string(), port));
    }
    match value.matches(':').count() {
        0 if !value.is_empty() && !value.contains(']') => {
            Some(DbAddress::Tcp(value.to_string(), DEFAULT_PORT))
        }
        1 => {
            let (host, text) = value.split_once(':')?;
            if host.is_empty() || host.contains(']') {
                return None;
            }
            Some(DbAddress::Tcp(host.to_string(), port(text)?))
        }
        _ => {
            value.parse::<std::net::Ipv6Addr>().ok()?;
            Some(DbAddress::Tcp(value.to_string(), DEFAULT_PORT))
        }
    }
}

// Across a network the connection carries personnummer, amounts and the password, so TLS is
// required there unless DB_SSLMODE says otherwise. On loopback or a socket it adds nothing.
fn parse_db_tls(env: &Env, db_host: &str) -> Result<DbTls> {
    let raw = env.optional("DB_SSLMODE", "");
    Ok(match raw.trim() {
        "" => match parse_db_host(db_host) {
            Some(DbAddress::Tcp(host, _)) if !is_local_host(&host) => DbTls::Require,
            _ => DbTls::Prefer,
        },
        "disable" => DbTls::Disable,
        "prefer" => DbTls::Prefer,
        "require" => DbTls::Require,
        "verify-ca" => DbTls::VerifyCa,
        "verify-full" => DbTls::VerifyFull,
        other => {
            return Err(err(format!(
                "DB_SSLMODE must be disable, prefer, require, verify-ca or verify-full, got: '{other}'"
            )));
        }
    })
}

fn is_local_host(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

// Settings swisha no longer reads. Refused rather than ignored, so a configuration written for an
// older swisha stops on the setting it relies on.
fn refuse_replaced(env: &Env) -> Result<()> {
    for old in ["SWISH_CERT", "SWISH_KEY", "SWISH_SIGNING_CERT", "SWISH_SIGNING_KEY"] {
        if !env.optional(old, "").trim().is_empty() {
            return Err(err(format!(
                "{old} is no longer read. Each certificate is one file holding the certificate and \
                 its key, made with: cat cert.pem cert.key > bundle.pem. Add both with their number \
                 through POST /swish/payers."
            )));
        }
    }
    if !env.optional("SWISH_REQUIRE_SSN", "").trim().is_empty() {
        return Err(err(
            "SWISH_REQUIRE_SSN is no longer read: payee_ssn is now always required, because Swish \
             requires it on every payout. Remove the setting.",
        ));
    }
    // Numbers live in the store since 0.4. The first block's keys are found by name wherever they
    // are set, and every block's in the env file.
    let blocks = env.file_keys().into_iter().find(|key| {
        PAYER_KEYS.iter().any(|field| {
            key.strip_prefix(field).is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
        })
    });
    let first = PAYER_KEYS.iter().find(|key| !env.optional(key, "").trim().is_empty());
    if let Some(key) = blocks.as_deref().or(first.copied()) {
        return Err(err(format!(
            "{key} is no longer read: Swish numbers are stored in the database. Remove the payer \
             blocks, set SWISH_CERT_DIR, and add each number with POST /swish/payers under its \
             SWISH_NAME. The README's Upgrading from 0.3 shows how."
        )));
    }
    if !env.optional("SWISH_CA", "").trim().is_empty() {
        return Err(err(
            "SWISH_CA is no longer read: Swish's servers use public certificates the system already \
             trusts. Remove the setting.",
        ));
    }
    for key in ["TABLE_PAYOUTS", "TABLE_LOGS", "TABLE_EVENTS", "NOTIFY_PREFIX"] {
        if !env.optional(key, "").trim().is_empty() {
            return Err(err(format!(
                "{key} is no longer read: one swisha pays from every number using the tables \
                 swisha_payouts, swisha_events, swisha_logs and swisha_payers and the channel \
                 prefix swisha. Two separate deployments use two databases (DB_NAME). Remove the \
                 setting."
            )));
        }
    }
    Ok(())
}

// The four settings of a payer block before 0.4, block 1's spelling. Later blocks add _2, _3.
const PAYER_KEYS: [&str; 4] = ["SWISH_NUMBER", "SWISH_NAME", "SWISH_TLS", "SWISH_SIGNING"];

// Every setting swisha reads.
const SETTINGS: [&str; 20] = [
    "DB_HOST", "DB_NAME", "DB_USER", "DB_PASS", "DB_SSLMODE", "DB_SSLROOTCERT", "DB_POOL_SIZE",
    "SWISH_SERVER_SOCKET", "SWISH_SERVER_ADDR",
    "SWISH_CALLBACK_ADDR", "TRUSTED_PROXY", "SWISH_ENV", "SWISH_MAX_PAYOUT",
    "SWISH_PAYOUTS_PER_MINUTE", "SWISH_CALLBACK_URL",
    "SWISH_CERT_DIR", "API_SHARED_SECRET", "SWISH_PAYOUT_MESSAGE", "SWISH_ERROR_LANG", "RUST_LOG",
];

// A misspelled key would be ignored and its setting would keep the default, which for
// SWISH_MAX_PAYOUT is a ceiling nobody chose.
fn refuse_unknown(env: &Env) -> Result<()> {
    for key in env.file_keys() {
        if !SETTINGS.contains(&key.as_str()) {
            let file = env.source().map(|path| path.display().to_string()).unwrap_or_default();
            return Err(err(format!(
                "{key} in {file} is not a setting swisha reads. Check its spelling against the \
                 template: a misspelled setting would otherwise keep its default."
            )));
        }
    }
    Ok(())
}

impl Config {
    /// Reads every setting, applies defaults, and refuses an unsafe combination.
    ///
    /// Warns on standard error, and goes on, when the payout endpoint listens on loopback without
    /// a secret, or on a network address, where it speaks plain HTTP to whoever is on that network.
    ///
    /// # Errors
    ///
    /// Refuses to return a config, naming the setting, when:
    ///
    /// - a required setting is missing: `DB_NAME`, `DB_USER`, `SWISH_CALLBACK_URL` or
    ///   `SWISH_CERT_DIR`;
    /// - `SWISH_CERT_DIR` is not a directory, is open to other users, or swisha cannot write to it;
    /// - the payout endpoint would listen on a non-loopback address with no `API_SHARED_SECRET`,
    ///   a secret is shorter than 16 characters, or the two listeners share an address;
    /// - the env file holds a key swisha does not read, or a removed setting is still set, a payer
    ///   block from before 0.4 among them;
    /// - `SWISH_ENV` is neither `production` nor `test`, `DB_HOST`, `DB_SSLMODE` or
    ///   `TRUSTED_PROXY` cannot be read, `DB_SSLROOTCERT` names a file that cannot be read,
    ///   `SWISH_MAX_PAYOUT` is not an amount from 1 to 150,000, `SWISH_CALLBACK_URL` is not one
    ///   Swish accepts, or `SWISH_PAYOUT_MESSAGE` holds a character Swish refuses.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use swisha::{config::Config, env::Env};
    ///
    /// let config = Config::from_env(&Env::discover()?)?;
    /// println!("certificates in {}", config.cert_dir.display());
    /// # Ok::<(), swisha::error::BoxError>(())
    /// ```
    pub fn from_env(env: &Env) -> Result<Self> {
        let swish_env = env.optional("SWISH_ENV", "test");

        // Rejected before anything else, so a typo fails on the typo.
        match swish_env.as_str() {
            "production" | "test" => {}
            other => return Err(err(format!("SWISH_ENV must be 'production' or 'test', got: '{other}'"))),
        }

        refuse_replaced(env)?;
        refuse_unknown(env)?;
        let cert_dir = PathBuf::from(env.required("SWISH_CERT_DIR")?.trim());
        crate::util::fs::check_private_dir(&cert_dir, "SWISH_CERT_DIR")?;

        // The template is the message of every payout sent without one, so a character Swish
        // refuses would spend each of those references. {reference} only ever holds safe ones.
        let payout_message = env.optional("SWISH_PAYOUT_MESSAGE", "{reference}");
        if !is_valid_message(&payout_message.replace("{reference}", "")) {
            return Err(err(format!(
                "SWISH_PAYOUT_MESSAGE may only hold {{reference}} and {MESSAGE_CHARACTERS}: Swish \
                 refuses anything else with RP02, spending the reference. Got: '{payout_message}'"
            )));
        }

        // Without it a payout cannot be reported on, so refuse to start.
        let swish_callback_url = env.optional("SWISH_CALLBACK_URL", "");
        if swish_callback_url.is_empty() {
            return Err(err("SWISH_CALLBACK_URL is required"));
        }
        validate_callback_url(&swish_callback_url)?;

        let db_host = env.optional("DB_HOST", "localhost").trim().to_string();
        let db_tls = parse_db_tls(env, &db_host)?;
        let db_ssl_root_cert = match env.optional("DB_SSLROOTCERT", "").trim() {
            "" => None,
            path => {
                std::fs::metadata(path)
                    .map_err(|e| err(format!("DB_SSLROOTCERT ({path}) cannot be read: {e}")))?;
                Some(PathBuf::from(path))
            }
        };

        let config = Self {
            internal:       internal_listener(env),
            callback_addr:  env.optional("SWISH_CALLBACK_ADDR", "127.0.0.1:8084"),
            db_host,
            db_name:        env.required("DB_NAME")?,
            db_user:        env.required("DB_USER")?,
            db_pass: {
                let p = env.optional("DB_PASS", "");
                if p.is_empty() { eprintln!("Warning: DB_PASS is not set – connecting without a password"); }
                Secret(p)
            },
            db_tls,
            db_ssl_root_cert,
            db_pool_size:   parse_pool_size(env)?,
            table_payouts:  "swisha_payouts".into(),
            table_logs:     "swisha_logs".into(),
            table_events:   "swisha_events".into(),
            table_payers:   "swisha_payers".into(),
            trusted_proxies:  parse_trusted_proxies(&env.optional("TRUSTED_PROXY", "127.0.0.1"))?,
            api_secret: {
                let secret = env.optional("API_SHARED_SECRET", "");
                (!secret.trim().is_empty()).then_some(Secret(secret))
            },
            swish_base_url: crate::swish::client::swish_base_url(&swish_env).to_string(),
            swish_env,
            swish_max_payout: parse_max_payout(env)?,
            payouts_per_minute: parse_payouts_per_minute(env)?,
            swish_callback_url,
            swish_status_url: crate::swish::status_page::SUMMARY_URL.to_string(),
            cert_dir,
            payout_message,
            error_language:   Language::parse(&env.optional("SWISH_ERROR_LANG", "en")),
        };
        // A secret that exists must be strong enough to be worth having.
        if let Some(secret) = &config.api_secret
            && secret.expose().len() < 16
        {
            return Err(err(
                "API_SHARED_SECRET must be at least 16 characters. Generate one with: openssl rand -hex 32",
            ));
        }

        // What guards the payout endpoint depends on where it listens. A Unix socket is
        // guarded by its file permissions; a loopback port by the host boundary; a network
        // port by nothing at all unless a secret is set.
        match (&config.internal, &config.api_secret) {
            (InternalListener::Unix(_), _) => {}
            (InternalListener::Tcp(addr), None) if is_loopback(addr) => {
                eprintln!(
                    "Warning: API_SHARED_SECRET is not set. {addr} is loopback, so only processes on this host can reach the payout endpoint, but any of them can, and so can whoever reaches a TLS terminator or proxy that forwards to it. Set a secret behind one."
                );
            }
            (InternalListener::Tcp(addr), None) => {
                return Err(err(format!(
                    "API_SHARED_SECRET is required when the payout endpoint listens on {addr}: a non-loopback address is reachable from the network. Set a secret, bind loopback, or use SWISH_SERVER_SOCKET."
                )));
            }
            // Plain HTTP: across hosts, whatever carries the hop decides who reads it.
            (InternalListener::Tcp(addr), Some(_)) if !is_loopback(addr) => {
                eprintln!(
                    "Warning: the payout endpoint listens on {addr} over plain HTTP. The API secret, every number's key and each payout's personnummer cross that network unencrypted: keep that hop on a private network or behind a TLS terminator."
                );
            }
            (InternalListener::Tcp(_), Some(_)) => {}
        }

        if let InternalListener::Tcp(addr) = &config.internal
            && addr == &config.callback_addr
        {
            return Err(err(
                "SWISH_SERVER_ADDR and SWISH_CALLBACK_ADDR must differ: the payout endpoint must not share a listener with the publicly reachable callback",
            ));
        }
        config.db_address()?;
        Ok(config)
    }

    /// Where PostgreSQL is, read from `DB_HOST`: `host`, `host:port`, an IPv6 address bare or in
    /// brackets with an optional port, or a directory holding the socket. The port defaults to
    /// 5432.
    ///
    /// # Errors
    ///
    /// When `DB_HOST` is none of those, a port included.
    pub fn db_address(&self) -> Result<DbAddress> {
        parse_db_host(&self.db_host).ok_or_else(|| {
            err(format!(
                "DB_HOST must be host, host:port, [IPv6]:port or a socket directory, with a port \
                 from 1 to 65535, got: '{}'",
                self.db_host
            ))
        })
    }
}
