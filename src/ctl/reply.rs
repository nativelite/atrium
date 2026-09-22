//! Replies: the typed `Reply` and the JSON builders the server answers with.

#[cfg(doc)]
use super::ctl_cmd;
use super::AgentId;
use json::{Number, Value};

// ---- reply builders (compact JSON via `Value`'s Display) ------------------

pub(super) fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub(super) fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn i(n: usize) -> Value {
    Value::Number(Number::Int(n as i64))
}

/// Every reply the control server sends — one variant per wire shape.
///
/// The request side has always been typed (`Cmd` and its payloads); the reply
/// side was ~17 free functions each hand-assembling a `Value` from string keys,
/// with the server then re-parsing its own JSON to learn what it had said (for the
/// audit record). A key typo or a field added in one place and not another was
/// invisible to the compiler (r10 audit B12). Now the shape of every reply is this
/// type, [`Reply::to_value`] is the single serializer, and the server keeps the
/// typed value until it writes the line. The `reply_*` functions remain as the
/// named constructors, so call sites read the same.
///
/// The wire format is unchanged, byte for byte (same keys, same order): external
/// consumers (agents parsing `atrium ctl` stdout) see no difference. Replies carry
/// no type tag, so decoding a reply still needs to know the request it answers —
/// a client-side concern this enum does not attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `{"ok":false,"err":<msg>}` — the one failure shape.
    Err(String),
    BoardEntry {
        key: String,
        entry: Option<Value>,
    },
    BoardList(Value),
    BoardDel {
        key: String,
        deleted: bool,
    },
    BoardClaim {
        key: String,
        granted: bool,
        holder: Option<String>,
        lease_ms: u64,
        entry: Option<Value>,
    },
    BoardRelease {
        key: String,
        released: bool,
    },
    BusPublished {
        event: Value,
        subscribers: usize,
    },
    BusSubscribed(Vec<String>),
    BusFeed {
        feed: Value,
        cursor: u64,
    },
    BusResolved {
        seq: u64,
        resolved: bool,
    },
    BusTopics(Vec<(String, usize)>),
    Spawned {
        pane: AgentId,
        role: Option<String>,
        session: Option<String>,
        note: Option<String>,
    },
    Sent {
        target: AgentId,
        queued: bool,
    },
    Killed(Vec<AgentId>),
    Audit {
        entries: Vec<Value>,
        oldest: Option<u64>,
        latest: u64,
    },
    StatusOne {
        pane: AgentId,
        status: Option<String>,
        idle_ms: u64,
    },
    Tree(Vec<ListNode>),
}

/// An owned org-chart node inside [`Reply::Tree`]; built from a [`TreeNode`].
#[derive(Debug, Clone, PartialEq)]
pub struct ListNode {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub role: Option<String>,
    pub title: String,
    pub depth: usize,
    pub status: Option<String>,
    pub idle_ms: u64,
}

fn u(n: u64) -> Value {
    Value::Number(Number::Int(n as i64))
}

fn opt_s(v: &Option<String>) -> Value {
    v.as_deref().map(s).unwrap_or(Value::Null)
}

impl Reply {
    /// Did the command succeed? (`false` only for [`Reply::Err`].)
    pub fn is_ok(&self) -> bool {
        !matches!(self, Reply::Err(_))
    }

    /// The single serializer: the exact wire object for this reply.
    pub fn to_value(&self) -> Value {
        let ok = ("ok", Value::Bool(true));
        match self {
            Reply::Err(msg) => obj(vec![("ok", Value::Bool(false)), ("err", s(msg))]),
            Reply::BoardEntry { key, entry } => obj(vec![
                ok,
                ("key", s(key)),
                ("entry", entry.clone().unwrap_or(Value::Null)),
            ]),
            Reply::BoardList(board) => obj(vec![ok, ("board", board.clone())]),
            Reply::BoardDel { key, deleted } => obj(vec![
                ok,
                ("key", s(key)),
                ("deleted", Value::Bool(*deleted)),
            ]),
            Reply::BoardClaim {
                key,
                granted,
                holder,
                lease_ms,
                entry,
            } => obj(vec![
                ok,
                ("key", s(key)),
                ("granted", Value::Bool(*granted)),
                ("holder", opt_s(holder)),
                ("lease_ms", u(*lease_ms)),
                ("entry", entry.clone().unwrap_or(Value::Null)),
            ]),
            Reply::BoardRelease { key, released } => obj(vec![
                ok,
                ("key", s(key)),
                ("released", Value::Bool(*released)),
            ]),
            Reply::BusPublished { event, subscribers } => obj(vec![
                ok,
                ("event", event.clone()),
                ("subscribers", i(*subscribers)),
            ]),
            Reply::BusSubscribed(topics) => obj(vec![
                ok,
                (
                    "subscribed",
                    Value::Array(topics.iter().map(|t| s(t)).collect()),
                ),
            ]),
            Reply::BusFeed { feed, cursor } => {
                obj(vec![ok, ("feed", feed.clone()), ("cursor", u(*cursor))])
            }
            Reply::BusResolved { seq, resolved } => obj(vec![
                ok,
                ("seq", u(*seq)),
                ("resolved", Value::Bool(*resolved)),
            ]),
            Reply::BusTopics(topics) => obj(vec![
                ok,
                (
                    "topics",
                    Value::Array(
                        topics
                            .iter()
                            .map(|(topic, subs)| obj(vec![("topic", s(topic)), ("subs", i(*subs))]))
                            .collect(),
                    ),
                ),
            ]),
            Reply::Spawned {
                pane,
                role,
                session,
                note,
            } => obj(vec![
                ok,
                ("pane", i(pane.0)),
                ("role", opt_s(role)),
                ("session", opt_s(session)),
                ("note", opt_s(note)),
            ]),
            Reply::Sent { target, queued } => obj(vec![
                ok,
                ("target", i(target.0)),
                ("queued", Value::Bool(*queued)),
            ]),
            Reply::Killed(killed) => obj(vec![
                ok,
                (
                    "killed",
                    Value::Array(killed.iter().map(|id| i(id.0)).collect()),
                ),
            ]),
            Reply::Audit {
                entries,
                oldest,
                latest,
            } => obj(vec![
                ok,
                ("audit", Value::Array(entries.clone())),
                ("oldest_seq", oldest.map(u).unwrap_or(Value::Null)),
                ("latest_seq", u(*latest)),
            ]),
            Reply::StatusOne {
                pane,
                status,
                idle_ms,
            } => obj(vec![
                ok,
                ("pane", i(pane.0)),
                ("status", opt_s(status)),
                ("idle_ms", u(*idle_ms)),
            ]),
            Reply::Tree(nodes) => obj(vec![
                ok,
                (
                    "tree",
                    Value::Array(
                        nodes
                            .iter()
                            .map(|n| {
                                obj(vec![
                                    ("id", i(n.id.0)),
                                    ("parent", n.parent.map(|p| i(p.0)).unwrap_or(Value::Null)),
                                    ("role", opt_s(&n.role)),
                                    ("title", s(&n.title)),
                                    ("depth", i(n.depth)),
                                    ("status", opt_s(&n.status)),
                                    ("idle_ms", u(n.idle_ms)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
        }
    }

    /// The reply as the one-line JSON the channel carries.
    pub fn to_json(&self) -> String {
        self.to_value().to_string()
    }
}

impl std::fmt::Display for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_value())
    }
}

/// `{"ok":false,"err":"<msg>"}`
pub fn reply_err(msg: &str) -> Reply {
    Reply::Err(msg.to_string())
}

/// `{"ok":true,"key":<key>,"entry":<entry|null>}` — a board `get`/`set` result.
/// `entry` (built by [`crate::board::entry_to_value`]) is `null` when the key is
/// absent. The caller passes the rendered value so this layer stays independent of
/// the board's internals.
pub fn reply_board_entry(key: &str, entry: Option<Value>) -> Reply {
    Reply::BoardEntry {
        key: key.to_string(),
        entry,
    }
}

/// `{"ok":true,"board":[{key,by,ms,fields}, …]}` — the whole board (a `list`).
pub fn reply_board_list(board: Value) -> Reply {
    Reply::BoardList(board)
}

/// `{"ok":true,"key":<key>,"deleted":<bool>}` — a board `del`.
pub fn reply_board_del(key: &str, deleted: bool) -> Reply {
    Reply::BoardDel {
        key: key.to_string(),
        deleted,
    }
}

/// `{"ok":true,"key":<key>,"granted":<bool>,"holder":<who|null>,"lease_ms":<n>,
/// "entry":<entry|null>}` — a board `claim`. On grant, `entry` carries the fresh
/// claim; on denial, `holder`/`lease_ms` name who holds it and until when.
pub fn reply_board_claim(key: &str, claim: &crate::board::Claim) -> Reply {
    use crate::board::{entry_to_value, Claim};
    let key = key.to_string();
    match claim {
        Claim::Granted(e) => Reply::BoardClaim {
            key,
            granted: true,
            holder: e.claimed_by.clone(),
            lease_ms: e.lease_ms,
            entry: Some(entry_to_value(e)),
        },
        Claim::Denied { holder, lease_ms } => Reply::BoardClaim {
            key,
            granted: false,
            holder: Some(holder.clone()),
            lease_ms: *lease_ms,
            entry: None,
        },
    }
}

/// `{"ok":true,"key":<key>,"released":<bool>}` — a board `release`.
pub fn reply_board_release(key: &str, released: bool) -> Reply {
    Reply::BoardRelease {
        key: key.to_string(),
        released,
    }
}

/// `{"ok":true,"event":<event>,"subscribers":<n>}` — a bus `pub` result: the
/// stored event (built by [`crate::bus::event_to_value`]) plus how many *other*
/// subscribers will receive it (no-echo: the publisher is excluded). `subscribers`
/// is an **additive** field — existing consumers that read only `event`/`ok` are
/// unaffected — and the client turns a `0` into a STDERR warning
/// ([`zero_sub_warning`]), never touching this stdout JSON.
pub fn reply_bus_published(event: Value, subscribers: usize) -> Reply {
    Reply::BusPublished { event, subscribers }
}

/// `{"ok":true,"subscribed":[<topic>,…]}` — the caller's full topic set after a
/// `sub`/`unsub`.
pub fn reply_bus_subscribed(topics: Vec<String>) -> Reply {
    Reply::BusSubscribed(topics)
}

/// `{"ok":true,"feed":[<event>,…],"cursor":<seq>}` — the pulled events (already
/// serialized + scoped by the caller) and the new cursor to pass as the next
/// `--since`. `cursor` is the max seq returned, or the request's `since` when the
/// pull was empty (so the cursor never goes backwards).
pub fn reply_bus_feed(feed: Value, cursor: u64) -> Reply {
    Reply::BusFeed { feed, cursor }
}

/// `{"ok":true,"seq":<seq>,"resolved":<bool>}` — a bus `resolve`.
pub fn reply_bus_resolved(seq: u64, resolved: bool) -> Reply {
    Reply::BusResolved { seq, resolved }
}

/// `{"ok":true,"topics":[{"topic":<t>,"subs":<n>},…]}` — the active bus topics
/// and how many subscribers each has, for `bus topics` discoverability. The
/// server passes `(topic, subscriber_count)` pairs already ordered (the bus keeps
/// them in a `BTreeMap`, so this is topic-sorted and deterministic).
///
/// Discoverability is the fix for the exact footgun that stranded a teammate:
/// publishing to a topic nobody follows looks identical to publishing to a live
/// one. `bus topics` makes the roster of live topics — and who is actually
/// listening — visible before you publish.
pub fn reply_bus_topics(topics: Vec<(String, usize)>) -> Reply {
    Reply::BusTopics(topics)
}

/// Below this many milliseconds a pane is "active enough" that showing an idle
/// duration is just noise — the status render suppresses idle under it, so only a
/// *meaningfully* idle pane grows an `idle …` marker. (Contract W2: split the
/// overloaded "idle" into busy / idle-at-prompt / stale.)
pub const IDLE_RENDER_MIN_MS: u64 = 10_000;

/// Humanize an idle duration for the status line: `"12s"`, `"4m"`, `"1h3m"`,
/// `"2d"`. Coarse on purpose — the question a reader asks is "how stale is this
/// pane", not "how many seconds exactly", so past an hour the seconds and past a
/// day the minutes stop earning their width.
pub fn humanize_idle(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        let (d, h) = (secs / 86_400, (secs % 86_400) / 3600);
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    }
}

/// The stderr advisory for a `bus pub` whose reply reports **0 subscribers** —
/// `Some("warning: published to '<topic>' with 0 subscribers")` — or `None` when
/// the reply is not a zero-subscriber `bus pub` (any subscriber count, a missing
/// `subscribers` field from an older daemon, or a non-pub reply).
///
/// This is the silent-drop footgun made loud, but kept OFF stdout: the JSON reply
/// on stdout stays byte-clean for consumers that `json::parse` it, and the client
/// ([`ctl_cmd`]) prints this to STDERR. Pure and side-effect-free so it unit-tests
/// without a daemon.
pub fn zero_sub_warning(reply: &str) -> Option<String> {
    let v = json::parse(reply).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // Only a pub reply carries a single `event`; require an explicit 0 count so a
    // legacy reply without the additive field never warns.
    let event = v.get("event")?;
    if v.get("subscribers").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let topic = event.get("topic").and_then(Value::as_str).unwrap_or("?");
    Some(format!(
        "warning: published to '{topic}' with 0 subscribers"
    ))
}

/// `{"ok":true,"pane":<id>,"role":<role|null>,"session":<sid|null>,"note":<note|null>}`.
/// `note` carries a non-fatal advisory (e.g. permission flags atrium stripped from
/// the spawn argv) so the caller sees it instead of it happening silently.
pub fn reply_spawned(
    pane: AgentId,
    role: Option<&str>,
    session: Option<&str>,
    note: Option<&str>,
) -> Reply {
    Reply::Spawned {
        pane,
        role: role.map(str::to_string),
        session: session.map(str::to_string),
        note: note.map(str::to_string),
    }
}

/// `{"ok":true,"target":<id>,"queued":<bool>}` — a `send` was accepted. `queued`
/// is true when the target was busy (delivery waits for it to go idle), false
/// when it will go out immediately. Delivery itself is asynchronous.
pub fn reply_sent(target: AgentId, queued: bool) -> Reply {
    Reply::Sent { target, queued }
}

/// `{"ok":true,"killed":[<id>,…]}` — the set of panes torn down by a `kill`
/// (the target plus every descendant), sorted. Empty only if the target
/// vanished between resolve and teardown.
pub fn reply_killed(killed: &[AgentId]) -> Reply {
    Reply::Killed(killed.to_vec())
}

/// `{"ok":true,"audit":[<entry>,…]}` — the (already-serialized, already-scoped)
/// audit entries, oldest-first. The caller builds each entry `Value` from its
/// own log so this crate stays free of the log's storage type.
pub fn reply_audit(entries: Vec<Value>, oldest: Option<u64>, latest: u64) -> Reply {
    // `oldest`/`latest` make eviction VISIBLE. The ring is bounded and drops the
    // tail without saying so, so a consumer could not distinguish "nothing
    // happened before this" from "the log has already rolled". `oldest > 1` means
    // entries are gone for good.
    Reply::Audit {
        entries,
        oldest,
        latest,
    }
}

/// `{"ok":true,"pane":<id>,"status":"<label>","idle_ms":<n>}` — a single target's
/// status. `idle_ms` is the same **additive** u64-millisecond field as on
/// [`reply_list`] nodes (`0` when active); the baseline `{pane,status}` keys are
/// unchanged.
pub fn reply_status_one(pane: AgentId, status: Option<&str>, idle_ms: u64) -> Reply {
    Reply::StatusOne {
        pane,
        status: status.map(str::to_string),
        idle_ms,
    }
}

/// One node in the org chart, as the run loop knows it. `idle_ms` is how long
/// (monotonic ms) since the pane last produced output — `0` for an active pane —
/// computed by the run loop via [`crate::ipc::idle_ms`].
pub struct TreeNode<'a> {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub role: Option<&'a str>,
    pub title: &'a str,
    pub depth: usize,
    pub status: Option<&'a str>,
    pub idle_ms: u64,
}

/// `{"ok":true,"tree":[{id,parent,role,title,depth,status,idle_ms}, …]}`. The
/// baseline keys `{id,parent,role,title,depth,status}` are unchanged; `idle_ms`
/// is an **additive** node field (u64 milliseconds, `0` when active) so consumers
/// that ignore unknown keys still parse.
pub fn reply_list(nodes: &[TreeNode]) -> Reply {
    Reply::Tree(
        nodes
            .iter()
            .map(|n| ListNode {
                id: n.id,
                parent: n.parent,
                role: n.role.map(str::to_string),
                title: n.title.to_string(),
                depth: n.depth,
                status: n.status.map(str::to_string),
                idle_ms: n.idle_ms,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_golden;
