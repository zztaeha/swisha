//! Announcing payout changes, to subscribers here and to listeners elsewhere.
//!
//! Each payer has two channels, named for the payer: `swisha:{payer}:updates` carries status
//! changes and `swisha:{payer}:events` the audit trail. Service log lines go to `swisha:logs`.
//! `events` and `logs` include amounts and personal data; `updates` is the one safe to forward to
//! a browser. One payer's screen subscribes to its own channels and so never sees another payer's
//! payouts.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};
use crate::domain::errors::Language;
use crate::domain::payer::LogLine;
use crate::domain::payout::{EventRecord, StatusView};
use crate::domain::review;
use crate::domain::status::PayoutStatus;
use crate::state::SharedState;
use crate::store::PayoutStore;

/// One event as it reaches a subscriber.
///
/// Carried in process rather than read back from the database, so the stream never depends on
/// PostgreSQL `NOTIFY` reaching anyone.
#[derive(Clone, Debug)]
pub struct StreamEvent {
    /// The full channel name, prefix included.
    pub channel: String,
    /// The payer this concerns.
    pub payer: String,
    /// The payout this concerns.
    pub reference: String,
    /// The JSON body, already serialized.
    pub payload: String,
}

/// Fan-out to in-process subscribers, one channel per number.
///
/// A subscriber hears only the number it subscribed to, so an event wakes only that number's
/// streams, and a busy number can never make another number's subscriber fall behind. Service log
/// lines reach no subscriber: they span every number.
///
/// Lossy by design: a subscriber that falls behind misses messages rather than slowing the
/// payout path. Recovery is `GET /swish/status/{payer}/{reference}`, never the stream.
///
/// ```
/// use swisha::events::{EventStream, StreamEvent};
///
/// let stream = EventStream::default();
/// let mut subscriber = stream.subscribe("sthlm").expect("room for a stream");
/// stream.publish_for_test(StreamEvent {
///     channel: "swisha:sthlm:updates".into(),
///     payer: "sthlm".into(),
///     reference: "INV-1042".into(),
///     payload: "{}".into(),
/// });
/// assert_eq!(subscriber.try_recv().expect("delivered").reference, "INV-1042");
/// ```
#[derive(Clone)]
pub struct EventStream {
    inner: Arc<Fanout>,
}

struct Fanout {
    capacity: usize,
    per_number: usize,
    total: usize,
    numbers: Mutex<Numbers>,
    // Each open stream's key, by hash, with its number and the flag that ends its streams.
    keys: Mutex<HashMap<String, (String, watch::Sender<bool>)>>,
    sweeping: AtomicBool,
    // Raised once when the service stops, so every open subscription ends instead of holding
    // the stop for the whole drain.
    closing: watch::Sender<bool>,
}

// Each number's channel, and how many subscriptions are open over every number. Counted as each
// opens and as each drops: summing every channel under this lock took 0.8 s over 10,000 opens.
#[derive(Default)]
struct Numbers {
    // A slot holds a pointer, not four Strings: 6 KB a number's channel, against 17, and no copy per
    // reader.
    channels: HashMap<String, broadcast::Sender<Arc<StreamEvent>>>,
    open: usize,
}

impl EventStream {
    /// A stream buffering `capacity` messages per number for each of its subscribers, with at most
    /// [`STREAMS_PER_NUMBER`] subscribers on one number and [`STREAMS`] in all.
    pub fn new(capacity: usize) -> Self {
        Self::with_caps(capacity, STREAMS_PER_NUMBER, STREAMS)
    }

    /// The same, with at most `per_number` subscribers on one number and `total` in all.
    pub fn with_caps(capacity: usize, per_number: usize, total: usize) -> Self {
        Self {
            inner: Arc::new(Fanout {
                capacity,
                per_number,
                total,
                numbers: Mutex::new(Numbers::default()),
                keys: Mutex::new(HashMap::new()),
                sweeping: AtomicBool::new(false),
                closing: watch::Sender::new(false),
            }),
        }
    }

    /// A new subscription to one number's events. Messages published before this call are not
    /// replayed.
    ///
    /// # Errors
    ///
    /// [`StreamsFull`] when the number already holds as many subscribers as it may, or the stream
    /// does over every number. Counted under the lock, so a burst of requests cannot pass a cap.
    pub fn subscribe(&self, payer: &str) -> Result<Subscription, StreamsFull> {
        let mut numbers = lock(&self.inner.numbers);
        if numbers.open >= self.inner.total {
            return Err(StreamsFull::All);
        }
        let capacity = self.inner.capacity;
        let sender = numbers.channels.entry(payer.to_string()).or_insert_with(|| broadcast::channel(capacity).0);
        if sender.receiver_count() >= self.inner.per_number {
            return Err(StreamsFull::Number);
        }
        let receiver = sender.subscribe();
        numbers.open += 1;
        let slot = Slot { fanout: Arc::clone(&self.inner), payer: payer.to_string() };
        Ok(Subscription { receiver, slot })
    }

    /// Becomes `true` once, when the service is stopping. A subscription ends on it.
    pub fn closing(&self) -> watch::Receiver<bool> {
        self.inner.closing.subscribe()
    }

    /// Ends every subscription: the service is stopping, and a stream that never ends would
    /// hold the stop for the whole drain.
    pub fn close(&self) {
        self.inner.closing.send_replace(true);
    }

    /// How many subscriptions are open, over every number.
    pub fn subscriber_count(&self) -> usize {
        lock(&self.inner.numbers).open
    }

    /// How many numbers have a channel held. One goes with its last subscription.
    pub fn channel_count(&self) -> usize {
        lock(&self.inner.numbers).channels.len()
    }

    /// Publishes directly, exposed so the lossy behavior can be tested from outside the crate.
    pub fn publish_for_test(&self, event: StreamEvent) {
        self.send(event);
    }

    fn send(&self, event: StreamEvent) {
        let mut numbers = lock(&self.inner.numbers);
        if let Some(sender) = numbers.channels.get(&event.payer) {
            if sender.receiver_count() == 0 {
                numbers.channels.remove(&event.payer);
            } else {
                let _ = sender.send(Arc::new(event));
            }
        }
    }

    /// Becomes `true` once the key a stream was opened with, `key_hash`, no longer finds `payer`:
    /// [`sweep_keys`] checks, and a key replaced here ends its streams at once.
    pub fn key_revoked(&self, key_hash: &str, payer: &str) -> watch::Receiver<bool> {
        lock(&self.inner.keys)
            .entry(key_hash.to_string())
            .or_insert_with(|| (payer.to_string(), watch::Sender::new(false)))
            .1
            .subscribe()
    }

    /// Ends the streams of `payer` opened with any key but `current`, as when its key is replaced.
    pub fn revoke(&self, payer: &str, current: &str) {
        lock(&self.inner.keys).retain(|hash, (name, revoked)| {
            let ends = name == payer && hash != current;
            if ends {
                revoked.send_replace(true);
            }
            !ends
        });
    }

    /// Whether this call is the first, so one [`sweep_keys`] serves every stream on this instance.
    pub fn start_sweeping(&self) -> bool {
        !self.inner.sweeping.swap(true, Ordering::AcqRel)
    }
}

/// One subscription to a number's events: the broadcast receiver, which it derefs to, holding its
/// place under the caps until it is dropped.
pub struct Subscription {
    // Before the slot, so the receiver is gone when the slot counts the number's subscribers.
    receiver: broadcast::Receiver<Arc<StreamEvent>>,
    slot: Slot,
}

impl Subscription {
    /// The receiver, and the place it holds: keep the slot for as long as whatever wraps the
    /// receiver, since dropping it gives the place back.
    pub fn into_parts(self) -> (broadcast::Receiver<Arc<StreamEvent>>, Slot) {
        (self.receiver, self.slot)
    }
}

impl std::ops::Deref for Subscription {
    type Target = broadcast::Receiver<Arc<StreamEvent>>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl std::ops::DerefMut for Subscription {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}

/// A subscription's place under the caps. Dropped, it gives the place back, and its number's
/// channel too once no subscriber is left on it.
pub struct Slot {
    fanout: Arc<Fanout>,
    payer: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut numbers = lock(&self.fanout.numbers);
        numbers.open -= 1;
        if numbers.channels.get(&self.payer).is_some_and(|sender| sender.receiver_count() == 0) {
            numbers.channels.remove(&self.payer);
        }
    }
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new(PER_NUMBER)
    }
}

/// How many events each number's channel holds for a subscriber that has not read them yet. A
/// payout makes about six, so a stream can fall some 20 payouts behind before it misses one.
pub const PER_NUMBER: usize = 128;

/// How many event streams one number may hold open at once: plenty for a back office's screens,
/// and short of what one leaked key would need to take the files a payout needs.
pub const STREAMS_PER_NUMBER: usize = 64;

/// How many event streams one swisha holds open at once, over every number. Each holds a
/// connection, and so a file, that a payout to Swish might otherwise need.
pub const STREAMS: usize = 10_000;

/// Which cap stopped a new subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamsFull {
    /// The number already holds as many subscribers as it may.
    Number,
    /// The stream already holds as many subscribers as it may, over every number.
    All,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends the event streams whose key no longer finds their number, until the process stops:
/// every [`REFRESH`](crate::payers::REFRESH), one query for every open stream here. A store that
/// cannot be read is not a replaced key, so it never ends a stream.
pub async fn sweep_keys(state: SharedState) {
    let mut tick = tokio::time::interval(crate::payers::REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let open: Vec<String> = {
            let mut keys = lock(&state.stream.inner.keys);
            keys.retain(|_, (_, revoked)| revoked.receiver_count() > 0);
            keys.keys().cloned().collect()
        };
        if open.is_empty() {
            continue;
        }
        let found: HashMap<String, String> = match state.store.key_holders(&open).await {
            Ok(found) => found.into_iter().collect(),
            Err(e) => {
                tracing::warn!("[events] could not check the open streams' keys: {e}");
                continue;
            }
        };
        // A stream opened while the query ran is judged at the next sweep.
        let asked: HashSet<&String> = open.iter().collect();
        lock(&state.stream.inner.keys).retain(|hash, (name, revoked)| {
            let holds = found.get(hash) == Some(name) || !asked.contains(hash);
            if !holds {
                revoked.send_replace(true);
            }
            holds
        });
    }
}

// PostgreSQL refuses a NOTIFY channel over 63 bytes. With a payer name of at most 32, the longest
// here is 47.
const PREFIX: &str = "swisha";

/// One payer's audit trail channel. Carries amounts and the recipient's number.
///
/// The same names are used for PostgreSQL `LISTEN`:
///
/// ```
/// use swisha::events::{channel_events, channel_logs, channel_updates};
///
/// assert_eq!(channel_updates("sthlm"), "swisha:sthlm:updates");
/// assert_eq!(channel_events("sthlm"), "swisha:sthlm:events");
/// assert_eq!(channel_logs(), "swisha:logs");
/// ```
pub fn channel_events(payer: &str) -> String {
    format!("{PREFIX}:{payer}:events")
}

/// One payer's status change channel. Carries no personal data, not even who confirmed a payout
/// or why, so it is the one to forward onward.
pub fn channel_updates(payer: &str) -> String {
    format!("{PREFIX}:{payer}:updates")
}

/// The service log channel.
pub fn channel_logs() -> String {
    format!("{PREFIX}:logs")
}

/// What one audit trail entry records.
pub struct SwishLogFields {
    /// The payer the payout belongs to.
    pub payer:         String,
    /// The payout's reference.
    pub reference:    String,
    /// The `payoutInstructionUUID`, when one exists yet.
    pub swish_ref:     Option<String>,
    /// The status at the time.
    pub status:        Option<String>,
    /// Amount in SEK.
    pub amount:        Option<f64>,
    /// The recipient's Swish number.
    pub payee_alias: Option<String>,
    /// The Swish error code, on failures.
    pub error_code:    Option<String>,
    /// The error text, on failures.
    pub error_message: Option<String>,
    /// The address the request came from.
    pub ip:            Option<String>,
    /// Who did it, when a person did.
    pub actor:         Option<String>,
    /// Why, in their words.
    pub note:          Option<String>,
}

/// What a recorded event changes about its payout, written together with it by [`record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change<'a> {
    /// Nothing: the event only joins the trail.
    None,
    /// The status, where the status rules allow it. The event joins the trail either way, since
    /// what Swish said belongs there whether or not it moved the payout.
    Status(&'a str),
    /// `NEEDS_REVIEW`, where the status rules allow it, with the event only beside it.
    NeedsReview,
    /// The checks end, while the payout still holds this status.
    EndChecks(&'a str),
}

/// What [`record`] wrote: whether its change applied, and its event, which this process's streams
/// already carry and other processes hear once [`announce`](Self::announce) or
/// [`alone`](Self::alone) sends it.
#[must_use = "other processes hear the event only once it is announced or sent alone"]
pub struct Recorded {
    /// Whether the change applied.
    pub applied: bool,
    // The events channel and the event as published, when one was stored.
    message: Option<(String, String)>,
}

impl Recorded {
    /// Publishes the payout as it now stands, as [`announce`] does, and sends the event to other
    /// processes in the same statement, ahead of it.
    pub async fn announce(self, payer: &str, reference: &str, state: &SharedState) -> Option<serde_json::Value> {
        announce_after(self.message, payer, reference, state).await
    }

    /// [`announce`](Self::announce) when the change applied, and otherwise
    /// [`alone`](Self::alone), answering `None`.
    pub async fn announce_if_applied(
        self,
        payer: &str,
        reference: &str,
        state: &SharedState,
    ) -> Option<serde_json::Value> {
        if self.applied {
            self.announce(payer, reference, state).await
        } else {
            self.alone(state).await;
            None
        }
    }

    /// Sends the event to other processes on its own, where no announcement follows.
    pub async fn alone(self, state: &SharedState) {
        send_alone(self.message, state).await;
    }
}

async fn send_alone(message: Option<(String, String)>, state: &SharedState) {
    if let Some((channel, payload)) = message {
        notify(state, &[(&channel, &payload)]).await;
    }
}

/// Records one payout event together with the change it brings, both or neither. Once stored, the
/// event goes to this process's streams, and the answer carries it for other processes.
///
/// An `ERROR` or `DECLINED` refused over a payout a person confirmed paid is reported at error
/// level, in the trace and the service log: the recipient may not have the money. A `PAID` or
/// `DEBITED` on a payout a person redid is reported by [`announce`], which every caller that
/// applied one runs next.
///
/// # Errors
///
/// When the store wrote neither. Nothing is published then.
pub async fn record(
    event: &str,
    fields: SwishLogFields,
    change: Change<'_>,
    state: &SharedState,
) -> crate::error::Result<Recorded> {
    let (record, payload) = prepare(event, fields);
    let (applied, recorded) = match change {
        Change::None => {
            state.store.record_event(&record).await?;
            (false, true)
        }
        Change::Status(status) => (state.store.record_status(status, &record, true).await?, true),
        Change::NeedsReview => {
            let applied = state.store.record_status("NEEDS_REVIEW", &record, false).await?;
            (applied, applied)
        }
        Change::EndChecks(status) => {
            let ended = state.store.end_checks(status, &record).await?;
            (ended, ended)
        }
    };
    let message = recorded.then(|| {
        let channel = channel_events(&record.payer);
        fan_out(state, &channel, Some((&record.payer, &record.reference)), &payload);
        (channel, payload)
    });
    if applied
        && let Change::Status(status) = change
        && let Some(status) = PayoutStatus::parse(status)
    {
        state.followed.took(&record.payer, &record.reference, status);
    }
    if !applied && let Change::Status(status @ ("ERROR" | "DECLINED")) = change {
        failed_after_confirmation(&record.payer, &record.reference, status, state).await;
    }
    Ok(Recorded { applied, message })
}

// Publishes an event the store has written on the events channel, here and to other processes at
// once. Only the routes do: record leaves other processes to the announcement that follows it.
#[cfg(feature = "http")]
pub(crate) async fn published(state: &SharedState, record: &EventRecord, payload: &str) {
    let channel = channel_events(&record.payer);
    publish(state, &channel, Some((&record.payer, &record.reference)), payload).await;
}

// Swish reported a payout's money gone after a person redid it, so the recipient may be paid twice.
// Judged from announce's read after the write, which sees a redo committed while the write waited.
async fn paid_after_redo(payer: &str, reference: &str, view: &StatusView, state: &SharedState) {
    let Some(redo) = view.replaced_by.as_deref() else { return };
    let status = view.status.as_deref().unwrap_or_default();
    if !PayoutStatus::parse(status).is_some_and(PayoutStatus::is_terminal) {
        return;
    }
    let line = format!(
        "Payout {payer}/{reference} was redone as {redo}, and Swish now reports it {status}: the recipient may be paid twice"
    );
    tracing::error!("[events] {line}");
    let context = serde_json::json!({ "payer": payer, "reference": reference, "redo": redo, "status": status });
    log_admin_event("error", &line, context, "", state).await;
}

// Swish reported a payout failed after a person confirmed it paid. The store keeps the confirmation,
// so only this says one of the two is wrong.
async fn failed_after_confirmation(payer: &str, reference: &str, status: &str, state: &SharedState) {
    let view = match state.store.status_view(payer, reference).await {
        Ok(view) => view,
        Err(e) => {
            tracing::error!(
                "[events] could not tell whether {payer}/{reference}, refused {status}, was confirmed paid: {e}"
            );
            return;
        }
    };
    let Some(confirmation) = view.filter(|v| v.contradicted).and_then(|v| v.manual_confirmation) else {
        return;
    };
    let line = format!(
        "Payout {payer}/{reference} was confirmed paid by {}, and Swish now reports it {status}: the recipient may not have the money",
        confirmation.by
    );
    tracing::error!("[events] {line}");
    let context = serde_json::json!({
        "payer": payer, "reference": reference, "status": status, "confirmed_by": confirmation.by,
    });
    log_admin_event("error", &line, context, "", state).await;
}

/// Records one payout event and publishes it on the events channel. A failed write is tried again
/// every second for up to 30 seconds, and then logged.
///
/// Carries the amount and the recipient's number, so subscribers to this channel see personal
/// data.
pub async fn log_swish_event(event: &str, fields: SwishLogFields, state: &SharedState) {
    let (record, payload) = prepare(event, fields);
    let channel = channel_events(&record.payer);
    tokio::join!(
        written(event, &record, state),
        publish(state, &channel, Some((&record.payer, &record.reference)), &payload),
    );
}

// An event written alone has no later write to carry it, so a full pool must not cost the trail
// the row. 30 seconds stays inside the 60 a stop waits for a request in flight.
const TRAIL_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

async fn written(event: &str, record: &EventRecord, state: &SharedState) {
    let (payer, reference) = (&record.payer, &record.reference);
    let started = tokio::time::Instant::now();
    for tries in 1.. {
        let Err(e) = state.store.record_event(record).await else {
            if tries > 1 {
                tracing::info!("[events] recorded '{event}' for {payer}/{reference} at try {tries}");
            }
            return;
        };
        if started.elapsed() >= TRAIL_RETRY {
            tracing::error!("[events] could not record '{event}' for {payer}/{reference} in 30 seconds: {e}");
            return;
        }
        if tries == 1 {
            tracing::warn!("[events] failed to record '{event}' for {payer}/{reference}, trying again: {e}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

// The width of the trail's status and error code. Both can come from Swish at any length, and one
// too long would fail the write that records it.
const CODE_WIDTH: usize = 20;

fn clip(text: Option<String>) -> Option<String> {
    text.map(|text| match text.char_indices().nth(CODE_WIDTH) {
        Some((end, _)) => text[..end].to_string(),
        None => text,
    })
}

// An event as stored, and as published on the events channel.
pub(crate) fn prepare(event: &str, fields: SwishLogFields) -> (EventRecord, String) {
    let fields = SwishLogFields {
        status: clip(fields.status),
        error_code: clip(fields.error_code),
        ..fields
    };
    let payload_str = serde_json::json!({
        "payer":        &fields.payer,
        "reference":    &fields.reference,
        "swish_ref":     &fields.swish_ref,
        "event":         event,
        "status":        &fields.status,
        "amount":        fields.amount,
        "payee_alias": &fields.payee_alias,
        "error_code":    &fields.error_code,
        "error_message": &fields.error_message,
        "ip":            &fields.ip,
        "actor":         &fields.actor,
        "note":          &fields.note,
        "created_at":    crate::util::time::now().date_time(),
    })
    .to_string();

    let record = EventRecord {
        payer:         fields.payer,
        reference:     fields.reference,
        swish_ref:     fields.swish_ref,
        event:         event.to_string(),
        status:        fields.status,
        amount:        fields.amount,
        payee_alias:   fields.payee_alias,
        error_code:    fields.error_code,
        error_message: fields.error_message,
        ip:            fields.ip,
        actor:         fields.actor,
        note:          fields.note,
    };
    (record, payload_str)
}

/// Records one service log line and publishes it on the logs channel.
pub async fn log_admin_event(
    level:   &str,
    message: &str,
    context: serde_json::Value,
    ip:      &str,
    state:   &SharedState,
) {
    let context = context.to_string();
    let line = LogLine { level, message, context: Some(&context), ip: (!ip.is_empty()).then_some(ip) };
    tokio::join!(
        async {
            if let Err(e) = state.store.record_log(line.level, line.message, line.context, line.ip).await {
                tracing::error!("[events] log insert failed: {e}");
            }
        },
        publish_log(state, &line),
    );
}

/// Publishes a service log line on the logs channel without writing it, for one the store already
/// wrote with the change it records.
pub async fn publish_log(state: &SharedState, line: &LogLine<'_>) {
    let payload = serde_json::json!({
        "level":     line.level,
        "message":   line.message,
        "context":   line.context,
        "ip":        line.ip,
        "timestamp": crate::util::time::now().rfc3339(),
    })
    .to_string();
    publish(state, &channel_logs(), None, &payload).await;
}

/// The body of an `updates` event: a payout as it stands, with its error explained and what a
/// person may do about it.
///
/// `GET /swish/status/{payer}/{reference}` and a refused payout answer with this same shape, so a
/// client that missed a live notification can recover through the same handler.
///
/// ```
/// use swisha::domain::errors::Language;
/// use swisha::domain::payout::StatusView;
/// use swisha::events::update_payload;
///
/// let view = |status: &str, code: Option<&str>| StatusView {
///     status: Some(status.into()),
///     amount: 250.5,
///     swish_ref: Some("7CBA".into()),
///     error_code: code.map(Into::into),
///     checks_ended: false,
///     replaces: None,
///     replaced_by: None,
///     was_debited: false,
///     earlier_paid: false,
///     accepted: true,
///     send_unaccepted: false,
///     contradicted: false,
///     manual_confirmation: None,
/// };
/// let declined = view("DECLINED", Some("RF07"));
/// let declined = update_payload("sthlm", "INV-1042", &declined, Language::English);
/// assert_eq!(declined["error_category"], "user_fixable");
/// assert_eq!(declined["error_code"], "RF07");
/// assert_eq!(declined["actions"], serde_json::json!(["redo"]));
///
/// let paid = update_payload("sthlm", "INV-1043", &view("PAID", None), Language::Swedish);
/// assert!(paid["error_code"].is_null() && paid["review"].is_null());
/// assert_eq!(paid["actions"], serde_json::json!([]));
/// ```
pub fn update_payload(
    payer:     &str,
    reference: &str,
    view:      &StatusView,
    language:  Language,
) -> serde_json::Value {
    let status = view.status.as_deref().unwrap_or_default();
    let error_code = view.error_code.as_deref();
    // The three error fields stand or fall together: a code only beside a status that has failed.
    let info = crate::domain::errors::describe_failure(status, error_code);
    let actions = review::actions(view);
    let to_check = review::review(view);

    serde_json::json!({
        "payer":          payer,
        "reference":      reference,
        "status":         status,
        "swish_ref":      view.swish_ref,
        "error_code":     info.and(error_code),
        "error_message":  info.map(|i| i.message(language)),
        "error_category": info.map(|i| i.category.as_str()),
        "review":         to_check.map(|r| r.message(language)),
        "actions":        actions.iter().map(|action| action.as_str()).collect::<Vec<_>>(),
        "manual_confirmation": view.manual_confirmation.as_ref().map(|m| serde_json::json!({
            "by":   m.by,
            "note": m.note,
            "at":   crate::util::time::to_stockholm(m.at).rfc3339(),
        })),
        "replaces":       view.replaces,
        "replaced_by":    view.replaced_by,
    })
}

/// A payout as it now stands, shaped by [`update_payload`], or `None` when the payer has no
/// payout under that reference.
///
/// # Errors
///
/// When the store cannot be read.
pub async fn current_payload(
    payer: &str,
    reference: &str,
    state: &SharedState,
) -> crate::error::Result<Option<serde_json::Value>> {
    let language = state.config.error_language;
    Ok(state
        .store
        .status_view(payer, reference)
        .await?
        .map(|view| update_payload(payer, reference, &view, language)))
}

/// Publishes a payout as it now stands on its updates channel, and answers with it as the status
/// endpoint shows it, or `None` when it could not be read.
///
/// Read back from the store, so the stream always says what the status endpoint would, but for who
/// confirmed it and why: `updates` is the channel to show a browser, so `manual_confirmation`
/// carries `at` alone there, and `by` and `note` stay on `events` and the status endpoint. Record
/// the event that changed the payout first, so its error code is part of what is read.
///
/// A payout a person redid that is now `PAID` or `DEBITED` is reported at error level, in the
/// trace and the service log: the recipient may be paid twice. Every write that applies either
/// status is announced, so each is reported, and so is any later announcement of that payout.
pub async fn announce(payer: &str, reference: &str, state: &SharedState) -> Option<serde_json::Value> {
    announce_after(None, payer, reference, state).await
}

// Announces the payout, with `message`, an event's for other processes, sent ahead of it in one
// statement. Unread, the payout goes unannounced and the event out on its own.
async fn announce_after(
    message: Option<(String, String)>,
    payer: &str,
    reference: &str,
    state: &SharedState,
) -> Option<serde_json::Value> {
    let view = match state.store.status_view(payer, reference).await {
        Ok(Some(view)) => view,
        Ok(None) => {
            send_alone(message, state).await;
            return None;
        }
        Err(e) => {
            tracing::warn!("[events] could not read {payer}/{reference} to announce it: {e}");
            send_alone(message, state).await;
            return None;
        }
    };
    paid_after_redo(payer, reference, &view, state).await;
    let payload = update_payload(payer, reference, &view, state.config.error_language);
    let shown = match payload["manual_confirmation"].as_object() {
        Some(confirmation) => {
            let mut shown = payload.clone();
            shown["manual_confirmation"] = serde_json::json!({ "at": confirmation.get("at") });
            shown.to_string()
        }
        None => payload.to_string(),
    };
    let updates = channel_updates(payer);
    fan_out(state, &updates, Some((payer, reference)), &shown);
    match &message {
        Some((channel, event)) => notify(state, &[(channel, event), (&updates, &shown)]).await,
        None => notify(state, &[(&updates, &shown)]).await,
    }
    Some(payload)
}

// Every announcement goes two ways: to in-process SSE subscribers, and out through the
// configured notifier for listeners in other processes.
async fn publish(state: &SharedState, channel: &str, payout: Option<(&str, &str)>, payload: &str) {
    fan_out(state, channel, payout, payload);
    notify(state, &[(channel, payload)]).await;
}

fn fan_out(state: &SharedState, channel: &str, payout: Option<(&str, &str)>, payload: &str) {
    // A service log line belongs to no number, so no stream carries it.
    if let Some((payer, reference)) = payout {
        state.stream.send(StreamEvent {
            channel: channel.to_string(),
            payer: payer.to_string(),
            reference: reference.to_string(),
            payload: payload.to_string(),
        });
    }
}

// One statement for every message, so a change and the payout it leaves reach a listener together.
async fn notify(state: &SharedState, messages: &[(&str, &str)]) {
    let sent = match messages {
        [(channel, payload)] => state.notifier.publish(channel, payload).await,
        [first, second] => state.notifier.publish_both(*first, *second).await,
        _ => return,
    };
    if let Err(e) = sent {
        let channels = messages.iter().map(|(channel, _)| *channel).collect::<Vec<_>>().join("', '");
        tracing::warn!("notify failed on '{channels}': {e}");
    }
}
