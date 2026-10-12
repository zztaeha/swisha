//! Shared application state, built at startup and handed to every request.

use std::{collections::HashMap, sync::Arc, time::{Instant, SystemTime}};
use ring::signature::RsaKeyPair;
use tokio::sync::RwLock;
use crate::config::Config;
use crate::domain::payer::PayerRecord;
use crate::domain::status::PayoutStatus;
use crate::error::{Context, Result};
use crate::events::EventStream;
use crate::payers::Payer;

// Concrete types, not trait objects: no dyn dispatch, no boxed futures, and no generics
// threaded through every handler.
/// Where payouts are stored.
pub type Store = crate::store::postgres::PostgresStore;
/// How other processes are told about changes.
pub type Notifications = crate::notify::postgres::PostgresNotifier;

/// Everything a request handler needs.
pub struct AppState {
    /// Where payouts live.
    pub store:        Store,
    /// Cross-process notification.
    pub notifier:     Notifications,
    /// In-process fan-out to SSE subscribers.
    pub stream:       EventStream,
    /// The resolved configuration.
    pub config:       Config,
    /// The Swish numbers swisha pays from, as last loaded.
    pub payers:       Payers,
    /// Held while a number is added or changed, so one process makes one change at a time.
    pub payer_changes: tokio::sync::Mutex<()>,
    /// When the process started, for the health endpoint.
    pub started_at:   SystemTime,
    /// What Swish's status page last said about payouts, `None` until read or after a failure.
    pub swish_status: RwLock<Option<SwishStatus>>,
    /// The payouts this instance is following up after sending them.
    pub followed:     Arc<Followed>,
    /// The numbers with checks due that this instance cannot ask Swish about, as the checks last
    /// found them: their certificates are not loaded here.
    pub checks_waiting: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The payouts that may be signed at once: twice the cores. A burst waits here rather than
    /// taking a blocking thread each, which starved the workers and the database alike.
    pub signing:      tokio::sync::Semaphore,
}

/// The payouts this instance is following up after sending them, and the status each last took
/// here since, as a callback or a check brought it.
///
/// What lets a follow-up stop asking Swish for what a callback already said, without reading the
/// store before every question. A callback that reaches another instance is not seen here, and the
/// follow-up asks Swish as it would have.
#[derive(Default)]
pub struct Followed(std::sync::Mutex<HashMap<(String, String), Option<PayoutStatus>>>);

impl Followed {
    /// Follows `payer`'s payout `reference` until the guard is dropped.
    pub fn start(self: &Arc<Self>, payer: &str, reference: &str) -> Following {
        let key = (payer.to_string(), reference.to_string());
        self.lock().insert(key.clone(), None);
        Following { followed: Arc::clone(self), key }
    }

    /// Notes that a followed payout took `status`. A payout no one follows is not kept.
    pub fn took(&self, payer: &str, reference: &str, status: PayoutStatus) {
        if let Some(slot) = self.lock().get_mut(&(payer.to_string(), reference.to_string())) {
            *slot = Some(status);
        }
    }

    /// The status a followed payout last took here, if any.
    pub fn status(&self, payer: &str, reference: &str) -> Option<PayoutStatus> {
        self.lock().get(&(payer.to_string(), reference.to_string())).copied().flatten()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Option<PayoutStatus>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One payout followed by [`Followed::start`], forgotten when this is dropped.
pub struct Following {
    followed: Arc<Followed>,
    key: (String, String),
}

impl Drop for Following {
    fn drop(&mut self) {
        self.followed.lock().remove(&self.key);
    }
}

impl AppState {
    /// State over a store and its notifier, with the clock started now and nothing read from
    /// Swish's status page yet.
    pub fn new(store: Store, notifier: Notifications, config: Config, payers: Payers) -> Self {
        Self {
            store,
            notifier,
            stream: EventStream::default(),
            config,
            payers,
            payer_changes: tokio::sync::Mutex::new(()),
            started_at: SystemTime::now(),
            swish_status: RwLock::new(None),
            followed: Arc::default(),
            checks_waiting: std::sync::Mutex::default(),
            signing: tokio::sync::Semaphore::new(
                2 * std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            ),
        }
    }

    /// The number stored under `name`, ready to ask Swish about its payouts, disabled or not.
    /// `None` when no number has that name, or its certificates could not be loaded.
    pub fn payer(&self, name: &str) -> Option<Arc<PayerState>> {
        self.payers.snapshot().ready(name)
    }
}

/// The Swish numbers swisha pays from, as a snapshot replaced whole whenever one changes, so a
/// request reads one consistent set and never waits for a reload.
#[derive(Default)]
pub struct Payers {
    current: std::sync::RwLock<Arc<Roster>>,
    reloading: tokio::sync::Mutex<()>,
    failed_files: std::sync::Mutex<HashMap<String, crate::payers::FilesSeen>>,
}

impl Payers {
    /// Numbers already loaded, as a test or a library caller builds them, with no row behind them.
    /// Each is enabled: a disabled number is a stored row, read with the rest by a refresh.
    pub fn new(ready: Vec<PayerState>) -> Self {
        let entries = ready
            .into_iter()
            .map(|payer| Entry {
                record: PayerRecord {
                    name: payer.name.clone(),
                    nickname: payer.nickname.clone(),
                    number: payer.number.clone(),
                    added_by: String::new(),
                    created_at: 0,
                    updated_at: 0,
                    disabled: false,
                },
                loaded: Ok(Arc::new(payer)),
            })
            .collect();
        let payers = Self::default();
        payers.replace(Roster::new(entries));
        payers
    }

    /// The numbers as last loaded.
    pub fn snapshot(&self) -> Arc<Roster> {
        Arc::clone(&self.current.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Puts a new set of numbers in place of the old, for every request from now on.
    pub fn replace(&self, roster: Roster) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(roster);
    }

    /// Held for the whole of a reload, so two reloads never race to put an older set in place.
    pub async fn reload_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reloading.lock().await
    }

    // How the files of each number that failed on what they hold looked, as the last reload saw
    // them.
    pub(crate) fn failed_files(&self) -> HashMap<String, crate::payers::FilesSeen> {
        self.failed_files.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn set_failed_files(&self, files: HashMap<String, crate::payers::FilesSeen>) {
        *self.failed_files.lock().unwrap_or_else(|e| e.into_inner()) = files;
    }
}

/// Every stored number, as one load found it.
#[derive(Default)]
pub struct Roster {
    entries: Vec<Entry>,
    // Positions in `entries` by name and by Swish number. Every request, callback and reload looks
    // a number up, so a linear search made a reload quadratic in the number count.
    by_name: HashMap<String, usize>,
    by_number: HashMap<String, usize>,
}

impl Roster {
    /// A roster of these numbers.
    pub fn new(entries: Vec<Entry>) -> Self {
        let by_name = entries.iter().enumerate().map(|(i, entry)| (entry.record.name.clone(), i)).collect();
        let by_number = entries.iter().enumerate().map(|(i, entry)| (entry.record.number.clone(), i)).collect();
        Self { entries, by_name, by_number }
    }

    /// Every number, one entry each.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The number stored under `name`.
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.by_name.get(name).map(|&i| &self.entries[i])
    }

    /// The number stored under `name`, when its certificates loaded.
    pub fn ready(&self, name: &str) -> Option<Arc<PayerState>> {
        self.find(name).and_then(|entry| entry.loaded.as_ref().ok()).cloned()
    }

    /// The name of the number that pays from `number`, loaded or not: a callback needs only that.
    pub fn name_of(&self, number: &str) -> Option<&str> {
        self.by_number.get(number).map(|&i| self.entries[i].record.name.as_str())
    }
}

/// One stored number: its row, and either its loaded client and key, or why they did not load.
#[derive(Clone)]
pub struct Entry {
    /// The number as stored.
    pub record: PayerRecord,
    /// Ready to pay, or the reason its certificates could not be used, such as a file other users
    /// can open or one that expired.
    pub loaded: std::result::Result<Arc<PayerState>, String>,
}

/// [`AppState`] as handlers receive it.
pub type SharedState = Arc<AppState>;

/// One payer, ready to pay: its own mTLS client, its own signing key, its own health probe.
///
/// Nothing here is shared between payers, so one city can never sign with another's key or ask
/// Swish about a payout with another's certificate.
pub struct PayerState {
    /// What callers send as `payer`.
    pub name:           String,
    /// What people call it.
    pub nickname:       String,
    /// The Swish number, sent as `payerAlias`.
    pub number:         String,
    /// The mTLS client built from this payer's TLS certificate.
    pub client:         reqwest::Client,
    /// The key that signs this payer's payouts, parsed once.
    pub signing_key:    Arc<RsaKeyPair>,
    /// The signing certificate's serial, sent as `signingCertificateSerialNumber`.
    pub signing_serial: String,
    /// When the TLS certificate expires, as seconds since the Unix epoch.
    pub tls_expires:    i64,
    /// When the signing certificate expires, as seconds since the Unix epoch.
    pub signing_expires: i64,
    // Only the Swish reachability check is cached. The database is pinged live on every health
    // request, because a stale answer about it is the one a health endpoint must never give.
    /// The last reachability check with this payer's certificate, `None` before the first.
    pub probe:          RwLock<Option<SwishProbe>>,
}

impl PayerState {
    /// Builds the client and parses the signing key for one number.
    ///
    /// # Errors
    ///
    /// When the TLS file is not a usable client identity, or the signing file's key is not a
    /// usable RSA key. Both are named with the payer.
    pub fn connect(payer: &Payer) -> Result<Self> {
        let client = crate::swish::client::build_swish_client(&payer.tls.pem)
            .with_context(|| format!("payer '{}', TLS certificate", payer.name))?;
        let signing_key = crate::swish::sign::rsa_key_pair(&payer.signing.pem)
            .with_context(|| format!("payer '{}', signing key", payer.name))?;
        Ok(Self {
            name: payer.name.clone(),
            nickname: payer.nickname.clone(),
            number: payer.number.clone(),
            client,
            signing_key: Arc::new(signing_key),
            signing_serial: payer.signing.serial.clone(),
            tls_expires: payer.tls.expires,
            signing_expires: payer.signing.expires,
            probe: RwLock::new(None),
        })
    }

    /// Records that Swish answered this number just now, with anything but `401` or `403`. That
    /// proves the connection and the certificate as a probe would, so the probes can pass it by.
    pub fn answered(&self) {
        self.heard(true);
    }

    /// Records that Swish refused this number just now with `401` or `403`: it does not trust the
    /// certificate, or the number is not one it pays from, so the number does not reach Swish.
    pub fn refused(&self) {
        self.heard(false);
    }

    fn heard(&self, reachable: bool) {
        if let Ok(mut probe) = self.probe.try_write() {
            *probe = Some(SwishProbe { reachable, at: Instant::now() });
        }
    }
}

/// What Swish's own status page says about payouts, and when it was read.
#[derive(Clone, Debug)]
pub struct SwishStatus {
    /// The `Payout` component's status: `operational`, `degraded_performance`, `partial_outage`,
    /// `major_outage` or `under_maintenance`. `None` when the page does not list the component.
    pub payout: Option<String>,
    /// The earliest maintenance announced for payouts that has not finished.
    pub next_maintenance: Option<Maintenance>,
    /// When the page was read, on the monotonic clock: only ever asked how long ago.
    pub at: Instant,
}

/// A maintenance window Swish announced, with the times as Swish wrote them.
#[derive(Clone, Debug)]
pub struct Maintenance {
    /// Swish's title for it.
    pub name: String,
    /// When it starts, RFC 3339.
    pub from: String,
    /// When it is due to end, RFC 3339.
    pub until: String,
}

/// What the last Swish reachability check found, and when.
///
/// Cached rather than run per request, because reaching Swish is slow and they rate-limit. Lives
/// here rather than with the health route so the library build, which has no HTTP layer, still
/// compiles.
#[derive(Clone, Copy)]
pub struct SwishProbe {
    /// Whether Swish answered with anything but `401` or `403`, with which it refuses the number.
    pub reachable: bool,
    /// When the check ran, so a reader can see how much to trust it. Monotonic, so a step in the
    /// system clock can neither stall the probes nor make health read a check as never made.
    pub at: Instant,
}
