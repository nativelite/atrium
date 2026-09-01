//! The shared **bus**: topic-routed pub/sub for a coordinating team — the
//! "what just happened, and does anyone need to act?" half of the coordination
//! layer, the active complement to the [`crate::board`]'s durable "what is
//! currently true".
//!
//! A team of agent panes that only shares a board must *poll* it — state without
//! notification. The bus fixes that: a teammate **publishes a structured event**
//! to a **topic**, and other teammates **pull** the events on the topics they
//! subscribe to. Two urgency classes: [`Kind::Fyi`] (cheap, informational, lands
//! on the feed) and [`Kind::DecisionNeeded`] (an escalation — something needs a
//! lead/human decision, surfaced actively). Like the board it lives in the amux
//! daemon (single broker process ⇒ a plain in-memory log behind the pipe, no
//! locking, no consensus).
//!
//! **Backpressure is built in from message one**, because unread messages cost
//! attention:
//! * *No echo* — you never receive your own events back ([`Bus::feed`] filters on
//!   `from`).
//! * *Pull, not push* — [`Bus::feed`] returns only events on topics the caller
//!   subscribed to (via [`Bus::subscribe`]); an agent pays only for topics it owns.
//! * *Per-agent rate cap* — [`Bus::publish`] rejects an agent that floods
//!   ([`RATE_MAX`] events per [`RATE_WINDOW_MS`]), so one looping worker can't
//!   storm the team.
//! * *Bounded ring* — at most [`RING_CAP`] events are retained; the oldest fall
//!   off. The log can never grow without bound.
//!
//! Optionally mirrored to a snapshot file (set `AMUX_BUS=<path>`) so the feed and
//! subscriptions survive a restart. Pure logic + a best-effort atomic write;
//! unit-tested without a terminal.
//!
//! Roadmap: this module and [`crate::board`] move into an `abus` org crate once
//! the coordination layer is clearly its own concern — see the roadmap.

use json::{Number, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// Environment knob naming a snapshot file to persist the bus to. Unset ⇒ the
/// bus is in-memory only (lost on exit). Opt-in, on top of `--allow-ctl`.
pub const ENV_BUS: &str = "AMUX_BUS";

/// The topic that subscribes to *everything* — the lead/operator firehose.
pub const TOPIC_ALL: &str = "*";

/// How many events the ring retains before the oldest fall off (backpressure:
/// the log is bounded, never unbounded).
pub const RING_CAP: usize = 512;

/// Per-agent publish rate cap: at most this many events per [`RATE_WINDOW_MS`].
/// A worker that exceeds it is told to slow down — one looping agent can't storm
/// the team. Generous enough that normal coordination never trips it.
pub const RATE_MAX: usize = 20;
/// The sliding window (ms) the rate cap counts publishes over.
pub const RATE_WINDOW_MS: u64 = 10_000;

/// The urgency class of an event — the visibility-vs-approval split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Cheap, informational. Lands on the feed; a teammate reads it when they
    /// pull their topics. The default — most coordination is FYI.
    Fyi,
    /// An escalation: something needs a lead/human decision. Surfaced actively
    /// (counted for the bar/panel) and stays "open" until [`Bus::resolve`]d.
    DecisionNeeded,
}

impl Kind {
    /// The canonical wire/label form.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Fyi => "fyi",
            Kind::DecisionNeeded => "decision_needed",
        }
    }

    /// Parse a kind keyword (with a couple of intuitive aliases). `None` for
    /// anything else — the caller reports a clear error.
    pub fn from_keyword(k: &str) -> Option<Kind> {
        match k {
            "fyi" | "info" | "note" => Some(Kind::Fyi),
            "decision_needed" | "decision" | "ask" | "escalate" => Some(Kind::DecisionNeeded),
            _ => None,
        }
    }
}

/// One published event: a structured message on a topic, from someone, at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Monotonic id (the feed cursor: `feed --since <seq>` resumes after it).
    pub seq: u64,
    pub topic: String,
    pub kind: Kind,
    /// The role/pane that published it (accountability + the no-echo key).
    pub from: Option<String>,
    pub ts_ms: u64,
    /// Freeform structured payload (`BTreeMap` ⇒ stable field order).
    pub fields: BTreeMap<String, String>,
    /// A `DecisionNeeded` event that has been answered. FYI events are never
    /// "open", so this stays false for them.
    pub resolved: bool,
}

/// The shared bus — a bounded event ring plus per-subscriber topic interests.
#[derive(Debug, Default)]
pub struct Bus {
    events: VecDeque<Event>,
    next_seq: u64,
    /// subscriber → the set of topics it pulls (may include [`TOPIC_ALL`]).
    subs: BTreeMap<String, BTreeSet<String>>,
    /// subscriber → recent publish timestamps, for the rolling rate cap.
    rate: BTreeMap<String, VecDeque<u64>>,
    path: Option<PathBuf>,
}

impl Bus {
    /// An in-memory bus (no persistence).
    pub fn new() -> Self {
        Self::default()
    }

    /// A bus backed by a snapshot file, loading any existing state. A missing,
    /// empty, or corrupt file simply starts empty — persistence is best-effort
    /// and never fatal.
    pub fn with_file(path: PathBuf) -> Self {
        let mut bus = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| parse_snapshot(&s))
            .unwrap_or_default();
        bus.path = Some(path);
        bus
    }

    /// Publish a structured event to `topic`. Enforces the per-agent rate cap
    /// (keyed on `from`; an operator with no `from` is uncapped) and the ring
    /// bound. Returns the stored event, or an error if the sender is over its
    /// rate cap. Persists if backed by a file.
    pub fn publish(
        &mut self,
        topic: &str,
        kind: Kind,
        from: Option<&str>,
        fields: &[(String, String)],
        now_ms: u64,
    ) -> Result<Event, String> {
        if let Some(who) = from {
            if !self.rate_admit(who, now_ms) {
                return Err(format!(
                    "rate cap: {who:?} published more than {RATE_MAX} events in \
                     {}s; slow down (backpressure)",
                    RATE_WINDOW_MS / 1000
                ));
            }
        }
        // Seqs are 1-based so the initial feed cursor `--since 0` fetches the
        // very first event (the filter is `seq > since`, exclusive).
        self.next_seq += 1;
        let seq = self.next_seq;
        let event = Event {
            seq,
            topic: topic.to_string(),
            kind,
            from: from.map(str::to_string),
            ts_ms: now_ms,
            fields: fields.iter().cloned().collect(),
            resolved: false,
        };
        self.events.push_back(event.clone());
        while self.events.len() > RING_CAP {
            self.events.pop_front();
        }
        self.persist();
        Ok(event)
    }

    /// Roll the rate window for `who` forward to `now_ms` and admit one publish,
    /// returning whether it fit under [`RATE_MAX`]. Only records the timestamp on
    /// admission, so a rejected publish doesn't count against the window.
    fn rate_admit(&mut self, who: &str, now_ms: u64) -> bool {
        let win = self.rate.entry(who.to_string()).or_default();
        let cutoff = now_ms.saturating_sub(RATE_WINDOW_MS);
        while win.front().is_some_and(|&t| t < cutoff) {
            win.pop_front();
        }
        if win.len() >= RATE_MAX {
            return false;
        }
        win.push_back(now_ms);
        true
    }

    /// Record `who`'s interest in `topics` (merged with any existing). Subscribe
    /// to [`TOPIC_ALL`] (`*`) to pull every topic — the lead/operator firehose.
    /// Persists if backed by a file.
    pub fn subscribe(&mut self, who: &str, topics: &[String]) {
        let set = self.subs.entry(who.to_string()).or_default();
        for t in topics {
            set.insert(t.clone());
        }
        self.persist();
    }

    /// Drop `who`'s interest in `topics`; if `topics` is empty, drop *all* of
    /// `who`'s subscriptions. Persists if backed by a file.
    pub fn unsubscribe(&mut self, who: &str, topics: &[String]) {
        if topics.is_empty() {
            self.subs.remove(who);
        } else if let Some(set) = self.subs.get_mut(who) {
            for t in topics {
                set.remove(t);
            }
            if set.is_empty() {
                self.subs.remove(who);
            }
        }
        self.persist();
    }

    /// The topics `who` subscribes to, if any.
    pub fn subscriptions(&self, who: &str) -> Option<&BTreeSet<String>> {
        self.subs.get(who)
    }

    /// The events `who` should pull: every event with `seq > since` on a topic
    /// `who` subscribes to (or any topic if `who` subscribed to [`TOPIC_ALL`]),
    /// **excluding `who`'s own events** (no echo). Oldest-first. Pull-not-push:
    /// an agent that subscribed to nothing gets nothing.
    pub fn feed(&self, who: &str, since: u64) -> Vec<&Event> {
        let subs = match self.subs.get(who) {
            Some(s) if !s.is_empty() => s,
            _ => return Vec::new(),
        };
        let all = subs.contains(TOPIC_ALL);
        self.events
            .iter()
            .filter(|e| e.seq > since)
            .filter(|e| e.from.as_deref() != Some(who))
            .filter(|e| all || subs.contains(&e.topic))
            .collect()
    }

    /// Every unresolved [`Kind::DecisionNeeded`] event, oldest-first — the open
    /// escalations the lead/human still owes an answer. Drives the bar/panel
    /// "N decisions" attention marker; unlike [`feed`](Bus::feed) it is global
    /// (the broker sees all open decisions, not a per-topic slice).
    pub fn pending_decisions(&self) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|e| e.kind == Kind::DecisionNeeded && !e.resolved)
            .collect()
    }

    /// Mark a `DecisionNeeded` event resolved (answered); `true` if it existed and
    /// was open. Persists if backed by a file.
    pub fn resolve(&mut self, seq: u64) -> bool {
        for e in &mut self.events {
            if e.seq == seq && !e.resolved {
                e.resolved = true;
                self.persist();
                return true;
            }
        }
        false
    }

    /// The most recent `n` events (any topic), oldest-first — the unfiltered tail
    /// for the panel/`feed --all` view.
    pub fn tail(&self, n: usize) -> Vec<&Event> {
        let start = self.events.len().saturating_sub(n);
        self.events.iter().skip(start).collect()
    }

    fn persist(&self) {
        if let Some(p) = &self.path {
            let _ = write_atomic(p, &snapshot(self));
        }
    }
}

/// Render an event as a JSON value:
/// `{"seq":…,"topic":…,"kind":…,"from":…|null,"ts":…,"resolved":…,"fields":{…}}`.
pub fn event_to_value(e: &Event) -> Value {
    let fields: Vec<(String, Value)> = e
        .fields
        .iter()
        .map(|(f, v)| (f.clone(), Value::String(v.clone())))
        .collect();
    Value::Object(vec![
        ("seq".to_string(), Value::Number(Number::Int(e.seq as i64))),
        ("topic".to_string(), Value::String(e.topic.clone())),
        ("kind".to_string(), Value::String(e.kind.as_str().to_string())),
        (
            "from".to_string(),
            e.from.clone().map(Value::String).unwrap_or(Value::Null),
        ),
        ("ts".to_string(), Value::Number(Number::Int(e.ts_ms as i64))),
        ("resolved".to_string(), Value::Bool(e.resolved)),
        ("fields".to_string(), Value::Object(fields)),
    ])
}

/// Render a slice of events as a JSON array.
pub fn events_to_value(events: &[&Event]) -> Value {
    Value::Array(events.iter().map(|e| event_to_value(e)).collect())
}

/// Serialize the whole bus (events + subscriptions + seq cursor) to a snapshot.
fn snapshot(bus: &Bus) -> String {
    let events = bus.events.iter().map(event_to_value).collect();
    let subs = bus
        .subs
        .iter()
        .map(|(who, topics)| {
            let arr = topics.iter().cloned().map(Value::String).collect();
            (who.clone(), Value::Array(arr))
        })
        .collect();
    Value::Object(vec![
        ("next_seq".to_string(), Value::Number(Number::Int(bus.next_seq as i64))),
        ("events".to_string(), Value::Array(events)),
        ("subs".to_string(), Value::Object(subs)),
    ])
    .to_string()
}

/// Parse a snapshot back into a bus (no file attached). `None` if the text is not
/// a JSON object; individual malformed events/subs are skipped, not fatal.
fn parse_snapshot(text: &str) -> Option<Bus> {
    let v = json::parse(text).ok()?;
    let mut bus = Bus::default();
    if let Some(events) = v.get("events").and_then(Value::as_array) {
        for ev in events {
            if let Some(e) = event_from_value(ev) {
                bus.events.push_back(e);
            }
        }
    }
    // next_seq is the last-used seq (1-based, pre-incremented on publish). Honor
    // the stored cursor, but never below max(seq) so ids stay unique even if the
    // snapshot was hand-edited.
    let stored = v.get("next_seq").and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
    let from_events = bus.events.iter().map(|e| e.seq).max().unwrap_or(0);
    bus.next_seq = stored.max(from_events);
    if let Some(subs) = v.get("subs").and_then(Value::as_object) {
        for (who, topics) in subs {
            let set: BTreeSet<String> = topics
                .as_array()
                .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if !set.is_empty() {
                bus.subs.insert(who.clone(), set);
            }
        }
    }
    Some(bus)
}

/// Parse one event `Value`; `None` if it lacks the required shape.
fn event_from_value(ev: &Value) -> Option<Event> {
    let seq = ev.get("seq").and_then(Value::as_i64)?.max(0) as u64;
    let topic = ev.get("topic").and_then(Value::as_str)?.to_string();
    let kind = Kind::from_keyword(ev.get("kind").and_then(Value::as_str)?)?;
    let from = ev.get("from").and_then(Value::as_str).map(str::to_string);
    let ts_ms = ev.get("ts").and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
    let resolved = ev.get("resolved").and_then(Value::as_bool).unwrap_or(false);
    let mut fields = BTreeMap::new();
    if let Some(fo) = ev.get("fields").and_then(Value::as_object) {
        for (f, fv) in fo {
            if let Some(val) = fv.as_str() {
                fields.insert(f.clone(), val.to_string());
            }
        }
    }
    Some(Event {
        seq,
        topic,
        kind,
        from,
        ts_ms,
        fields,
        resolved,
    })
}

/// Write `contents` to `path` atomically (temp file + rename), so a reader never
/// sees a half-written bus.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn publish_assigns_monotonic_seq_and_returns_event() {
        let mut b = Bus::new();
        let e0 = b.publish("deploy", Kind::Fyi, Some("dev_1"), &f(&[("msg", "merged")]), 100).unwrap();
        let e1 = b.publish("deploy", Kind::Fyi, Some("dev_2"), &f(&[("msg", "built")]), 101).unwrap();
        assert_eq!(e0.seq, 1);
        assert_eq!(e1.seq, 2);
        assert_eq!(e0.fields.get("msg").map(String::as_str), Some("merged"));
    }

    #[test]
    fn feed_is_topic_scoped_and_never_echoes_the_caller() {
        let mut b = Bus::new();
        b.subscribe("lead", &["deploy".to_string()]);
        b.subscribe("dev_1", &["deploy".to_string()]);
        b.publish("deploy", Kind::Fyi, Some("dev_1"), &f(&[("m", "x")]), 1).unwrap();
        b.publish("billing", Kind::Fyi, Some("dev_2"), &f(&[("m", "y")]), 2).unwrap();
        // lead subscribes to deploy → sees dev_1's deploy event, not billing.
        let lead = b.feed("lead", 0);
        assert_eq!(lead.len(), 1);
        assert_eq!(lead[0].topic, "deploy");
        // dev_1 subscribes to deploy but authored the only deploy event → no echo.
        assert!(b.feed("dev_1", 0).is_empty(), "no echo of own events");
    }

    #[test]
    fn feed_since_cursor_resumes_after_seq() {
        let mut b = Bus::new();
        b.subscribe("lead", &[TOPIC_ALL.to_string()]);
        let e0 = b.publish("t", Kind::Fyi, Some("a"), &f(&[]), 1).unwrap();
        let e1 = b.publish("t", Kind::Fyi, Some("a"), &f(&[]), 2).unwrap();
        let after0 = b.feed("lead", e0.seq);
        assert_eq!(after0.len(), 1);
        assert_eq!(after0[0].seq, e1.seq);
    }

    #[test]
    fn star_subscription_is_the_firehose() {
        let mut b = Bus::new();
        b.subscribe("lead", &[TOPIC_ALL.to_string()]);
        b.publish("a", Kind::Fyi, Some("x"), &f(&[]), 1).unwrap();
        b.publish("b", Kind::Fyi, Some("y"), &f(&[]), 2).unwrap();
        assert_eq!(b.feed("lead", 0).len(), 2, "* pulls every topic");
    }

    #[test]
    fn no_subscription_means_no_feed() {
        let mut b = Bus::new();
        b.publish("a", Kind::Fyi, Some("x"), &f(&[]), 1).unwrap();
        assert!(b.feed("nobody", 0).is_empty(), "pull-not-push: unsubscribed sees nothing");
    }

    #[test]
    fn rate_cap_rejects_a_flooding_agent_then_recovers() {
        let mut b = Bus::new();
        // Fill the window right up to the cap.
        for i in 0..RATE_MAX {
            b.publish("t", Kind::Fyi, Some("loud"), &f(&[]), 1000 + i as u64).unwrap();
        }
        // One more inside the window is rejected…
        let over = b.publish("t", Kind::Fyi, Some("loud"), &f(&[]), 1000 + RATE_MAX as u64);
        assert!(over.is_err(), "over-cap publish is rejected");
        // …and a rejected publish does not consume a slot or a seq.
        assert_eq!(b.tail(RATE_MAX + 1).len(), RATE_MAX);
        // Past the window, it publishes again.
        let ok = b.publish("t", Kind::Fyi, Some("loud"), &f(&[]), 1000 + RATE_WINDOW_MS + 1);
        assert!(ok.is_ok(), "after the window the agent recovers");
    }

    #[test]
    fn operator_without_from_is_uncapped() {
        let mut b = Bus::new();
        for i in 0..(RATE_MAX + 5) {
            b.publish("t", Kind::Fyi, None, &f(&[]), 1000 + i as u64).unwrap();
        }
        // No rejection for the un-attributed operator.
        assert_eq!(b.tail(RATE_MAX + 5).len(), RATE_MAX + 5);
    }

    #[test]
    fn ring_is_bounded_dropping_oldest() {
        let mut b = Bus::new();
        for i in 0..(RING_CAP + 10) {
            // Uncapped operator publishes so the ring, not the rate cap, is under test.
            b.publish("t", Kind::Fyi, None, &f(&[("n", &i.to_string())]), i as u64).unwrap();
        }
        assert_eq!(b.tail(RING_CAP + 100).len(), RING_CAP, "ring is capped");
        // The oldest fell off: with 1-based seqs 1..=RING_CAP+10, keeping the last
        // RING_CAP means the earliest retained seq is 11.
        let earliest = b.tail(RING_CAP).first().unwrap().seq;
        assert_eq!(earliest, 11);
    }

    #[test]
    fn pending_decisions_tracks_open_escalations() {
        let mut b = Bus::new();
        b.publish("t", Kind::Fyi, Some("a"), &f(&[]), 1).unwrap();
        let d = b.publish("t", Kind::DecisionNeeded, Some("a"), &f(&[("q", "ship?")]), 2).unwrap();
        assert_eq!(b.pending_decisions().len(), 1, "one open decision");
        assert!(b.resolve(d.seq), "resolve an open decision");
        assert!(b.pending_decisions().is_empty(), "resolved decision clears");
        assert!(!b.resolve(d.seq), "resolving twice is a no-op");
    }

    #[test]
    fn unsubscribe_removes_interest() {
        let mut b = Bus::new();
        b.subscribe("dev", &["a".to_string(), "b".to_string()]);
        b.unsubscribe("dev", &["a".to_string()]);
        let subs = b.subscriptions("dev").unwrap();
        assert!(!subs.contains("a") && subs.contains("b"));
        // Empty topics drops all.
        b.unsubscribe("dev", &[]);
        assert!(b.subscriptions("dev").is_none());
    }

    #[test]
    fn snapshot_round_trips_events_subs_and_cursor() {
        let mut b = Bus::new();
        b.subscribe("lead", &[TOPIC_ALL.to_string()]);
        b.publish("deploy", Kind::Fyi, Some("dev_1"), &f(&[("m", "merged")]), 10).unwrap();
        b.publish("deploy", Kind::DecisionNeeded, Some("dev_1"), &f(&[("q", "ship?")]), 11).unwrap();
        let snap = snapshot(&b);
        let restored = parse_snapshot(&snap).unwrap();
        assert_eq!(restored.tail(10).len(), 2);
        assert_eq!(restored.next_seq, 2, "cursor survives so new ids don't collide");
        assert_eq!(restored.pending_decisions().len(), 1);
        // Subscriptions survive, so the feed still resolves.
        let lead = restored.feed("lead", 0);
        assert_eq!(lead.len(), 2);
    }
}
