//! `GET /events`, the server-sent event stream.

use std::convert::Infallible;

use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use serde::Deserialize;
use tokio_stream::{Stream, StreamExt, wrappers::{BroadcastStream, WatchStream}};

use crate::error::ApiError;
use crate::events::{StreamEvent, StreamsFull};
use crate::http::access;
use crate::state::SharedState;

#[derive(Debug, Deserialize)]
/// Query parameters for the event stream. All are optional.
pub struct EventFilter {
    /// Channel kind: `events` or `updates`. Omit for both. `logs` is refused: the service log
    /// covers every number, and a stream is one number's.
    pub channel: Option<String>,
    /// The number whose key the request carries, when given. The stream holds only that number's
    /// events either way.
    pub payer: Option<String>,
    /// Only events for one payout.
    pub reference: Option<String>,
}

/// `GET /events`. One number's server-sent event stream, lossy by design.
///
/// Fed from the number's own in-process broadcast, not the database. A subscriber that falls behind
/// misses messages instead of slowing payouts, and every stream ends when the service stops.
/// Anything that must not be missed is read back from `GET /swish/status/{payer}/{reference}`.
/// A stream also ends when its key is replaced: at once when that happens on this instance, and
/// within [`REFRESH`](crate::payers::REFRESH) when it happens on another. One number holds at most
/// [`STREAMS_PER_NUMBER`](crate::events::STREAMS_PER_NUMBER) streams, and swisha
/// [`STREAMS`](crate::events::STREAMS) over every number.
///
/// # Errors
///
/// `401` without the secret or a number's key, `403` for another number's `payer` or the `logs`
/// channel, `429` past either cap on open streams, saying which, and a `400` for a query it cannot
/// read or a channel other than `events` or `updates`.
pub async fn events(
    State(state): State<SharedState>,
    headers: HeaderMap,
    filter: Result<Query<EventFilter>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let holder = access::number(&state, &headers).await?;
    let hash = access::presented(&headers).ok_or(ApiError::UnknownKey)?;
    let Query(filter) = filter?;

    let EventFilter { channel, payer, reference } = filter;
    if let Some(name) = payer.as_deref() {
        access::covers(&holder, name)?;
    }
    match channel.as_deref() {
        None | Some("events" | "updates") => {}
        Some("logs") => {
            return Err(ApiError::Forbidden(
                "The service log covers every number, so no number's key streams it. Read it on \
                 the server, in swisha_logs."
                    .into(),
            ));
        }
        // A kind that does not exist would open a stream that never carries anything.
        Some(_) => return Err(ApiError::BadRequest("channel must be events or updates.".into())),
    }
    let payer = holder.name;
    let subscription = state.stream.subscribe(&payer).map_err(|full| ApiError::Rejected {
        status: 429,
        message: match full {
            StreamsFull::Number => "Too many event streams are open for this number. Close one, or share one stream among your screens.",
            StreamsFull::All => "Too many event streams are open on swisha, over every number. Try again shortly.",
        }
        .into(),
    })?;
    let revoked = state.stream.key_revoked(&hash, &payer);
    if state.stream.start_sweeping() {
        tokio::spawn(crate::events::sweep_keys(state.clone()));
    }

    // The slot lives in the stream's closure, so the place under the caps goes with the stream.
    let (receiver, slot) = subscription.into_parts();
    let live = BroadcastStream::new(receiver).filter_map(move |event| {
        let _held = &slot;
        // A lagged subscriber is dropped messages, not a broken stream: keep going.
        let event = event.ok()?;

        if !matches(&event, channel.as_deref(), reference.as_deref()) {
            return None;
        }

        // The SSE event name is the channel kind, so a client can switch on it directly.
        let name = kind(&event).to_string();
        Some(Ok(Event::default().event(name).data(&event.payload)))
    });

    // The stream ends when the service stops, so a stop never waits on a subscriber. A client
    // reconnects and reads back anything it missed from the status endpoint.
    let closing = WatchStream::new(state.stream.closing()).filter(|closing| *closing).map(|_| None);
    let revoked = WatchStream::new(revoked).filter(|revoked| *revoked).map(|_| None);
    let stream = live.map(Some).merge(closing).merge(revoked).map_while(|item| item);

    // Proxies drop idle connections; a periodic comment keeps the stream alive.
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Whether one event passes the requested filters, within the stream's own number.
///
/// Both filters must match.
pub fn matches(event: &StreamEvent, channel: Option<&str>, reference: Option<&str>) -> bool {
    channel.is_none_or(|channel| kind(event) == channel)
        && reference.is_none_or(|reference| event.reference == reference)
}

// The last segment of a channel name, such as `updates` in `{prefix}:{payer}:updates`.
fn kind(event: &StreamEvent) -> &str {
    event.channel.rsplit_once(':').map_or(&event.channel, |(_, kind)| kind)
}
