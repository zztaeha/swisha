#![cfg(feature = "http")]

use swisha::events::{EventStream, StreamEvent, channel_events, channel_updates};
use swisha::http::routes::events::matches;

fn event(channel: &str, reference: &str) -> StreamEvent {
    // The payer is the channel's middle segment, as events.rs publishes it.
    StreamEvent {
        channel: channel.to_string(),
        payer: channel.split(':').nth(1).unwrap_or_default().to_string(),
        reference: reference.to_string(),
        payload: "{}".into(),
    }
}

// PostgreSQL refuses a NOTIFY channel over 63 bytes, and every listener elsewhere would then hear
// nothing. The longest payer name has to fit.
#[test]
fn the_longest_channel_fits_postgres() {
    let longest = "a".repeat(32);
    assert!(channel_updates(&longest).len() <= 63, "{}", channel_updates(&longest));
    assert!(channel_events(&longest).len() <= 63, "{}", channel_events(&longest));
}

#[test]
fn no_filter_matches_everything() {
    assert!(matches(&event("swisha:sthlm:updates", "INV-1"), None, None));
    assert!(matches(&event("swisha:sthlm:events", "INV-2"), None, None));
}

// The channel filter names a kind, which every payer's channels share.
#[test]
fn the_channel_filter_matches_the_kind() {
    let e = event("swisha:sthlm:updates", "INV-1");
    assert!(matches(&e, Some("updates"), None));
    assert!(!matches(&e, Some("events"), None));
    assert!(!matches(&e, Some("swisha:sthlm:updates"), None), "a full name is not a kind");
}

#[test]
fn the_reference_filter_matches_one_payout() {
    assert!(matches(&event("swisha:sthlm:updates", "INV-1"), None, Some("INV-1")));
    assert!(!matches(&event("swisha:sthlm:updates", "INV-2"), None, Some("INV-1")));
}

// A stream carries its own number's events alone, so its filters narrow those, and both hold at
// once.
#[test]
fn every_filter_must_hold() {
    let e = event("swisha:sthlm:updates", "INV-1");
    assert!(matches(&e, Some("updates"), Some("INV-1")));
    assert!(!matches(&e, Some("events"), Some("INV-1")));
    assert!(!matches(&e, Some("updates"), Some("INV-9")));
}

#[tokio::test]
async fn subscribers_receive_what_is_published() {
    let stream = EventStream::new(16);
    let mut a = stream.subscribe("sthlm").expect("room for a stream");
    let mut b = stream.subscribe("sthlm").expect("room for a stream");
    assert_eq!(stream.subscriber_count(), 2);

    // publish() is private, so drive it the way events.rs does: through a send.
    let sent = event("swisha:sthlm:updates", "INV-1");
    stream.publish_for_test(sent.clone());

    for rx in [&mut a, &mut b] {
        let got = rx.recv().await.unwrap();
        assert_eq!(got.channel, "swisha:sthlm:updates");
        assert_eq!(got.payer, "sthlm");
        assert_eq!(got.reference, "INV-1");
    }
}

// Each number's events have a buffer of their own, so a busy number can never push a quiet one's
// events out before they are read, nor wake its streams.
#[test]
fn a_busy_number_never_pushes_out_another_numbers_events() {
    let stream = EventStream::default();
    let mut quiet = stream.subscribe("quiet").expect("room for a stream");
    let mut busy = stream.subscribe("busy").expect("room for a stream");
    stream.publish_for_test(event("swisha:quiet:updates", "Q-1"));
    for i in 0..5_000 {
        stream.publish_for_test(event("swisha:busy:updates", &format!("B-{i}")));
    }

    let got = quiet.try_recv().expect("the quiet number's own event, untouched by the other's 5,000");
    assert_eq!(got.reference, "Q-1");
    assert!(quiet.try_recv().is_err(), "nothing of the busy number's reaches the quiet one");
    assert!(
        matches!(busy.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))),
        "the busy number's own reader falls behind on its own traffic"
    );
}

// A number's channel goes with its last subscriber, so numbers streamed once and never again hold
// no buffer, and no subscribe has to look for them.
#[test]
fn a_numbers_channel_is_released_with_its_last_subscriber() {
    let stream = EventStream::default();
    for i in 0..1_000 {
        drop(stream.subscribe(&format!("n{i}")).expect("room for a stream"));
    }
    assert_eq!((stream.channel_count(), stream.subscriber_count()), (0, 0), "each went with its subscriber");
    let kept = stream.subscribe("last").expect("room for a stream");
    let other = stream.subscribe("last").expect("room for a stream");
    assert_eq!((stream.channel_count(), stream.subscriber_count()), (1, 2));
    drop(kept);
    assert_eq!((stream.channel_count(), stream.subscriber_count()), (1, 1), "kept while one is left");
    drop(other);
    assert_eq!((stream.channel_count(), stream.subscriber_count()), (0, 0));
}

// A stream past a cap is refused, saying which cap, and a closed stream frees its place.
#[test]
fn streams_are_capped_per_number_and_in_all() {
    use swisha::events::StreamsFull;
    let stream = EventStream::with_caps(16, 2, 3);
    let first = stream.subscribe("sthlm").expect("room");
    let _second = stream.subscribe("sthlm").expect("room");
    assert_eq!(stream.subscribe("sthlm").err(), Some(StreamsFull::Number));
    let _gbg = stream.subscribe("gbg").expect("another number has room");
    assert_eq!(stream.subscribe("gbg").err(), Some(StreamsFull::All));
    drop(first);
    stream.subscribe("sthlm").expect("a closed stream frees its place");
}
