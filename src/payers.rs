//! The Swish numbers swisha pays from: their certificates, the folder that holds them, and keeping
//! a running swisha's copy of them current.
//!
//! Each number is a row in the store and a folder of its own in the certificate folder, named for
//! its Swish number and holding two files, `tls-{number}-{name}.pem` and
//! `signing-{number}-{name}.pem`. Each is a certificate followed by its own private key, readable
//! by swisha alone. swisha writes the files itself and picks every path; a caller only ever sends
//! their contents. The certificate folder itself is made by hand, never by swisha.
//!
//! A number whose files fail their checks is kept, marked with the reason, rather than stopping
//! the service: one customer's broken certificate never stops another customer's payouts.
//!
//! Each number also has its own key, made when it is added and shown once. Every request about a
//! number carries it, so the API secret alone reaches no number that is already stored. Only the
//! key's hash is kept.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::domain::payer::{AddPayerOutcome, LogLine, NewPayer, PayerRecord};
use crate::domain::request::{CertificatesRequest, NewPayerRequest, NicknameRequest, PersonRequest};
use crate::domain::validate::{
    MAX_BY, MAX_NICKNAME, MAX_NOTE, is_valid_payer_name, is_valid_swish_number, plain_text,
};
use crate::error::{Context, Result, err};
use crate::events::publish_log;
use crate::state::{Entry, PayerState, Roster, SharedState};
use crate::store::PayoutStore;
use crate::swish::cert::{certificate_matches_key, read_cert_serial, read_not_after, read_subject_number};
use crate::util::fs::{create_private_dir, read_private, sync_dir, write_private};

/// How often a running swisha reads the stored numbers, so a change another instance made reaches
/// it within this long.
pub const REFRESH: Duration = Duration::from_secs(10);

/// When a running swisha checks that its numbers reach Swish, for health.
#[derive(Clone, Copy, Debug)]
pub struct ProbeSchedule {
    /// How often it asks Swish about one of its numbers: never more often, whatever the count.
    pub pace: Duration,
    /// How long an answer from Swish, to a probe or to a real request, proves a number reaches it.
    pub fresh: Duration,
    /// How soon a number Swish did not answer is asked again.
    pub retry: Duration,
}

/// The probes the service runs: one number a second, each at most every 10 minutes, and one that
/// got no answer again after 30 seconds. With 10,000 numbers and no payouts, each is asked at
/// least every 2.8 hours; a number whose payouts keep proving it is never asked.
pub const PROBES: ProbeSchedule = ProbeSchedule {
    pace: Duration::from_secs(1),
    fresh: Duration::from_secs(10 * 60),
    retry: Duration::from_secs(30),
};

const KEY_PREFIX: &str = "swk_";

// A certificate write stages its files in a temporary folder, and a replacement moves the old
// ones into one. A write cut short leaves it behind; once it is an hour old it is removed.
const INCOMING: &str = ".incoming-";
const REPLACED: &str = ".replaced-";
const LEFTOVER_AGE: i64 = 60 * 60;
const TIDY: Duration = Duration::from_secs(10 * 60);

/// A number's key as it is made: the key, shown once, and the hash the store keeps in its place.
/// Debug output leaves the key out.
pub struct NewKey {
    /// `swk_` and 64 hex characters, 32 random bytes.
    pub key: String,
    /// What [`key_hash`] gives for the key.
    pub hash: String,
}

impl std::fmt::Debug for NewKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewKey").field("hash", &self.hash).finish_non_exhaustive()
    }
}

/// Makes a key for a number.
///
/// # Panics
///
/// When the operating system's random number generator fails, which leaves no safe way to make a
/// key.
///
/// # Examples
///
/// ```
/// let made = swisha::payers::new_key();
/// assert!(made.key.starts_with("swk_") && made.key.len() == 68);
/// assert_eq!(swisha::payers::key_hash(&made.key), Some(made.hash));
/// ```
pub fn new_key() -> NewKey {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).expect("SystemRandom::fill failed");
    let key = format!("{KEY_PREFIX}{}", crate::util::hex::upper(&bytes));
    let hash = hash(&key);
    NewKey { key, hash }
}

/// The hash the store keeps for `key`, or `None` when `key` is not shaped as swisha makes one, so
/// a malformed key is refused without a query.
///
/// ```
/// use swisha::payers::key_hash;
///
/// assert_eq!(key_hash("swk_123"), None);
/// assert_eq!(key_hash(&format!("swk_{}", "A".repeat(64))).map(|hash| hash.len()), Some(64));
/// ```
pub fn key_hash(key: &str) -> Option<String> {
    let hex = key.strip_prefix(KEY_PREFIX)?;
    let shaped = hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b));
    shaped.then(|| hash(key))
}

fn hash(key: &str) -> String {
    crate::util::hex::upper(ring::digest::digest(&ring::digest::SHA256, key.as_bytes()).as_ref())
}

/// One certificate as loaded: the file's contents and what swisha read from it.
///
/// The file holds the certificate, optionally its chain, and its own private key, so the two can
/// never be mismatched across files. Debug output leaves the key out.
#[derive(Clone)]
pub struct CertificateFile {
    /// The file's contents as PEM: the certificate first, then any chain, and the private key.
    pub pem: String,
    /// The leaf certificate's serial, in the form Swish expects.
    pub serial: String,
    /// When the leaf certificate expires, as seconds since the Unix epoch.
    pub expires: i64,
}

impl std::fmt::Debug for CertificateFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateFile")
            .field("serial", &self.serial)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

/// One Swish number with both of its certificates read and checked.
#[derive(Clone, Debug)]
pub struct Payer {
    /// What callers send as `payer`.
    pub name: String,
    /// What people call it.
    pub nickname: String,
    /// The Swish number, sent to Swish as `payerAlias`.
    pub number: String,
    /// The TLS certificate and its key, which authenticate every connection to Swish.
    pub tls: CertificateFile,
    /// The signing certificate and its key. Its serial tells Swish which key signed.
    pub signing: CertificateFile,
}

/// Checks one certificate file's contents as swisha will use them for the Swish number `number`,
/// and reads its serial and expiry.
///
/// The file must hold a certificate and exactly one PKCS#8 key that belongs to it, and must not
/// have expired. A certificate naming another Swish number as its common name is refused: Swish
/// would refuse every payout signed or sent with it as `PA01`. One naming none is accepted, since
/// older certificates may not.
///
/// # Errors
///
/// Saying what is wrong and how to fix it.
pub fn inspect(pem: &str, number: &str) -> Result<CertificateFile> {
    if !certificate_matches_key(pem)? {
        return Err(err(
            "the certificate and the private key in it are not a pair. Rebuild the file from one \
             certificate and its own key.",
        ));
    }
    if let Some(named) = read_subject_number(pem)?
        && named != number
    {
        return Err(err(format!(
            "the certificate belongs to Swish number {named}, not {number}. Swish would refuse \
             every payout made with it as PA01."
        )));
    }
    let serial = read_cert_serial(pem)?;
    let expires = read_not_after(pem)?;
    if expires <= crate::util::time::unix_now() {
        let expiry = crate::util::time::to_stockholm(expires).date();
        return Err(err(format!(
            "the certificate expired on {expiry}. Order a new one in the Swish Certificate System."
        )));
    }
    Ok(CertificateFile { pem: pem.to_string(), serial, expires })
}

// Why a number did not load: what its files hold, which only a change to them can put right, or
// anything else, such as a file that could not be read, which may pass by itself.
enum Unloaded {
    Content(String),
    Other(String),
}

// A number's folder, named for its Swish number, and its files, named for the number and the name.
// Only a number and a name shaped as one make a path, so a hand-edited row stays inside the folder.
struct Place {
    folder: PathBuf,
    tls: String,
    signing: String,
}

impl Place {
    fn of(cert_dir: &Path, number: &str, name: &str) -> Result<Self> {
        if !is_valid_swish_number(number) || !is_valid_payer_name(name) {
            return Err(err(
                "Its stored Swish number or name is not shaped as one, so no file is read for it.",
            ));
        }
        Ok(Self {
            folder: cert_dir.join(number),
            tls: format!("tls-{number}-{name}.pem"),
            signing: format!("signing-{number}-{name}.pem"),
        })
    }
}

fn try_load(record: &PayerRecord, folder: &Path) -> std::result::Result<PayerState, Unloaded> {
    let place = Place::of(folder, &record.number, &record.name).map_err(|e| Unloaded::Other(e.to_string()))?;
    let read = |file: &str| {
        let path = place.folder.join(file);
        read_private(&path, "certificate file").map_err(|e| Unloaded::Other(e.to_string()))
    };
    let (tls, signing) = (read(&place.tls)?, read(&place.signing)?);
    let content = |e: crate::error::BoxError| Unloaded::Content(e.to_string());
    let tls = inspect(&tls, &record.number).context("TLS certificate").map_err(content)?;
    let signing = inspect(&signing, &record.number).context("signing certificate").map_err(content)?;
    PayerState::connect(&Payer {
        name: record.name.clone(),
        nickname: record.nickname.clone(),
        number: record.number.clone(),
        tls,
        signing,
    })
    .map_err(|e| Unloaded::Other(e.to_string()))
}

/// How a number's two certificate files looked when what they hold failed. A write, a rename or a
/// chmod changes it, so an unchanged one holds the same bytes, which would fail the same way.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FilesSeen([Option<Stamp>; 2]);

// One file's device, inode and length, and its modification and change times to the nanosecond.
type Stamp = (u64, u64, u64, i64, i64, i64, i64);

impl FilesSeen {
    fn of(folder: &Path, record: &PayerRecord) -> Self {
        use std::os::unix::fs::MetadataExt;
        let Ok(place) = Place::of(folder, &record.number, &record.name) else {
            return Self([None, None]);
        };
        let look = |file: &str| {
            let m = std::fs::metadata(place.folder.join(file)).ok()?;
            Some((m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()))
        };
        Self([look(&place.tls), look(&place.signing)])
    }
}

/// Reads every stored number and puts the set in place.
///
/// A number whose row has not changed since it last loaded keeps its client, key and last probe.
/// A changed or new one is loaded from its files. One that failed is tried again once its files
/// change, so a file put right is picked up without a restart; one whose files could not be read
/// is tried again every time. Loading runs on the blocking pool, so the runtime never waits on it.
///
/// # Errors
///
/// When the store cannot be read. A number whose files fail is kept, with the reason, and is not
/// an error.
pub async fn refresh(state: &SharedState) -> Result<()> {
    refresh_unless(state, |_| false).await
}

// Reads every number again, unless `holds` already holds of the numbers as they stand once the
// reload lock is taken: requests that missed together wait for one read, not one each.
async fn refresh_unless(state: &SharedState, holds: impl Fn(&Roster) -> bool) -> Result<()> {
    let _reloading = state.payers.reload_lock().await;
    if holds(&state.payers.snapshot()) {
        return Ok(());
    }
    let records = state.store.payers().await?;
    let previous = state.payers.snapshot();
    if unchanged(&records, &previous) {
        return Ok(());
    }
    let seen = state.payers.failed_files();
    let folder = state.config.cert_dir.clone();
    let (entries, failed) = tokio::task::spawn_blocking(move || reloaded(records, &previous, &seen, &folder))
        .await
        .map_err(|e| err(format!("Reloading the numbers stopped: {e}")))?;
    state.payers.replace(Roster::new(entries));
    state.payers.set_failed_files(failed);
    Ok(())
}

// Every stored number held as stored, and loaded: a rebuild would make the same roster.
fn unchanged(records: &[PayerRecord], roster: &Roster) -> bool {
    records.len() == roster.entries().len()
        && records.iter().all(|record| {
            roster.find(&record.name).is_some_and(|entry| entry.loaded.is_ok() && entry.record == *record)
        })
}

// Each stored number as it now stands, and how the files of each that failed on what they hold
// looked. 1,000 such numbers took 107 ms of the runtime every refresh when they were read again.
fn reloaded(
    records: Vec<PayerRecord>,
    previous: &Roster,
    seen: &HashMap<String, FilesSeen>,
    folder: &Path,
) -> (Vec<Entry>, HashMap<String, FilesSeen>) {
    let mut failed = HashMap::new();
    let kept: Vec<Option<Entry>> = records
        .iter()
        .map(|record| {
            let entry = previous.find(&record.name).filter(|entry| entry.record == *record)?;
            if entry.loaded.is_ok() {
                return Some(entry.clone());
            }
            let files = seen.get(&record.name).filter(|files| **files == FilesSeen::of(folder, record))?;
            failed.insert(record.name.clone(), files.clone());
            Some(entry.clone())
        })
        .collect();
    let to_load: Vec<&PayerRecord> =
        records.iter().zip(&kept).filter(|(_, kept)| kept.is_none()).map(|(record, _)| record).collect();
    let mut loads = load_all(&to_load, folder).into_iter();

    let entries = records
        .into_iter()
        .zip(kept)
        .map(|(record, kept)| {
            if let Some(entry) = kept {
                return entry;
            }
            let earlier = previous.find(&record.name).filter(|entry| entry.record == record);
            let (files, loaded) = loads.next().expect("one load for each number not kept");
            let loaded = loaded.map_err(|unloaded| match unloaded {
                Unloaded::Content(reason) => {
                    failed.insert(record.name.clone(), files);
                    reason
                }
                Unloaded::Other(reason) => reason,
            });
            if let Err(problem) = &loaded
                && earlier.and_then(|entry| entry.loaded.as_ref().err()) != Some(problem)
            {
                tracing::warn!("[payers] {} cannot pay: {problem}", record.name);
            }
            Entry { record, loaded }
        })
        .collect();
    (entries, failed)
}

type Load = (FilesSeen, std::result::Result<Arc<PayerState>, Unloaded>);

// Loads each number, in order, on every core when there are many: 10,000 at startup took 4 s on one
// thread. Each one's files are looked at before the read, so a change made meanwhile is seen next.
fn load_all(records: &[&PayerRecord], folder: &Path) -> Vec<Load> {
    let load = |record: &&PayerRecord| (FilesSeen::of(folder, record), try_load(record, folder).map(Arc::new));
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    if records.len() < 64 || threads == 1 {
        return records.iter().map(load).collect();
    }
    std::thread::scope(|scope| {
        let parts: Vec<_> = records
            .chunks(records.len().div_ceil(threads))
            .map(|part| scope.spawn(move || part.iter().map(load).collect::<Vec<_>>()))
            .collect();
        parts.into_iter().flat_map(|part| part.join().expect("a loading thread stopped")).collect()
    })
}

/// The number stored under `name` as this instance holds it. One stored since the last refresh,
/// as by another instance moments ago, is loaded first, so it can be used at once. So is one that
/// failed to load within [`REFRESH`] of its last change, as while its files were being put in
/// place; an older failure stands until the next refresh.
pub async fn find(state: &SharedState, name: &str) -> Option<Entry> {
    let held = |roster: &Roster| {
        roster.find(name).is_some_and(|entry| !(entry.loaded.is_err() && just_changed(&entry.record)))
    };
    let roster = state.payers.snapshot();
    if held(&roster) {
        return roster.find(name).cloned();
    }
    if let Err(e) = refresh_unless(state, held).await {
        tracing::warn!("[payers] store error loading {name}: {e}");
    }
    state.payers.snapshot().find(name).cloned()
}

// Whether a number's row changed within the last refresh. Its stamp is in microseconds.
fn just_changed(record: &PayerRecord) -> bool {
    record.updated_at / 1_000_000 >= crate::util::time::unix_now() - REFRESH.as_secs() as i64
}

/// The name of the number that pays from `number`, loaded or not, as this instance holds it. One
/// stored since the last refresh is read first, as [`find`] does. Only a value shaped as a Swish
/// number is looked for, so anything else costs no read.
///
/// # Errors
///
/// When the store cannot be read for a number this instance does not hold: whether swisha pays
/// from it is then unknown, not no.
pub async fn name_of(state: &SharedState, number: &str) -> Result<Option<String>> {
    if let Some(name) = state.payers.snapshot().name_of(number) {
        return Ok(Some(name.to_string()));
    }
    if !is_valid_swish_number(number) {
        return Ok(None);
    }
    refresh_unless(state, |roster| roster.name_of(number).is_some()).await?;
    Ok(state.payers.snapshot().name_of(number).map(str::to_string))
}

/// Keeps the stored numbers current until the process stops: reads them every [`REFRESH`], checks
/// that they reach Swish on [`PROBES`], and tidies what certificate writes cut short by a crash left
/// in the certificate folder. A number's folder a replacement moved aside goes back at once, and
/// anything else goes once it is an hour old.
pub async fn watch(state: SharedState) {
    tokio::spawn(probe(state.clone(), PROBES));
    tokio::spawn(tidy(state.clone()));
    let mut tick = tokio::time::interval(REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = refresh(&state).await {
            tracing::warn!("[payers] store error reading the numbers: {e}");
        }
    }
}

/// Checks that the numbers reach Swish on `schedule` until the process stops, for health: at each
/// `pace`, the number longest overdue, each with its own certificate.
///
/// One question at a time, so Swish hears at most one probe per `pace` from this copy whatever the
/// count, and an idle number keeps no connection open between them. A number Swish answered about
/// a payout within `fresh` is not asked: that answer already proved it.
pub async fn probe(state: SharedState, schedule: ProbeSchedule) {
    let mut tick = tokio::time::interval(schedule.pace);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Some(payer) = most_overdue(&state.payers.snapshot(), &schedule) {
            let probe = crate::swish::client::probe(&payer.client, &state.config.swish_base_url).await;
            *payer.probe.write().await = Some(probe);
        }
    }
}

// The next number to ask about, if any is due: one that got no answer, after `retry`, as it holds
// health degraded; then one never asked; then one answered over `fresh` ago. Longest overdue first.
fn most_overdue(roster: &Roster, schedule: &ProbeSchedule) -> Option<Arc<PayerState>> {
    let now = Instant::now();
    roster
        .entries()
        .iter()
        .filter(|entry| !entry.record.disabled)
        .filter_map(|entry| entry.loaded.as_ref().ok())
        .filter_map(|payer| {
            let (rank, due) = match *payer.probe.try_read().ok()? {
                Some(last) if !last.reachable => (0, last.at + schedule.retry),
                None => (1, now),
                Some(last) => (2, last.at + schedule.fresh),
            };
            (due <= now).then_some(((rank, due), payer))
        })
        .min_by_key(|(order, _)| *order)
        .map(|(_, payer)| Arc::clone(payer))
}

/// Why a change to a stored number was refused.
#[derive(Debug)]
pub enum PayerError {
    /// A field or a certificate is wrong. Nothing was stored.
    Invalid(String),
    /// No number is stored under that name.
    NotFound,
    /// The name or the number is taken, or the number paid out under another name.
    Conflict(String),
    /// The store or the certificate folder failed.
    Failed(String),
}

impl std::fmt::Display for PayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayerError::Invalid(message) | PayerError::Conflict(message) | PayerError::Failed(message) => {
                f.write_str(message)
            }
            PayerError::NotFound => f.write_str("Not found."),
        }
    }
}

impl std::error::Error for PayerError {}

/// A number [`add`] stored.
pub struct Added {
    /// The number's key. It exists only here: the store keeps its hash.
    pub key: String,
    /// Why its certificate files are not in place, when they could not be put there. The number is
    /// stored all the same, and its files can be uploaded again with its key.
    pub unplaced: Option<String>,
}

/// Adds a number: checks both certificates as swisha will use them, writes them to the
/// certificate folder, stores the row and loads it, and returns the number's key. `ip` is who
/// asked, for the log.
///
/// Once the row is stored the key is always returned, since nothing else can ever show it: files
/// that could not be put in place leave the number stored but unusable, saying why.
///
/// # Errors
///
/// [`PayerError::Invalid`] for a field or a certificate, [`PayerError::Conflict`] when the name
/// or the number is taken or the number paid out under another name, [`PayerError::Failed`] when
/// the store or the folder fails before the number is stored. A conflict never names another
/// number: the server's log does.
pub async fn add(
    state: &SharedState,
    request: &NewPayerRequest,
    ip: &str,
) -> std::result::Result<Added, PayerError> {
    let name = request.name.trim();
    let number = request.number.trim();
    if !is_valid_payer_name(name) {
        return Err(PayerError::Invalid("name must be 1 to 32 characters of a-z, 0-9, - and _.".into()));
    }
    if !is_valid_swish_number(number) {
        return Err(PayerError::Invalid("number must be the Swish number, 10 or 11 digits.".into()));
    }
    let nickname = plain_text(&request.nickname, "nickname", MAX_NICKNAME).map_err(PayerError::Invalid)?;
    let by = plain_text(&request.by, "by", MAX_BY).map_err(PayerError::Invalid)?;
    usable(name, &nickname, number, &request.tls, &request.signing)?;

    let _change = state.payer_changes.lock().await;
    let folder = &state.config.cert_dir;
    let place = Place::of(folder, number, name).map_err(|e| PayerError::Failed(e.to_string()))?;
    let staged = stage(folder, &place, name, &request.tls, &request.signing).await?;
    let key = new_key();
    let (message, context) = (
        format!("Payer {name} added by {by}"),
        json!({ "payer": name, "nickname": &nickname, "number": number, "by": &by }).to_string(),
    );
    let logged = line(&message, &context, ip);
    let outcome = state
        .store
        .add_payer(&NewPayer { name, nickname: &nickname, number, added_by: &by, key_hash: &key.hash }, &logged)
        .await;
    let refused = match outcome {
        Ok(AddPayerOutcome::Added) => None,
        Ok(AddPayerOutcome::NameTaken) => Some(PayerError::Conflict(format!(
            "A number is already stored under the name {name}."
        ))),
        Ok(AddPayerOutcome::NumberTaken(holder)) => {
            tracing::warn!("[payers] {name} refused: Swish number {number} is stored as {holder}");
            Some(PayerError::Conflict(format!(
                "Swish number {number} is already stored, under another name. The server's log \
                 names it."
            )))
        }
        Ok(AddPayerOutcome::NumberPaidAs(holder)) => {
            tracing::warn!("[payers] {name} refused: Swish number {number} has paid out as {holder}");
            Some(PayerError::Conflict(format!(
                "Swish number {number} has paid out under another name, and can only be added under \
                 that one: the name is part of the double-payout guard, so under a new name every \
                 reference it already paid would pay again. The server's log names it."
            )))
        }
        // An answer lost after the commit leaves the number stored under this request's own key,
        // which tells it apart from one never stored.
        Err(e) => match state.store.payer_by_key(&key.hash).await {
            Ok(Some(holder)) if holder.name == name => None,
            Ok(_) => Some(failed(name, "Could not store the number", e)),
            Err(_) => {
                tracing::warn!(
                    "[payers] {name}: Could not store the number, nor read back whether it was: {e}"
                );
                Some(PayerError::Failed(format!(
                    "Could not store the number, and could not read back whether it was stored. If \
                     adding it again answers 409, it was: give it a new key with swisha --new-key \
                     {name} on the server, then upload its certificates with PUT \
                     /swish/payers/{name}/certificates."
                )))
            }
        },
    };
    if let Some(refused) = refused {
        discard(staged).await;
        return Err(refused);
    }
    let unplaced = match install(folder, staged, &place).await {
        Ok(aside) => {
            drop_aside(aside).await;
            None
        }
        Err(e) => {
            tracing::warn!("[payers] {name} was stored, but its certificate files could not be put in place: {e}");
            Some(format!(
                "The certificate files could not be put in place: {e}. Upload them again with PUT \
                 /swish/payers/{name}/certificates."
            ))
        }
    };

    reload(state).await;
    publish_log(state, &logged).await;
    Ok(Added { key: key.key, unplaced })
}

/// Replaces a number's two certificates, as when they are renewed. The same checks as
/// [`add`] apply, against the number already stored.
///
/// # Errors
///
/// As [`add`], and [`PayerError::NotFound`] when no number has that name.
pub async fn replace_certificates(
    state: &SharedState,
    name: &str,
    request: &CertificatesRequest,
    ip: &str,
) -> std::result::Result<(), PayerError> {
    let by = plain_text(&request.by, "by", MAX_BY).map_err(PayerError::Invalid)?;
    // The number never changes, so the roster holds it; only one added elsewhere since the last
    // refresh is read from the store, which the reload below reads whole anyway.
    let record = match state.payers.snapshot().find(name) {
        Some(entry) => entry.record.clone(),
        None => stored(state, name).await?,
    };
    usable(name, &record.nickname, &record.number, &request.tls, &request.signing)?;

    let _change = state.payer_changes.lock().await;
    let folder = &state.config.cert_dir;
    let place = Place::of(folder, &record.number, name).map_err(|e| PayerError::Failed(e.to_string()))?;
    let staged = stage(folder, &place, name, &request.tls, &request.signing).await?;
    let aside = install(folder, staged, &place)
        .await
        .map_err(|e| failed(name, "The certificate files could not be put in place", e))?;
    // The row moves after the files, so another instance reloading on it reads the new ones. A
    // change that could not be recorded is undone, so no file stands without its log line.
    let (message, context) =
        (format!("Payer {name} certificates replaced by {by}"), json!({ "payer": name, "by": &by }).to_string());
    let logged = line(&message, &context, ip);
    match state.store.touch_payer(name, &logged).await {
        Ok(true) => drop_aside(aside).await,
        Ok(false) => {
            undo_install(folder, &place, aside).await;
            return Err(PayerError::NotFound);
        }
        Err(e) => {
            undo_install(folder, &place, aside).await;
            let what = "The change could not be recorded, so the certificates were not replaced";
            return Err(failed(name, what, e));
        }
    }

    reload(state).await;
    publish_log(state, &logged).await;
    Ok(())
}

/// Gives a number a new nickname. Its name and number never change.
///
/// # Errors
///
/// [`PayerError::Invalid`] for the nickname or `by`, [`PayerError::NotFound`] when no number has
/// that name, [`PayerError::Failed`] when the store fails.
pub async fn set_nickname(
    state: &SharedState,
    name: &str,
    request: &NicknameRequest,
    ip: &str,
) -> std::result::Result<(), PayerError> {
    let nickname = plain_text(&request.nickname, "nickname", MAX_NICKNAME).map_err(PayerError::Invalid)?;
    let by = plain_text(&request.by, "by", MAX_BY).map_err(PayerError::Invalid)?;
    let _change = state.payer_changes.lock().await;
    let (message, context) = (
        format!("Payer {name} nickname changed to {nickname} by {by}"),
        json!({ "payer": name, "nickname": &nickname, "by": &by }).to_string(),
    );
    let logged = line(&message, &context, ip);
    match state.store.set_payer_nickname(name, &nickname, &logged).await {
        Ok(true) => {}
        Ok(false) => return Err(PayerError::NotFound),
        Err(e) => return Err(failed(name, "Could not store the nickname", e)),
    }

    reload(state).await;
    publish_log(state, &logged).await;
    Ok(())
}

/// Disables a number, so it takes no new payouts, or enables it again. Its earlier payouts settle
/// either way.
///
/// # Errors
///
/// [`PayerError::Invalid`] for `by` or `note`, [`PayerError::NotFound`] when no number has that
/// name, [`PayerError::Failed`] when the store fails.
pub async fn set_disabled(
    state: &SharedState,
    name: &str,
    disabled: bool,
    request: &PersonRequest,
    ip: &str,
) -> std::result::Result<(), PayerError> {
    let by = plain_text(&request.by, "by", MAX_BY).map_err(PayerError::Invalid)?;
    let note = match request.note.as_deref().filter(|note| !note.trim().is_empty()) {
        Some(note) => Some(plain_text(note, "note", MAX_NOTE).map_err(PayerError::Invalid)?),
        None => None,
    };
    let _change = state.payer_changes.lock().await;
    let what = if disabled { "disabled" } else { "enabled" };
    let (message, context) = (
        format!("Payer {name} {what} by {by}"),
        json!({ "payer": name, "by": &by, "note": note }).to_string(),
    );
    let logged = line(&message, &context, ip);
    match state.store.set_payer_disabled(name, disabled, &logged).await {
        Ok(true) => {}
        Ok(false) => return Err(PayerError::NotFound),
        Err(e) => return Err(failed(name, "Could not store the change", e)),
    }

    reload(state).await;
    publish_log(state, &logged).await;
    Ok(())
}

/// Gives a number a new key and returns it. The old one stops working at once, on every
/// instance. `current` is the hash of the key the request came with, and the replacement happens
/// only while that is still the number's key. The server's own command passes `None`.
///
/// # Errors
///
/// [`PayerError::Invalid`] for `by` or `note`, [`PayerError::NotFound`] when no number has that
/// name, [`PayerError::Conflict`] when `current` was replaced first, [`PayerError::Failed`] when
/// the store fails.
pub async fn replace_key(
    state: &SharedState,
    name: &str,
    current: Option<&str>,
    request: &PersonRequest,
    ip: &str,
) -> std::result::Result<String, PayerError> {
    let by = plain_text(&request.by, "by", MAX_BY).map_err(PayerError::Invalid)?;
    let note = match request.note.as_deref().filter(|note| !note.trim().is_empty()) {
        Some(note) => Some(plain_text(note, "note", MAX_NOTE).map_err(PayerError::Invalid)?),
        None => None,
    };
    let key = new_key();
    let (message, context) = (
        format!("Payer {name} key replaced by {by}"),
        json!({ "payer": name, "by": &by, "note": note }).to_string(),
    );
    let logged = line(&message, &context, ip);
    match state.store.replace_payer_key(name, current, &key.hash, &logged).await {
        Ok(true) => {}
        Ok(false) if current.is_some() => {
            return Err(PayerError::Conflict(
                "This key was replaced while the request was made, so it no longer works. Use the \
                 key the other replacement returned."
                    .into(),
            ));
        }
        Ok(false) => return Err(PayerError::NotFound),
        Err(e) => return Err(failed(name, "Could not store the new key", e)),
    }
    state.stream.revoke(name, &key.hash);
    publish_log(state, &logged).await;
    Ok(key.key)
}

// The line a change to a number is logged with, written with the change. `ip` is blank for the
// server's own commands.
fn line<'a>(message: &'a str, context: &'a str, ip: &'a str) -> LogLine<'a> {
    LogLine { level: "info", message, context: Some(context), ip: (!ip.is_empty()).then_some(ip) }
}

/// Every number in `roster` as an operator reads it on the server: its name, nickname and number,
/// whether it can pay, and when each certificate expires. `now` is seconds since the Unix epoch.
pub fn listing(roster: &Roster, now: i64) -> String {
    if roster.entries().is_empty() {
        return "No Swish number is stored.\n".into();
    }
    let expiry = |which: &str, at: i64| {
        let days = (at - now).div_euclid(86_400);
        format!("  {which} certificate expires {}, in {days} days\n", crate::util::time::to_stockholm(at).date())
    };
    let mut out = String::new();
    for entry in roster.entries() {
        let record = &entry.record;
        let state = match (&entry.loaded, record.disabled) {
            (Err(problem), _) => format!("cannot pay: {problem}"),
            (Ok(_), true) => "disabled".into(),
            (Ok(_), false) => "ready".into(),
        };
        out.push_str(&format!("{} ({}), {}: {state}\n", record.name, record.nickname, record.number));
        if let Ok(payer) = &entry.loaded {
            out.push_str(&expiry("TLS", payer.tls_expires));
            out.push_str(&expiry("signing", payer.signing_expires));
        }
    }
    out
}

// Both files, checked as a load would check them, and proven usable before anything is written.
fn usable(
    name: &str,
    nickname: &str,
    number: &str,
    tls: &str,
    signing: &str,
) -> std::result::Result<(), PayerError> {
    let tls = inspect(tls, number).map_err(|e| PayerError::Invalid(format!("tls: {e}")))?;
    let signing = inspect(signing, number).map_err(|e| PayerError::Invalid(format!("signing: {e}")))?;
    let payer = Payer {
        name: name.to_string(),
        nickname: nickname.to_string(),
        number: number.to_string(),
        tls,
        signing,
    };
    PayerState::connect(&payer).map(drop).map_err(|e| PayerError::Invalid(e.to_string()))
}

async fn stored(state: &SharedState, name: &str) -> std::result::Result<PayerRecord, PayerError> {
    let records = state
        .store
        .payers()
        .await
        .map_err(|e| failed(name, "Could not read the numbers", e))?;
    records.into_iter().find(|record| record.name == name).ok_or(PayerError::NotFound)
}

// A change that failed. Why goes to the server's log, for the operator: its text can name a table,
// a constraint or a folder, none of which the caller needs.
fn failed(name: &str, what: &str, e: impl std::fmt::Display) -> PayerError {
    tracing::warn!("[payers] {name}: {what}: {e}");
    PayerError::Failed(format!("{what}."))
}

// This instance takes its own change at once rather than at the next refresh.
async fn reload(state: &SharedState) {
    if let Err(e) = refresh(state).await {
        tracing::warn!("[payers] store error reading the numbers after a change: {e}");
    }
}

// Both files written under their final names into a temporary folder of their own.
async fn stage(
    folder: &Path,
    place: &Place,
    name: &str,
    tls: &str,
    signing: &str,
) -> std::result::Result<PathBuf, PayerError> {
    let (folder, tls, signing) = (folder.to_path_buf(), tls.to_string(), signing.to_string());
    let (tls_file, signing_file) = (place.tls.clone(), place.signing.clone());
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let staged = temporary(&folder, INCOMING);
        create_private_dir(&staged)?;
        let written = write_private(&staged.join(tls_file), &tls)
            .and_then(|()| write_private(&staged.join(signing_file), &signing))
            .and_then(|()| sync_dir(&staged));
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }
        Ok(staged)
    })
    .await
    .map_err(|e| failed(name, "Could not write the certificate files", e))?
    .map_err(|e| failed(name, "Could not write the certificate files", e))
}

// The staged folder takes the number's place. The one it replaces is kept aside and returned, for
// the caller to drop once the change is recorded, or to put back if it cannot be.
async fn install(folder: &Path, staged: PathBuf, place: &Place) -> Result<Option<PathBuf>> {
    let (folder, target) = (folder.to_path_buf(), place.folder.clone());
    tokio::task::spawn_blocking(move || -> Result<Option<PathBuf>> {
        let old = temporary(&folder, REPLACED);
        let replacing = match std::fs::rename(&target, &old) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(err(format!("Cannot move {} aside: {e}", target.display())));
            }
        };
        if let Err(e) = std::fs::rename(&staged, &target) {
            if replacing {
                let _ = std::fs::rename(&old, &target);
            }
            let _ = std::fs::remove_dir_all(&staged);
            return Err(err(format!("Cannot put {} in place: {e}", target.display())));
        }
        sync_dir(&folder)?;
        Ok(replacing.then_some(old))
    })
    .await
    .map_err(|e| err(format!("the write was interrupted: {e}")))?
}

async fn drop_aside(aside: Option<PathBuf>) {
    if let Some(aside) = aside {
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(aside)).await;
    }
}

// The new files leave the number's place, as a write cut short would leave them, and the ones moved
// aside come back. A crash between the two is put right by the cleanup, as after any replacement.
async fn undo_install(folder: &Path, place: &Place, aside: Option<PathBuf>) {
    let (folder, target) = (folder.to_path_buf(), place.folder.clone());
    let _ = tokio::task::spawn_blocking(move || {
        let new = temporary(&folder, INCOMING);
        if let Err(e) = std::fs::rename(&target, &new) {
            tracing::warn!("[payers] cannot take the new files out of {}: {e}", target.display());
            return;
        }
        if let Some(aside) = aside
            && let Err(e) = std::fs::rename(&aside, &target)
        {
            tracing::warn!("[payers] cannot put {} back from {}: {e}", target.display(), aside.display());
        }
        let _ = sync_dir(&folder);
        let _ = std::fs::remove_dir_all(&new);
    })
    .await;
}

async fn discard(staged: PathBuf) {
    let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(staged)).await;
}

// Beside the numbers' folders, and starting with a dot, so it can never be one. Dated in its name:
// a rename leaves a folder's times alone, and a shared folder's times come from other clocks.
fn temporary(folder: &Path, kind: &str) -> PathBuf {
    let made = crate::util::time::unix_now();
    folder.join(format!("{kind}{made}-{}", crate::swish::random_payout_uuid()))
}

// When a temporary folder was made, read from its name, or `None` for any other name.
fn made_at(name: &str) -> Option<i64> {
    let rest = name.strip_prefix(INCOMING).or_else(|| name.strip_prefix(REPLACED))?;
    rest.split_once('-')?.0.parse().ok()
}

async fn tidy(state: SharedState) {
    let mut tick = tokio::time::interval(TIDY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let folder = state.config.cert_dir.clone();
        let _ = tokio::task::spawn_blocking(move || remove_leftovers(&folder)).await;
    }
}

// Only what is dated over an hour ago goes: anything newer may be another instance's write still
// in progress on a shared folder. `remove_dir_all` removes a link itself, never what it points to.
fn remove_leftovers(folder: &Path) {
    put_back(folder);
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("[payers] cannot read {} for writes cut short: {e}", folder.display());
            return;
        }
    };
    let oldest = crate::util::time::unix_now() - LEFTOVER_AGE;
    for entry in entries.flatten() {
        if !entry.file_name().to_str().and_then(made_at).is_some_and(|made| made <= oldest) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!("[payers] removed {}, left by a write cut short", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("[payers] cannot remove {}, left by a write cut short: {e}", path.display());
            }
        }
    }
}

// A replacement cut short between its two moves leaves a number with no folder, and its files in a
// `.replaced-` one: the files the stored row and every running instance still use. Those go back.
fn put_back(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let mut newest: HashMap<String, (i64, PathBuf)> = HashMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(made) = made_at(&name).filter(|_| name.starts_with(REPLACED)) else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(number) = number_in(&entry.path()) else { continue };
        if newest.get(&number).is_none_or(|(latest, _)| made > *latest) {
            newest.insert(number, (made, entry.path()));
        }
    }
    for (number, (_, aside)) in newest {
        let home = folder.join(&number);
        if std::fs::symlink_metadata(&home).is_ok() {
            continue;
        }
        match std::fs::rename(&aside, &home).and_then(|()| sync_dir(folder).map_err(std::io::Error::other)) {
            Ok(()) => tracing::warn!("[payers] put {} back, moved aside by a replacement cut short", home.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("[payers] cannot put {} back from {}: {e}", home.display(), aside.display()),
        }
    }
}

// The Swish number a folder's files are named for, as `tls-{number}-{name}.pem`.
fn number_in(folder: &Path) -> Option<String> {
    std::fs::read_dir(folder).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        let number = name.strip_prefix("tls-")?.split_once('-')?.0;
        is_valid_swish_number(number).then(|| number.to_string())
    })
}
