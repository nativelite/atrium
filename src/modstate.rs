//! What the mod inside a pane's Claude Code tells the broker, and how far the
//! broker believes it.
//!
//! A pane that hosts `claude` can load a small plugin of function hooks (the
//! "mod", `mod/` in this repo). It speaks to atrium over the same control
//! channel as `atrium ctl`, with the pane's own capability token, and says
//! three things: `hello` (which version it is and which capabilities it
//! offers), `report` (the pane's own status, straight from the engine's
//! events, its context fill and cost, and the text of its last answer), and
//! `whoami` (a question: what is this pane). This module is the broker's
//! memory of those reports, pure over `now_ms` so every rule is unit-tested.
//!
//! # Trust
//!
//! A report is about the reporter. Every entry here is keyed by the agent id
//! the server resolved from the token, never by anything in the request, so a
//! pane cannot mark a sibling idle to get a delivery through or forge its
//! answer. A report is an **upgrade over the transcript inference**
//! ([`crate::vendors`], via `agsess`), never a requirement: a pane whose mod
//! never said hello, a codex pane, a claude too old to load mods, all keep the
//! inferred status. A report older than [`REPORT_STALE_MS`] stops counting, so
//! a mod that died mid-session cannot pin a pane at `working` forever.
//!
//! `errored` and `ended` are states the transcript inference cannot see. The
//! chrome has no colour for them yet; [`ModStatus::as_agsess`] says what the
//! border shows meanwhile, and `ctl status`/`ctl list` carry the real label.
//!
//! # Sight (Part II of `PLAN-mod.md`)
//!
//! Three more things a mod reports, all about itself: a **caption** (`doing`,
//! six words on what the pane is doing right now), the real paths it
//! **touched** with an edit (from which the broker computes **collisions**:
//! two panes editing the same file on different worktrees), and the **reply**
//! to a question the broker queued for it (`ctl ask`, answered by a fork of
//! the pane's own context, without interrupting its turn). The ask queue
//! lives here too, so the rules are unit-tested with the rest.

use std::collections::{HashMap, HashSet, VecDeque};

/// The capabilities a mod may declare and the broker will act on. Anything
/// else in a `hello` is dropped from `accepted` and never consulted.
pub const CAPS: &[&str] = &[
    "status", "inbox", "answer", "context", "tools", "guard", "ask",
];

/// A status report older than this is ignored and the inferred status is used
/// again. Two minutes: the mod reports on every turn boundary and tool call,
/// so a healthy pane refreshes far faster; a dead mod goes quiet for good.
pub const REPORT_STALE_MS: u64 = 120_000;

/// The most of an answer the broker keeps. An answer is read by a parent that
/// spawned the pane as a subagent, never shown as operator text.
pub const ANSWER_CAP: usize = 16 * 1024;

/// The most of a version or engine string the broker keeps.
pub const VERSION_CAP: usize = 64;

/// The most of a `reason` the broker keeps.
pub const REASON_CAP: usize = 200;

/// The most of a caption (`doing`) the broker keeps: six words, roughly.
pub const DOING_CAP: usize = 80;

/// The most touched paths the broker remembers per pane; past it a new path
/// is dropped (a pane that edits hundreds of files is a build, not a worker).
pub const TOUCH_CAP: usize = 256;

/// The most of a question (`ctl ask`) the broker queues.
pub const ASK_CAP: usize = 2_000;

/// The most of an ask's reply the broker keeps.
pub const ASK_REPLY_CAP: usize = 4_000;

/// Unanswered questions one pane may hold; the next `ask` is refused.
pub const ASK_QUEUE: usize = 8;

/// How long an ask's reply stays readable after it arrived.
pub const ASK_REPLY_TTL_MS: u64 = 600_000;

/// The question `Ctrl+A ?` asks the focused pane.
pub const ASK_DEFAULT_QUESTION: &str =
    "What are you doing right now, what do you need, and what, if anything, is blocking you?";

// ---- Health and the circuit breaker (PLAN 0.48.0, S2) ----
//
// The mod is an upgrade, never a requirement. Three things are watched, all
// pure over the time a tick is handed:
//
// 1. **The hello window.** A claude pane launched with the mod is expected
//    to say `hello` within [`HELLO_WINDOW_MS`] of the moment it is seen
//    started. One that does not is *set aside*: from then on it is a pane
//    without a mod (no cap, sends typed, status from the status hooks or by
//    inference), whatever it says later.
// 2. **The inbox.** A pane whose mod took the `inbox` cap collects its own
//    deliveries, and atrium types nothing into it. If that mod stops asking
//    for [`INBOX_SILENT_MS`], the cap is taken back and what is queued is
//    typed, so a send never waits on a mod that is gone.
// 3. **What the mod says of itself.** A `report` may carry failure counts.
//    They are hostile input: bounded, scrubbed, taken only from a pane that
//    has a mod, and able to do three things, all to the pane that sent
//    them: be shown, take back the capability of a feature the mod lost, and
//    set that pane's mod aside.
//
// Both windows run on the loop's own clock ([`ModState::health_tick`]): time
// atrium was not turning is held against no pane.
//
// The **breaker** is session-wide: once it trips, new claude panes launch
// without the mod; nothing that is running is restarted. It trips on
// [`TRIP_PANES`] voices. A voice is a seat (a pane, and whatever it is
// respawned as) that atrium itself saw miss the hello window, or one set
// aside by its own reports that has no such seat above it in the spawn tree
// (a pane and the children it spawned are one voice). One pane, lying as it
// likes and respawned as often as its lead likes, is one voice: it loses its
// own mod and nobody else's. And only what atrium saw is remembered past the
// session: the trip is written down when two seats missed the window, never
// on reports alone.

/// How long after a modded pane is seen started its `hello` may take, on the
/// loop's clock. The mod says hello from `session.start` and, when atrium
/// cannot be asked, tries again 3, 6, 12 and 24 s later (`HELLO_BACKOFF_S` in
/// `mod/hooks/health.ts`), each try bounded at 5 s: a mod that loaded gets
/// three or four tries inside thirty seconds and keeps trying after, and the
/// window itself stands still while atrium does. Thirty seconds is
/// also how long a send queued for a pane whose mod never loaded waits, at
/// the most, before it is typed.
pub const HELLO_WINDOW_MS: u64 = 30_000;

/// How long a mod that took the `inbox` cap may go without asking for its
/// deliveries before the cap is taken back. The mod holds one `inbox --wait`
/// at a time, which the broker answers within 60 s, and re-issues it at once
/// (or polls every 1.5 s): a live one is heard from at least every minute.
/// Three minutes is three missed holds.
pub const INBOX_SILENT_MS: u64 = 180_000;

/// The most one health pass adds to the loop's clock. The loop turns every
/// few milliseconds and a pass is due every [`TICK_MIN_MS`]; a gap of two
/// seconds or more between two passes is atrium not turning (a roster being
/// spawned, a slow disk, a suspended machine), and a pane cannot be answered
/// by an atrium that is not turning.
pub const TICK_MAX_MS: u64 = 2_000;

/// How often a health pass is due.
pub const TICK_MIN_MS: u64 = 250;

/// The most events kept between two passes; a session that runs no health
/// phase never collects them.
const EVENTS_CAP: usize = 256;

/// How many voices trip the breaker. Two: one pane is an accident or a liar,
/// two that atrium cannot tie to each other are a broken mod.
pub const TRIP_PANES: usize = 2;

/// What the count of a feature the mod lost to a missing engine method is
/// keyed with (`feature.inbox`).
pub const FEATURE_SITE: &str = "feature.";

/// The features a mod can lose by itself (`Feature` in `mod/hooks/health.ts`).
pub const FEATURES: &[&str] = &[
    "pane", "inbox", "tools", "guard", "paths", "captions", "ask",
];

/// The capabilities a lost feature takes with it. Native delivery and the
/// ask that rides it; the typed tools; the file guard. The pane, real paths
/// and captions stand behind no capability: losing one is recorded and costs
/// the broker's side nothing.
pub fn feature_caps(feature: &str) -> &'static [&'static str] {
    match feature {
        "inbox" => &["inbox", "ask"],
        "ask" => &["ask"],
        "tools" => &["tools"],
        "guard" => &["guard"],
        _ => &[],
    }
}

/// Where an `api` failure means the mod cannot work at all, so the whole of
/// it is set aside: finding atrium (`session.start`), saying hello, and
/// reporting. Everywhere else a missing engine method costs one feature.
pub const CORE_SITES: &[&str] = &["session.start", "hello", "report"];

/// Errors of its own hooks within [`FAIL_WINDOW_MS`]
/// ([`PaneFailures::thrown_recently`]) at which a pane's mod is set aside.
/// Five, the count at which the mod switches itself off (`INERT_AFTER` in
/// `mod/hooks/health.ts`), over the same window, of the same classes.
pub const FAIL_SET_ASIDE: u64 = 5;

/// The most of a failure message the broker keeps.
pub const FAIL_MSG_CAP: usize = 120;

/// The most hooks one pane's failure counts may name.
pub const FAIL_HOOKS_CAP: usize = 17;

/// The most of a hook's name the broker keeps.
pub const FAIL_HOOK_CAP: usize = 40;

/// The most one hook's count may read.
pub const FAIL_COUNT_CAP: u64 = 1_000_000;

/// What a refusal starts with once a pane's mod is set aside; the mod reads
/// it and stops asking (`SET_ASIDE` in `mod/hooks/health.ts`).
pub const MOD_SET_ASIDE: &str = "mod set aside";

/// The class of a mod's failure, as it reports it (`mod/hooks/health.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailClass {
    /// The plugin interface is not what the mod was written for.
    Api,
    /// A ctl call outlasted its deadline.
    Timeout,
    /// A ctl call could not be made.
    Ctl,
    /// A reply was not what the verb answers.
    Shape,
    /// Anything else a hook threw.
    Hook,
}

impl FailClass {
    /// The wire label.
    pub fn parse(s: &str) -> Option<FailClass> {
        match s.trim() {
            "api" => Some(FailClass::Api),
            "timeout" => Some(FailClass::Timeout),
            "ctl" => Some(FailClass::Ctl),
            "shape" => Some(FailClass::Shape),
            "hook" => Some(FailClass::Hook),
            _ => None,
        }
    }

    /// The label `ctl status` and `atrium mod status` print.
    pub fn label(self) -> &'static str {
        match self {
            FailClass::Api => "api",
            FailClass::Timeout => "timeout",
            FailClass::Ctl => "ctl",
            FailClass::Shape => "shape",
            FailClass::Hook => "hook",
        }
    }

    /// Does this class say the plugin interface changed?
    pub fn is_interface(self) -> bool {
        self == FailClass::Api
    }
}

/// The failure part of a `report`, as the request carries it: counts per
/// hook (cumulative since the mod started), and the last failure's class and
/// message. Parsed by [`parse_fail_counts`]; nothing here is trusted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureReport {
    pub counts: Vec<(String, u64)>,
    pub class: Option<FailClass>,
    pub message: Option<String>,
    /// Where the last failure happened: the key its count is under.
    pub at: Option<String>,
    /// How many of the failures were thrown by the mod's own hooks (class
    /// `hook` or `api`), cumulative: the only ones that count toward setting
    /// the mod aside. A timeout, an exchange that failed and a reply of the
    /// wrong shape are in the counts and not in this number.
    pub thrown: Option<u64>,
}

/// A failure's site as a `report` names it (`fail_at`): a hook's name, in the
/// alphabet [`parse_fail_counts`] takes.
pub fn parse_fail_site(s: &str) -> Result<String, String> {
    let s = s.trim();
    let named = !s.is_empty()
        && s.chars().count() <= FAIL_HOOK_CAP
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if named {
        Ok(s.to_string())
    } else {
        Err("fail_at is a hook's name: letters, digits, . _ -".to_string())
    }
}

/// `tool.call:2,whoami:1` as `[("tool.call", 2), ("whoami", 1)]`. A hook's
/// name is letters, digits, `.`, `_` and `-`, at most [`FAIL_HOOK_CAP`] of
/// them; a count is a whole number, read up to [`FAIL_COUNT_CAP`]; at most
/// [`FAIL_HOOKS_CAP`] entries. Anything else is an error, so a malformed
/// field is refused whole rather than half believed.
pub fn parse_fail_counts(s: &str) -> Result<Vec<(String, u64)>, String> {
    let mut out: Vec<(String, u64)> = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (hook, n) = part
            .rsplit_once(':')
            .ok_or_else(|| "fail is <hook>:<count>[,<hook>:<count>...]".to_string())?;
        let named = !hook.is_empty()
            && hook.chars().count() <= FAIL_HOOK_CAP
            && hook
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !named {
            return Err("fail: a hook's name is letters, digits, . _ -".to_string());
        }
        let n: u64 = n
            .parse()
            .map_err(|_| "fail: a count is a whole number".to_string())?;
        if out.len() >= FAIL_HOOKS_CAP {
            return Err(format!("fail: at most {FAIL_HOOKS_CAP} hooks"));
        }
        match out.iter_mut().find(|(h, _)| h == hook) {
            Some(slot) => slot.1 = slot.1.max(n.min(FAIL_COUNT_CAP)),
            None => out.push((hook.to_string(), n.min(FAIL_COUNT_CAP))),
        }
    }
    if out.is_empty() {
        return Err("fail is <hook>:<count>[,<hook>:<count>...]".to_string());
    }
    Ok(out)
}

/// What the broker keeps of one pane's reported failures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneFailures {
    /// The highest count each hook ever reported, by hook, in name order.
    pub counts: std::collections::BTreeMap<String, u64>,
    pub class: Option<FailClass>,
    /// The last message: scrubbed, capped, for display only.
    pub message: Option<String>,
    pub at_ms: u64,
    /// The highest `thrown` the mod reported: errors of its own hooks.
    pub thrown: u64,
    /// When that number grew, on the loop's clock, and by how much: what
    /// the sliding window is counted over.
    recent: VecDeque<(u64, u64)>,
}

impl PaneFailures {
    /// Every failure the pane reported.
    pub fn total(&self) -> u64 {
        self.counts.values().fold(0u64, |a, n| a.saturating_add(*n))
    }

    /// The errors its own hooks threw within [`FAIL_WINDOW_MS`] of `clock`.
    /// Five accidents spread over a long session are not a broken mod; five
    /// in ten minutes are.
    pub fn thrown_recently(&self, clock: u64) -> u64 {
        self.recent
            .iter()
            .filter(|(at, _)| clock.saturating_sub(*at) <= FAIL_WINDOW_MS)
            .fold(0u64, |a, (_, n)| a.saturating_add(*n))
    }
}

/// The window the errors of a mod's own hooks are counted over, on the
/// loop's clock: ten minutes (`FAIL_WINDOW_MS` in `mod/hooks/health.ts`).
pub const FAIL_WINDOW_MS: u64 = 600_000;

/// What the count of a failed exchange with atrium is keyed with
/// (`ctl.hello`, `ctl.report`), as against a hook's own name.
pub const CTL_SITE: &str = "ctl.";

/// Is a trip worth writing down for later sessions? Only on what atrium saw
/// itself, and only when nothing in the session speaks against it:
///
/// - `observed_roots`: the root of the spawn tree of each seat that missed
///   the hello window. Two seats under one root are one lead's children, and
///   a lead must not be able to keep the mod off the machine: it takes misses
///   under [`TRIP_PANES`] different roots.
/// - `healthy`: some pane atrium launched with the mod said hello before the
///   trip. Then the mod works on this machine, whatever went wrong for the
///   others, and the trip is this session's alone.
///
/// Reports never count here: they are a pane's word.
pub fn worth_remembering(observed_roots: &[usize], healthy: bool) -> bool {
    let roots: HashSet<usize> = observed_roots.iter().copied().collect();
    !healthy && roots.len() >= TRIP_PANES
}

/// The chrome's flag on a pane whose mod was set aside.
pub const NOTE_SET_ASIDE: &str = "mod off";

/// The chrome's flag on a pane that still runs with the mod loaded after the
/// breaker backed it out.
pub const NOTE_BACKED_OUT: &str = "mod backed out";

/// Panes launched with the mod since the run loop last asked: the spawn path
/// has no hold on the [`ModState`], so it leaves the pane's id here and the
/// health phase collects it ([`take_injected`]).
static INJECTED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Did this session launch any pane with the mod? Until it has, there is no
/// mod here to be healthy or not.
static EVER_INJECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Has any pane of this session been launched with the mod?
pub fn ever_injected() -> bool {
    EVER_INJECTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Note that `pane` was launched with the mod in its environment.
pub fn note_injected(pane: usize) {
    EVER_INJECTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut held = INJECTED.lock().unwrap_or_else(|e| e.into_inner());
    // Nobody collecting (a test that spawns panes and runs no loop): bounded.
    if held.len() < 4096 {
        held.push(pane);
    }
}

/// The panes launched with the mod since the last call.
pub fn take_injected() -> Vec<usize> {
    std::mem::take(&mut *INJECTED.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Why a pane's mod was set aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aside {
    /// It never said hello within [`HELLO_WINDOW_MS`] of starting.
    NoHello,
    /// Its own reports counted [`FAIL_SET_ASIDE`] failures or more.
    Failures,
    /// Its own reports named a failure of the plugin interface.
    Interface,
}

impl Aside {
    /// The reason in a few words, for a refusal and for the status.
    pub fn label(self) -> &'static str {
        match self {
            Aside::NoHello => "no hello within its window",
            Aside::Failures => "it reported repeated failures",
            Aside::Interface => "it reported that the plugin interface changed",
        }
    }
}

/// What one live pane looks like to a health tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSeen {
    pub pane: usize,
    /// Its agent is up, by something the pane itself did since its launch.
    pub started: bool,
    /// The pane at the top of its spawn tree (itself, for a pane atrium or
    /// the human opened): two seats under one root are one lead's doing.
    pub root: usize,
}

/// What a health tick found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthEvent {
    /// A pane's mod was set aside; it runs as a pane without one.
    SetAside { pane: usize, why: Aside },
    /// A pane's mod stopped collecting its deliveries; they are typed now.
    InboxLost { pane: usize },
    /// A pane atrium launched with the mod said hello and was granted
    /// something: the mod works here.
    Healthy { pane: usize },
    /// A pane's mod lost one feature to a missing engine method; the
    /// capability behind it, if any, is taken back.
    FeatureLost { pane: usize, feature: &'static str },
    /// The breaker tripped. `running` are the live panes that have the mod:
    /// each is left as it is. `remember` says the trip rests on what atrium
    /// saw itself (two seats missed the hello window) and is worth keeping
    /// for later sessions; a trip made of reports is this session's alone.
    Tripped {
        reason: String,
        running: Vec<usize>,
        remember: bool,
    },
}

/// One pane atrium launched with the mod, and what it has shown since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expect {
    /// When it was seen started, on the loop's clock; `None` until then.
    started_at: Option<u64>,
    /// Did it say hello?
    hello: bool,
}

/// A status as a mod reports it: the four the transcript inference also
/// knows, plus two only the engine can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModStatus {
    /// A model turn is running.
    Working,
    /// A permission prompt is on screen (`tool.check` resolved to `ask`).
    WaitingApproval,
    /// The turn ended with an answer; the prompt is free.
    WaitingPrompt,
    /// Nothing has happened for a while.
    Idle,
    /// The turn ended on an API error or a refusal with no fallback.
    Errored,
    /// The session ended (`/exit`, a signal, a `/clear`'s old session).
    Ended,
}

impl ModStatus {
    /// The wire label, as `report status=<label>` spells it.
    pub fn parse(s: &str) -> Option<ModStatus> {
        match s.trim() {
            "working" => Some(ModStatus::Working),
            "waiting-approval" => Some(ModStatus::WaitingApproval),
            "waiting-prompt" => Some(ModStatus::WaitingPrompt),
            "idle" => Some(ModStatus::Idle),
            "errored" => Some(ModStatus::Errored),
            "ended" => Some(ModStatus::Ended),
            _ => None,
        }
    }

    /// The label `ctl status` and `ctl list` print for a reported status.
    pub fn label(self) -> &'static str {
        match self {
            ModStatus::Working => "working",
            ModStatus::WaitingApproval => "waiting-approval",
            ModStatus::WaitingPrompt => "waiting-prompt",
            ModStatus::Idle => "idle",
            ModStatus::Errored => "errored",
            ModStatus::Ended => "ended",
        }
    }

    /// What the chrome shows for this status, in the vocabulary the border and
    /// bar already speak. `Errored` reads as waiting for a prompt (the turn is
    /// over and the prompt is free; a red border is a later change). `Ended`
    /// overrides nothing: the pane is leaving and the inference is right.
    pub fn as_agsess(self) -> Option<agsess::Status> {
        match self {
            ModStatus::Working => Some(agsess::Status::Working),
            ModStatus::WaitingApproval => Some(agsess::Status::WaitingApproval),
            ModStatus::WaitingPrompt | ModStatus::Errored => Some(agsess::Status::WaitingPrompt),
            ModStatus::Idle => Some(agsess::Status::Idle),
            ModStatus::Ended => None,
        }
    }

    /// Every status, for tables and tests.
    pub const ALL: [ModStatus; 6] = [
        ModStatus::Working,
        ModStatus::WaitingApproval,
        ModStatus::WaitingPrompt,
        ModStatus::Idle,
        ModStatus::Errored,
        ModStatus::Ended,
    ];
}

/// What a mod said in its `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModInfo {
    /// The mod's own version string.
    pub version: String,
    /// The Claude Code version hosting it, when the mod could read one.
    pub engine: Option<String>,
    /// The capabilities the broker accepted (a subset of [`CAPS`]).
    pub caps: Vec<String>,
    /// When the hello arrived (wall-clock ms).
    pub since_ms: u64,
    /// The pane that spawned this one, as the broker knew it at the hello.
    /// Kept so a parent can still read the answer of a pane that has exited
    /// (a `claude -p` subagent answers and leaves in the same instant).
    pub parent: Option<usize>,
}

/// One `report`'s payload, as the request carries it. Every field is optional;
/// a report with none is refused by the parser.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub status: Option<ModStatus>,
    pub reason: Option<String>,
    /// Context-window fill, 0..=100.
    pub context_pct: Option<u8>,
    /// The session's cost so far, in dollars.
    pub cost_usd: Option<f64>,
    /// Turns completed so far.
    pub turns: Option<u64>,
    /// The text of the last completed turn's answer.
    pub answer: Option<String>,
    /// The caption: what the pane is doing right now, in a few words. An
    /// empty string clears it (the turn ended).
    pub doing: Option<String>,
    /// Real paths the pane edited since its last report. Recorded by
    /// [`ModState::touch`], which needs the pane's root; [`ModState::report`]
    /// ignores this field.
    pub touched: Vec<String>,
    /// The reply to a queued question: the ask's id and the text. Recorded by
    /// [`ModState::reply`]; [`ModState::report`] ignores this field.
    pub ask: Option<(u64, String)>,
    /// What the mod counted of its own failures. Recorded by
    /// [`ModState::failures_reported`], which needs the pane's place in the
    /// spawn tree; [`ModState::report`] ignores this field.
    pub failures: Option<FailureReport>,
}

/// What the broker remembers of one pane's reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneReport {
    /// The last status and when it was reported.
    pub status: Option<(ModStatus, u64)>,
    pub reason: Option<String>,
    pub context_pct: Option<u8>,
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    /// The last answer and its sequence number (monotonic across all panes, so
    /// a waiter can tell a new answer from the one it already read).
    pub answer: Option<(String, u64)>,
    /// The caption and when it was reported.
    pub doing: Option<(String, u64)>,
}

/// One file a pane edited: its real path and when it was first and last
/// touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    pub path: String,
    pub first_ms: u64,
    pub last_ms: u64,
}

/// One file two or more live panes edited: the key both touches share (a
/// path relative to each pane's worktree, or an absolute one) and the panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub key: String,
    pub panes: Vec<usize>,
}

/// A question queued for a pane's mod (`ctl ask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub id: u64,
    pub text: String,
    /// The pane that asked; `None` is the human.
    pub from: Option<usize>,
    pub at_ms: u64,
}

/// A pane's reply to an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskReply {
    pub pane: usize,
    pub text: String,
    pub at_ms: u64,
}

/// The key a touched path is remembered under: relative to `root` (the
/// pane's worktree) when it lies inside it, else the path itself in one
/// spelling ([`norm_path`]). Two worktrees of one repo hold the same relative
/// paths, and an edit to `src/x.rs` in each is the merge conflict the radar
/// exists to catch; a shared absolute path outside both collides on its own
/// name. A pane with no worktree keys by the absolute path alone — never by
/// its cwd, under which two unrelated repos' `README.md` would be one file.
pub fn touch_key(path: &str, root: Option<&str>) -> String {
    let path = norm_path(path.trim());
    if let Some(root) = root.map(str::trim).filter(|r| !r.is_empty()) {
        let root = norm_path(root);
        let root = root.trim_end_matches('/');
        if let Some(rest) = path.strip_prefix(root) {
            if let Some(rel) = rest.strip_prefix('/') {
                if !rel.is_empty() {
                    return rel.to_string();
                }
            }
        }
    }
    path
}

/// A path as the filesystem it came from compares it: `\\?\` (and
/// `\\?\UNC\`) stripped, every `\` a `/`, and a path Windows wrote — a drive
/// letter or a `\\host` — lowercased, since that filesystem does not tell
/// `C:\Wt` from `c:/wt`. A unix path keeps its case: `/wt/A` and `/wt/a` are
/// two directories.
pub fn norm_path(p: &str) -> String {
    let s = if let Some(rest) = p.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else {
        p.strip_prefix("\\\\?\\").unwrap_or(p).to_string()
    };
    let windows = windows_spelled(&s);
    let s = s.replace('\\', "/");
    if windows {
        s.to_lowercase()
    } else {
        s
    }
}

/// Did Windows write this path: a drive letter, or a `\\host` / `//host`?
fn windows_spelled(p: &str) -> bool {
    let b = p.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    drive || p.starts_with("\\\\") || p.starts_with("//")
}

/// The broker's memory of every pane's mod, keyed by agent id.
#[derive(Debug, Default)]
pub struct ModState {
    panes: HashMap<usize, (Option<ModInfo>, PaneReport)>,
    answer_seq: u64,
    /// Panes whose context crossed `respawn_at` and were flagged; cleared when
    /// the fill drops back under, so one crossing is one decision.
    respawn_flagged: HashSet<usize>,
    /// What each pane edited, by touch key ([`touch_key`]).
    touched: HashMap<usize, HashMap<String, Touch>>,
    /// Collision keys already announced; cleared when a key stops colliding,
    /// so one collision is one decision.
    collision_flagged: HashSet<String>,
    /// Questions queued per pane, oldest first.
    asks: HashMap<usize, VecDeque<Ask>>,
    /// Every ask id ever issued and the pane it was issued to, so a reply is
    /// accepted only from that pane and `asked` can tell pending from unknown.
    issued: HashMap<u64, usize>,
    replies: HashMap<u64, AskReply>,
    ask_seq: u64,
    /// The panes launched with the mod, and what each has shown since.
    expect: HashMap<usize, Expect>,
    /// The panes whose mod is set aside, and why.
    aside: HashMap<usize, Aside>,
    /// When each pane holding the `inbox` cap last asked for its deliveries.
    inbox_seen: HashMap<usize, u64>,
    /// The panes whose mod stopped collecting: their `inbox` cap is taken back.
    inbox_lost: HashSet<usize>,
    /// What each pane's mod reported of its own failures.
    failures: HashMap<usize, PaneFailures>,
    /// Every failure ever reported, as a counter.
    failures_total: u64,
    /// The features each pane's mod lost to a missing engine method.
    lost_features: HashMap<usize, HashSet<&'static str>>,
    /// The voice of a respawned pane: the id of the first pane in its seat.
    /// A pane that was never respawned is its own voice and has no entry.
    voice_of: HashMap<usize, usize>,
    /// The voices atrium itself saw miss the hello window, each with the
    /// voice of its tree's root (kept after the pane is gone: the breaker
    /// counts what was seen this session).
    observed: HashMap<usize, usize>,
    /// How many panes atrium launched with the mod said hello and were
    /// granted something, and how many of those after the breaker tripped.
    hellos: u32,
    late_hellos: u32,
    /// The voices set aside by their own reports, each with the voices above
    /// it in the spawn tree when it was.
    reporters: HashMap<usize, Vec<usize>>,
    /// Why the breaker tripped; `None` while it has not.
    tripped: Option<String>,
    /// What happened between two health ticks, handed out by the next.
    events: Vec<HealthEvent>,
    last_tick_ms: Option<u64>,
    /// The loop's clock: the sum of the time between health passes, each
    /// counted up to [`TICK_MAX_MS`].
    loop_ms: u64,
}

impl ModState {
    /// Record a `hello`. Idempotent: a reload re-sends it and replaces the
    /// previous info; reports already made are kept. Returns the capabilities
    /// accepted, in [`CAPS`] order, de-duplicated.
    pub fn hello(
        &mut self,
        pane: usize,
        parent: Option<usize>,
        version: &str,
        engine: Option<&str>,
        caps: &[String],
        now_ms: u64,
    ) -> Vec<String> {
        // A pane that missed its window and says hello after all has shown
        // the mod works there: it gets its mod back, and its seat is no
        // longer one atrium saw fail.
        if self.aside.get(&pane) == Some(&Aside::NoHello) && self.expect.contains_key(&pane) {
            self.aside.remove(&pane);
            let voice = self.voice(pane);
            self.observed.remove(&voice);
        }
        // A pane whose mod was set aside for its own failures is a pane
        // without a mod from then on: its hello is recorded and granted
        // nothing.
        let set_aside = self.aside.contains_key(&pane);
        let accepted: Vec<String> = CAPS
            .iter()
            .filter(|_| !set_aside)
            .filter(|c| caps.iter().any(|offered| offered.trim() == **c))
            .map(|c| c.to_string())
            .collect();
        let info = ModInfo {
            version: clip(&text_safe(version), VERSION_CAP),
            engine: engine.map(|e| clip(&text_safe(e), VERSION_CAP)),
            caps: accepted.clone(),
            since_ms: now_ms,
            parent,
        };
        self.panes.entry(pane).or_default().0 = Some(info);
        let expected = match self.expect.get_mut(&pane) {
            Some(e) => {
                e.hello = true;
                true
            }
            None => false,
        };
        // A fresh hello is a fresh mod: its inbox is heard from as of now,
        // and whatever feature the last one lost it has to lose again.
        self.inbox_lost.remove(&pane);
        self.lost_features.remove(&pane);
        if accepted.iter().any(|c| c == "inbox") {
            self.inbox_seen.insert(pane, self.loop_ms);
        } else {
            self.inbox_seen.remove(&pane);
        }
        // Only a pane atrium launched with the mod says anything about the
        // mod atrium installs: any pane can run `atrium ctl hello`.
        if expected && !accepted.is_empty() {
            self.hellos = self.hellos.saturating_add(1);
            if self.tripped.is_some() {
                self.late_hellos = self.late_hellos.saturating_add(1);
            }
            self.raise(HealthEvent::Healthy { pane });
        }
        accepted
    }

    /// The parent a pane's mod was spawned under, as recorded at its hello.
    pub fn parent_of(&self, pane: usize) -> Option<usize> {
        self.info(pane).and_then(|i| i.parent)
    }

    /// Record a `report`. Fields given replace what was held; fields absent
    /// are kept. Text is scrubbed of control characters and capped. Returns
    /// the sequence number of the answer, when the report carried one.
    /// `touched` and `ask` are not recorded here: see [`ModState::touch`] and
    /// [`ModState::reply`], which the server calls with what they need.
    pub fn report(&mut self, pane: usize, r: &Report, now_ms: u64) -> Option<u64> {
        let slot = &mut self.panes.entry(pane).or_default().1;
        if let Some(d) = r.doing.as_deref() {
            let d = clip(text_safe(d).trim(), DOING_CAP);
            slot.doing = (!d.is_empty()).then_some((d, now_ms));
        }
        if let Some(s) = r.status {
            slot.status = Some((s, now_ms));
            // A reason belongs to the status it came with; a status reported
            // without one clears the old reason rather than leaving a stale
            // "api-error" under a fresh "working".
            slot.reason = r.reason.as_deref().map(|x| clip(&text_safe(x), REASON_CAP));
        } else if let Some(x) = r.reason.as_deref() {
            slot.reason = Some(clip(&text_safe(x), REASON_CAP));
        }
        if let Some(p) = r.context_pct {
            slot.context_pct = Some(p.min(100));
        }
        if let Some(c) = r.cost_usd {
            if c.is_finite() && c >= 0.0 {
                slot.cost_usd = Some(c);
            }
        }
        if let Some(t) = r.turns {
            slot.turns = Some(t);
        }
        let mut seq = None;
        if let Some(a) = r.answer.as_deref() {
            self.answer_seq += 1;
            slot.answer = Some((clip(&text_safe(a), ANSWER_CAP), self.answer_seq));
            seq = Some(self.answer_seq);
        }
        seq
    }

    /// The mod's hello, if the pane ever said one.
    pub fn info(&self, pane: usize) -> Option<&ModInfo> {
        self.panes.get(&pane).and_then(|(i, _)| i.as_ref())
    }

    /// Did the pane's mod declare `cap`, did the broker accept it, and does
    /// it still hold: a mod that was set aside holds none, and one that
    /// stopped collecting its deliveries holds neither `inbox` nor `ask`
    /// (an ask rides the inbox). Every reader goes through here, so a pane
    /// whose mod failed is, for all of them, a pane without one.
    pub fn has_cap(&self, pane: usize, cap: &str) -> bool {
        if self.aside.contains_key(&pane) {
            return false;
        }
        if self.inbox_lost.contains(&pane) && matches!(cap, "inbox" | "ask") {
            return false;
        }
        if self
            .lost_features
            .get(&pane)
            .is_some_and(|lost| lost.iter().any(|f| feature_caps(f).contains(&cap)))
        {
            return false;
        }
        self.info(pane)
            .is_some_and(|i| i.caps.iter().any(|c| c == cap))
    }

    /// The pane's reported status, if one was reported within
    /// [`REPORT_STALE_MS`] of `now_ms`.
    pub fn status_for(&self, pane: usize, now_ms: u64) -> Option<ModStatus> {
        // A set-aside pane's status is still read: atrium's own status hooks
        // (`crate::status_hooks`) report through the same verb, with no mod
        // at all, and they are what such a pane's status comes from.
        let (s, at) = self.panes.get(&pane)?.1.status?;
        (now_ms.saturating_sub(at) <= REPORT_STALE_MS).then_some(s)
    }

    /// Everything reported for the pane.
    pub fn report_for(&self, pane: usize) -> Option<&PaneReport> {
        self.panes.get(&pane).map(|(_, r)| r)
    }

    /// Drop everything about a pane: it exited, or was respawned as a new
    /// process whose mod will say hello again. Replies it already gave stay
    /// readable until they expire; every question it never answered — queued,
    /// or taken by a mod that died with it in flight — is dropped, so the
    /// next process under this pane id starts with a clear queue.
    pub fn forget(&mut self, pane: usize) {
        self.panes.remove(&pane);
        self.expect.remove(&pane);
        self.aside.remove(&pane);
        self.inbox_seen.remove(&pane);
        self.inbox_lost.remove(&pane);
        self.lost_features.remove(&pane);
        self.failures.remove(&pane);
        self.respawn_flagged.remove(&pane);
        self.touched.remove(&pane);
        self.asks.remove(&pane);
        let replies = &self.replies;
        self.issued
            .retain(|id, p| *p != pane || replies.contains_key(id));
    }

    // ---- health and the breaker ----

    /// Note that `pane` was launched with the mod: its `hello` is expected
    /// within [`HELLO_WINDOW_MS`] of the moment a tick sees it started.
    pub fn expect_mod(&mut self, pane: usize) {
        self.expect.entry(pane).or_insert(Expect {
            started_at: None,
            hello: false,
        });
    }

    /// Was `pane` launched with the mod (and is it still watched)?
    pub fn expects(&self, pane: usize) -> bool {
        self.expect.contains_key(&pane)
    }

    /// May `pane` report failures of a mod: it was launched with one in this
    /// session, or it holds a hello? Every other pane (a shell, a codex pane,
    /// a claude launched without the mod) has no mod to speak of, and what it
    /// says of one is refused and changes nothing.
    pub fn has_a_mod(&self, pane: usize) -> bool {
        self.expects(pane) || self.info(pane).is_some()
    }

    /// Does `pane` still owe the tick its first sight of being started?
    pub fn awaiting_start(&self, pane: usize) -> bool {
        self.expect
            .get(&pane)
            .is_some_and(|e| e.started_at.is_none())
    }

    /// Why the pane's mod was set aside, if it was.
    pub fn aside(&self, pane: usize) -> Option<Aside> {
        self.aside.get(&pane).copied()
    }

    /// Did the pane's mod stop collecting its deliveries?
    pub fn inbox_lost(&self, pane: usize) -> bool {
        self.inbox_lost.contains(&pane)
    }

    /// The features the pane's mod reported it lost to a missing engine
    /// method, in name order.
    pub fn features_lost(&self, pane: usize) -> Vec<&'static str> {
        let mut lost: Vec<&'static str> = self
            .lost_features
            .get(&pane)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        lost.sort_unstable();
        lost
    }

    /// What the pane's mod reported of its own failures.
    pub fn failures_for(&self, pane: usize) -> Option<&PaneFailures> {
        self.failures.get(&pane)
    }

    /// Every failure any mod reported this session: a counter, which a pane
    /// leaving does not lower.
    pub fn failures_total(&self) -> u64 {
        self.failures_total
    }

    /// Why the breaker tripped, if it did.
    pub fn tripped(&self) -> Option<&str> {
        self.tripped.as_deref()
    }

    /// How many panes atrium launched with the mod said hello after the
    /// breaker tripped: a sign the trip may not be the mod's fault.
    pub fn hellos_since_trip(&self) -> u32 {
        self.late_hellos
    }

    /// The pane's mod asked for its deliveries.
    pub fn note_inbox(&mut self, pane: usize) {
        let at = self.loop_ms;
        if let Some(seen) = self.inbox_seen.get_mut(&pane) {
            *seen = at;
        }
    }

    /// `old` was respawned as `new`: the same seat, a new process. Whatever
    /// the breaker held against the old id it holds against this one voice,
    /// so a pane respawned ten times is one voice, not ten.
    pub fn respawned(&mut self, old: usize, new: usize) {
        let voice = self.voice(old);
        self.voice_of.remove(&old);
        if voice != new {
            self.voice_of.insert(new, voice);
        }
    }

    /// The voice a pane speaks with: its own id, or the id of the first pane
    /// in the seat it was respawned into.
    fn voice(&self, pane: usize) -> usize {
        self.voice_of.get(&pane).copied().unwrap_or(pane)
    }

    /// Queue an event for the next tick; bounded, since a session that runs
    /// no health phase never collects them.
    fn raise(&mut self, event: HealthEvent) {
        if self.events.len() < EVENTS_CAP {
            self.events.push(event);
        }
    }

    /// Set a pane's mod aside: no capability from here on. When its mod had
    /// said hello, whatever status that mod last reported is dropped at once,
    /// so a stale `working` does not sit for [`REPORT_STALE_MS`] and the
    /// unmodded path decides from this tick. A pane whose mod never said
    /// hello keeps what is held: every status it has came from atrium's
    /// status hooks, which are the unmodded path.
    fn set_aside(&mut self, pane: usize, why: Aside) {
        self.aside.insert(pane, why);
        if let Some((Some(_), report)) = self.panes.get_mut(&pane) {
            report.status = None;
            report.reason = None;
            report.doing = None;
        }
    }

    /// Record the failures a pane's mod reported of itself. `ancestors` are
    /// the panes above it in the spawn tree, as the broker knows them (never
    /// anything the request said). A pane with no mod ([`ModState::has_a_mod`])
    /// changes nothing here. The counts are kept for display (a hook's
    /// highest count; at most [`FAIL_HOOKS_CAP`] hooks; the message scrubbed
    /// by [`fail_text`]). Beyond that they can do two things, both to the
    /// pane that sent them and no other:
    ///
    /// - a `feature.<name>` count takes back the capability that feature
    ///   stood behind ([`feature_caps`]): the mod lost one feature to a
    ///   missing engine method, and keeps the rest;
    /// - an `api` failure at a place the mod cannot work without
    ///   ([`CORE_SITES`]), or [`FAIL_SET_ASIDE`] errors of its own hooks
    ///   within [`FAIL_WINDOW_MS`] (`thrown`: class `hook` and `api` only),
    ///   set the whole mod aside. Returns why, the first time.
    pub fn failures_reported(
        &mut self,
        pane: usize,
        ancestors: &[usize],
        r: &FailureReport,
    ) -> Option<Aside> {
        if !self.has_a_mod(pane) {
            return None;
        }
        let at_ms = self.loop_ms;
        let slot = self.failures.entry(pane).or_default();
        let mut features: Vec<&'static str> = Vec::new();
        for (hook, n) in r.counts.iter().take(FAIL_HOOKS_CAP) {
            let hook: String = hook
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .take(FAIL_HOOK_CAP)
                .collect();
            if hook.is_empty() {
                continue;
            }
            if !slot.counts.contains_key(&hook) && slot.counts.len() >= FAIL_HOOKS_CAP {
                continue;
            }
            if let Some(feature) = hook
                .strip_prefix(FEATURE_SITE)
                .and_then(|f| FEATURES.iter().copied().find(|known| *known == f))
            {
                features.push(feature);
            }
            let n = (*n).min(FAIL_COUNT_CAP);
            let held = slot.counts.entry(hook).or_insert(0);
            if n > *held {
                self.failures_total = self.failures_total.saturating_add(n - *held);
                *held = n;
            }
        }
        if let Some(c) = r.class {
            slot.class = Some(c);
        }
        if let Some(m) = r.message.as_deref() {
            slot.message = Some(fail_text(m)).filter(|m| !m.is_empty());
        }
        slot.at_ms = at_ms;
        if let Some(t) = r.thrown.map(|t| t.min(FAIL_COUNT_CAP)) {
            if t > slot.thrown {
                slot.recent.push_back((at_ms, t - slot.thrown));
                slot.thrown = t;
            }
        }
        while slot
            .recent
            .front()
            .is_some_and(|(at, _)| at_ms.saturating_sub(*at) > FAIL_WINDOW_MS)
        {
            slot.recent.pop_front();
        }
        let thrown = slot.thrown_recently(at_ms);
        for feature in features {
            if self.lost_features.entry(pane).or_default().insert(feature) {
                self.raise(HealthEvent::FeatureLost { pane, feature });
            }
        }
        if self.aside.contains_key(&pane) {
            return None;
        }
        let core = r.class.is_some_and(FailClass::is_interface)
            && r.at.as_deref().is_some_and(|at| CORE_SITES.contains(&at));
        let why = if core {
            Aside::Interface
        } else if thrown >= FAIL_SET_ASIDE {
            Aside::Failures
        } else {
            return None;
        };
        self.set_aside(pane, why);
        // A voice is a pane atrium launched with the mod. One that loaded a
        // mod by hand loses it like any other, and is nobody's voice.
        if self.expect.contains_key(&pane) {
            let voice = self.voice(pane);
            let above: Vec<usize> = ancestors.iter().map(|a| self.voice(*a)).collect();
            self.reporters.insert(voice, above);
        }
        self.raise(HealthEvent::SetAside { pane, why });
        Some(why)
    }

    /// Is a health pass due: [`TICK_MIN_MS`] since the last, or something
    /// waiting to be said? The run loop turns every few milliseconds; the
    /// health of a pane does not change that fast.
    pub fn health_due(&self, now_ms: u64) -> bool {
        !self.events.is_empty()
            || self
                .last_tick_ms
                .map_or(true, |last| now_ms.saturating_sub(last) >= TICK_MIN_MS)
    }

    /// One pass of the health check over the live panes: start each expected
    /// pane's hello window when it is first seen started, set aside the ones
    /// whose window ran out, take the `inbox` cap back from a mod that
    /// stopped asking, and trip the breaker when [`TRIP_PANES`] voices say
    /// the mod is broken. Returns what happened since the last pass,
    /// including what a `hello` or a `report` in between caused. A pane that
    /// is gone is forgotten, never counted.
    ///
    /// Every window runs on the **loop's clock**, which moves only while the
    /// loop does: a pass adds the time since the last one, and no more than
    /// [`TICK_MAX_MS`] of it. While atrium is stalled (a long spawn, a
    /// suspended machine, a debugger) no pane could have been answered, so
    /// none of that time is held against one.
    pub fn health_tick(&mut self, now_ms: u64, live: &[PaneSeen]) -> Vec<HealthEvent> {
        if let Some(last) = self.last_tick_ms {
            self.loop_ms += now_ms.saturating_sub(last).min(TICK_MAX_MS);
        }
        self.last_tick_ms = Some(now_ms);
        let clock = self.loop_ms;
        let mut events = std::mem::take(&mut self.events);
        let alive: HashSet<usize> = live.iter().map(|s| s.pane).collect();
        self.expect.retain(|p, _| alive.contains(p));
        self.inbox_seen.retain(|p, _| alive.contains(p));
        let mut overdue: Vec<usize> = Vec::new();
        for seen in live {
            let Some(e) = self.expect.get_mut(&seen.pane) else {
                continue;
            };
            if seen.started && e.started_at.is_none() {
                e.started_at = Some(clock);
            }
            let late = !e.hello
                && e.started_at
                    .is_some_and(|s| clock.saturating_sub(s) >= HELLO_WINDOW_MS);
            if late && !self.aside.contains_key(&seen.pane) {
                overdue.push(seen.pane);
            }
        }
        for pane in overdue {
            self.set_aside(pane, Aside::NoHello);
            let voice = self.voice(pane);
            let root = live
                .iter()
                .find(|s| s.pane == pane)
                .map_or(pane, |s| s.root);
            let root = self.voice(root);
            self.observed.insert(voice, root);
            events.push(HealthEvent::SetAside {
                pane,
                why: Aside::NoHello,
            });
        }
        let mut silent: Vec<usize> = self
            .inbox_seen
            .iter()
            .filter(|(pane, at)| {
                clock.saturating_sub(**at) > INBOX_SILENT_MS && self.has_cap(**pane, "inbox")
            })
            .map(|(pane, _)| *pane)
            .collect();
        silent.sort_unstable();
        for pane in silent {
            self.inbox_lost.insert(pane);
            events.push(HealthEvent::InboxLost { pane });
        }
        if self.tripped.is_none() {
            if let Some(reason) = self.trip_reason() {
                let running: Vec<usize> = live
                    .iter()
                    .map(|s| s.pane)
                    .filter(|p| self.has_a_mod(*p))
                    .collect();
                self.tripped = Some(reason.clone());
                events.push(HealthEvent::Tripped {
                    reason,
                    running,
                    remember: worth_remembering(
                        &self.observed.values().copied().collect::<Vec<_>>(),
                        self.hellos > 0,
                    ),
                });
            }
        }
        events
    }

    /// The voices set aside by their own reports that have no such voice
    /// above them: a pane and the children it spawned speak with one voice.
    fn report_voices(&self) -> usize {
        self.reporters
            .values()
            .filter(|ancestors| !ancestors.iter().any(|a| self.reporters.contains_key(a)))
            .count()
    }

    /// Why the breaker should trip now, in atrium's own words (nothing a pane
    /// wrote is in it), or `None` while fewer than [`TRIP_PANES`] voices
    /// spoke. A voice counts once, however it spoke.
    fn trip_reason(&self) -> Option<String> {
        let seen = self.observed.len();
        let told = self
            .reporters
            .iter()
            .filter(|(voice, ancestors)| {
                !self.observed.contains_key(voice)
                    && !ancestors.iter().any(|a| self.reporters.contains_key(a))
            })
            .count();
        debug_assert!(told <= self.report_voices());
        if seen + told < TRIP_PANES {
            return None;
        }
        let mut parts = Vec::new();
        if seen > 0 {
            parts.push(format!(
                "{seen} claude pane{} loaded it and never said hello",
                if seen == 1 { "" } else { "s" }
            ));
        }
        if told > 0 {
            parts.push(format!(
                "{told} pane{} reported that {} mod kept failing",
                if told == 1 { "" } else { "s" },
                if told == 1 { "its" } else { "their" }
            ));
        }
        Some(parts.join(", "))
    }

    /// What the chrome flags a pane's mod with: [`NOTE_SET_ASIDE`] when it was
    /// set aside, [`NOTE_BACKED_OUT`] when the breaker tripped and this pane
    /// still runs with the mod loaded (it is left alone; a respawn drops it).
    pub fn note_for(&self, pane: usize) -> Option<&'static str> {
        if self.aside.contains_key(&pane) {
            return Some(NOTE_SET_ASIDE);
        }
        (self.tripped.is_some() && self.has_a_mod(pane)).then_some(NOTE_BACKED_OUT)
    }

    /// Record the real paths `pane` edited. `root` is the pane's worktree,
    /// which keys the touch ([`touch_key`]); `None` keys by the path itself. A
    /// path past [`TOUCH_CAP`] distinct keys is dropped.
    pub fn touch(&mut self, pane: usize, paths: &[String], root: Option<&str>, now_ms: u64) {
        let map = self.touched.entry(pane).or_default();
        for raw in paths {
            let path = text_safe(raw.trim());
            if path.is_empty() {
                continue;
            }
            let key = touch_key(&path, root);
            if let Some(t) = map.get_mut(&key) {
                t.last_ms = now_ms;
                t.path = path;
            } else if map.len() < TOUCH_CAP {
                map.insert(
                    key,
                    Touch {
                        path,
                        first_ms: now_ms,
                        last_ms: now_ms,
                    },
                );
            }
        }
    }

    /// What `pane` touched, by key, oldest first touch first.
    pub fn touches_of(&self, pane: usize) -> Vec<(&str, &Touch)> {
        let mut v: Vec<(&str, &Touch)> = self
            .touched
            .get(&pane)
            .map(|m| m.iter().map(|(k, t)| (k.as_str(), t)).collect())
            .unwrap_or_default();
        v.sort_by_key(|(k, t)| (t.first_ms, k.to_string()));
        v
    }

    /// Who touched `path`: every pane whose touch key or real path is `path`
    /// or ends with `/path`, so `ctl who src/x.rs` finds the file in every
    /// worktree — compared in one spelling ([`norm_path`]), so any spelling of
    /// a Windows path finds it. Sorted by pane.
    pub fn who(&self, path: &str) -> Vec<(usize, &str, &Touch)> {
        let q = norm_path(path.trim());
        let q = q.trim_end_matches('/');
        if q.is_empty() {
            return Vec::new();
        }
        // A touch Windows wrote is keyed lowercase; a query against it is too,
        // so `Src\Main.rs` finds `src/main.rs` while `/wt/A` stays `/wt/A`.
        let lower = q.to_lowercase();
        let hit = |s: &str, q: &str| s == q || s.ends_with(&format!("/{q}"));
        let mut v: Vec<(usize, &str, &Touch)> = self
            .touched
            .iter()
            .flat_map(|(pane, m)| {
                m.iter()
                    .filter(|(k, t)| {
                        let q = if windows_spelled(&t.path) {
                            lower.as_str()
                        } else {
                            q
                        };
                        hit(k, q) || hit(&norm_path(&t.path), q)
                    })
                    .map(move |(k, t)| (*pane, k.as_str(), t))
            })
            .collect();
        v.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        v
    }

    /// Every key two or more of the `live` panes touched, sorted by key, the
    /// panes sorted within.
    pub fn collisions(&self, live: &[usize]) -> Vec<Collision> {
        let mut by_key: HashMap<&str, Vec<usize>> = HashMap::new();
        for pane in live {
            if let Some(m) = self.touched.get(pane) {
                for k in m.keys() {
                    let v = by_key.entry(k.as_str()).or_default();
                    if !v.contains(pane) {
                        v.push(*pane);
                    }
                }
            }
        }
        let mut out: Vec<Collision> = by_key
            .into_iter()
            .filter(|(_, panes)| panes.len() >= 2)
            .map(|(key, mut panes)| {
                panes.sort_unstable();
                Collision {
                    key: key.to_string(),
                    panes,
                }
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// The collisions among `live` panes not yet announced: each is returned
    /// once, until it stops being a collision (a pane left) and starts again.
    pub fn collisions_new(&mut self, live: &[usize]) -> Vec<Collision> {
        let all = self.collisions(live);
        let keys: HashSet<&str> = all.iter().map(|c| c.key.as_str()).collect();
        self.collision_flagged.retain(|k| keys.contains(k.as_str()));
        let mut fresh = Vec::new();
        for c in all {
            if self.collision_flagged.insert(c.key.clone()) {
                fresh.push(c);
            }
        }
        fresh
    }

    /// Queue a question for `pane`'s mod. Refused when the pane already holds
    /// [`ASK_QUEUE`] unanswered questions. Returns the ask's id.
    pub fn ask(
        &mut self,
        pane: usize,
        from: Option<usize>,
        text: &str,
        now_ms: u64,
    ) -> Result<u64, String> {
        // An expired reply takes its ask with it: the id stops counting as
        // unanswered, and a late `reply` to it is refused like any unknown id.
        let replies = &mut self.replies;
        let issued = &mut self.issued;
        replies.retain(|id, r| {
            let keep = now_ms.saturating_sub(r.at_ms) <= ASK_REPLY_TTL_MS;
            if !keep {
                issued.remove(id);
            }
            keep
        });
        let text = clip(text_safe(text).trim(), ASK_CAP);
        if text.is_empty() {
            return Err("ask needs a question".to_string());
        }
        let pending = self
            .issued
            .iter()
            .filter(|(id, p)| **p == pane && !self.replies.contains_key(*id))
            .count();
        if pending >= ASK_QUEUE {
            return Err(format!(
                "pane {pane} already has {ASK_QUEUE} unanswered questions"
            ));
        }
        self.ask_seq += 1;
        let id = self.ask_seq;
        self.asks.entry(pane).or_default().push_back(Ask {
            id,
            text,
            from,
            at_ms: now_ms,
        });
        self.issued.insert(id, pane);
        Ok(id)
    }

    /// Hand `pane`'s queued questions to its mod, emptying the queue.
    pub fn take_asks(&mut self, pane: usize) -> Vec<Ask> {
        self.asks
            .get_mut(&pane)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Record `pane`'s reply to ask `id`. Only the pane the question was
    /// issued to may answer it, and only once; anything else is refused.
    pub fn reply(&mut self, pane: usize, id: u64, text: &str, now_ms: u64) -> bool {
        if self.issued.get(&id) != Some(&pane) || self.replies.contains_key(&id) {
            return false;
        }
        self.replies.insert(
            id,
            AskReply {
                pane,
                text: clip(text_safe(text).trim(), ASK_REPLY_CAP),
                at_ms: now_ms,
            },
        );
        true
    }

    /// The reply to ask `id`, once it arrived and until it expires.
    pub fn reply_for(&self, id: u64) -> Option<&AskReply> {
        self.replies.get(&id)
    }

    /// Is ask `id` issued and still unanswered?
    pub fn ask_pending(&self, id: u64) -> bool {
        self.issued.contains_key(&id) && !self.replies.contains_key(&id)
    }

    /// True exactly once when a pane's reported context fill reaches
    /// `threshold` while the pane is at its prompt or idle (a respawn mid-turn
    /// would lose work): the moment to ask its parent to checkpoint and
    /// respawn it. Flagged until the fill drops back under the threshold.
    pub fn respawn_crossing(&mut self, pane: usize, threshold: u8, now_ms: u64) -> bool {
        let Some(pct) = self.report_for(pane).and_then(|r| r.context_pct) else {
            return false;
        };
        if pct < threshold {
            self.respawn_flagged.remove(&pane);
            return false;
        }
        if self.respawn_flagged.contains(&pane) {
            return false;
        }
        let idle = matches!(
            self.status_for(pane, now_ms),
            Some(ModStatus::WaitingPrompt) | Some(ModStatus::Idle)
        );
        if !idle {
            return false;
        }
        self.respawn_flagged.insert(pane);
        true
    }

    /// Lay the fresh reported statuses over a snapshot of the inferred ones, so
    /// every reader of [`crate::vendors::AgentState::status_for`] (the border,
    /// the bar, the overview, delivery gating) sees the report without knowing
    /// it exists. `panes` maps each agent id to its session id; a pane with no
    /// session id has no entry in the snapshot and gets nothing laid over it.
    pub fn overlay(
        &self,
        world: &mut crate::vendors::AgentState,
        panes: &[(usize, Option<String>)],
        now_ms: u64,
    ) {
        world.clear_status_overrides();
        let live: Vec<usize> = panes.iter().map(|(p, _)| *p).collect();
        let collisions = self.collisions(&live);
        let colliding: HashSet<usize> = collisions
            .iter()
            .flat_map(|c| c.panes.iter().copied())
            .collect();
        world.set_collisions(collisions);
        for (pane, session) in panes {
            // The flag on its mod is the pane's, session id or not.
            if let Some(note) = self.note_for(*pane) {
                world.set_mod_note(*pane, note);
            }
            let Some(id) = session.as_deref() else {
                continue;
            };
            let Some(status) = self.status_for(*pane, now_ms) else {
                continue;
            };
            if let Some(s) = status.as_agsess() {
                world.set_status_override(id, s);
            }
            // The facts beside the status: what the chrome and the overview
            // read for the states and figures the inference has no word for.
            let r = self.report_for(*pane);
            world.set_mod_facts(
                id,
                crate::vendors::ModFacts {
                    status,
                    reason: r.and_then(|r| r.reason.clone()),
                    context_pct: r.and_then(|r| r.context_pct),
                    cost_usd: r.and_then(|r| r.cost_usd),
                    doing: r.and_then(|r| r.doing.as_ref().map(|(d, _)| d.clone())),
                    colliding: colliding.contains(pane),
                },
            );
        }
    }
}

/// How a session runs the model's subagents, for the mod's `agent.spawn`
/// hook: `panes` (the default: a visible pane beside the caller, its answer
/// collected), `native` (the engine's own, invisible), or `deny` (refused
/// with a pointer to `atrium_spawn`). A fleet sets it with `subagents`;
/// `subagents_keep` leaves the pane open after its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subagents {
    Panes,
    Native,
    Deny,
}

impl Subagents {
    /// The fleet key's values.
    pub fn parse(s: &str) -> Option<Subagents> {
        match s.trim() {
            "panes" => Some(Subagents::Panes),
            "native" => Some(Subagents::Native),
            "deny" => Some(Subagents::Deny),
            _ => None,
        }
    }

    /// The word `whoami` carries.
    pub fn label(self) -> &'static str {
        match self {
            Subagents::Panes => "panes",
            Subagents::Native => "native",
            Subagents::Deny => "deny",
        }
    }
}

static SESSION_SUBAGENTS: std::sync::OnceLock<(Subagents, bool)> = std::sync::OnceLock::new();
static SESSION_CAPTIONS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Record the fleet's `captions` for the session. First call wins.
pub fn set_session_captions(on: bool) {
    let _ = SESSION_CAPTIONS.set(on);
}

/// May a pane's mod spend a small model call on its caption, over the model's
/// own recent words? Only when a fleet said `captions: true`: that is text
/// leaving the pane for a second model call, and a cost, so the default is
/// off. The tool-derived caption is free and always on.
pub fn session_captions() -> bool {
    SESSION_CAPTIONS.get().copied().unwrap_or(false)
}
static SESSION_RESPAWN_AT: std::sync::OnceLock<Option<u8>> = std::sync::OnceLock::new();

/// Record the fleet's `respawn_at` for the session. First call wins.
pub fn set_session_respawn_at(pct: Option<u8>) {
    let _ = SESSION_RESPAWN_AT.set(pct);
}

/// The context fill at which an idle pane's parent is asked to checkpoint and
/// respawn it; `None` (the default) never asks.
pub fn session_respawn_at() -> Option<u8> {
    SESSION_RESPAWN_AT.get().copied().flatten()
}

/// Record the session's subagent policy (a fleet's `subagents` and
/// `subagents_keep`). First call wins, like the fleet's deny rules.
pub fn set_session_subagents(mode: Subagents, keep: bool) {
    let _ = SESSION_SUBAGENTS.set((mode, keep));
}

/// The tool rules a claude pane is launched with under a subagent policy:
/// `deny` is enforced by Claude Code itself (`--disallowedTools`), so it
/// holds whether or not the pane's mod loaded. Both names this tree knows the
/// subagent tool by are denied: `Agent` (Claude Code 2.1.325's built-in tool
/// table lists it) and `Task` (its older name, which `mod/hooks/caption.ts`
/// still reads). `panes` and `native` add no rule: `panes` is the mod's
/// takeover, a courtesy that needs the mod.
pub fn subagent_deny(policy: &str) -> &'static [&'static str] {
    if policy == Subagents::Deny.label() {
        &["Agent", "Task"]
    } else {
        &[]
    }
}

/// The session's subagent policy as `whoami` reports it: `(mode, keep)`,
/// `panes` and `false` unless a fleet said otherwise.
pub fn session_subagents() -> (String, bool) {
    let (mode, keep) = SESSION_SUBAGENTS
        .get()
        .copied()
        .unwrap_or((Subagents::Panes, false));
    (mode.label().to_string(), keep)
}

/// `s` with every control character and line separator replaced by a space.
/// A report's text is shown in panels and typed nowhere, but an embedded
/// escape or newline would still corrupt a frame or a log line. The list is
/// the one `ctl_server::wake_safe` uses for bus wakes: C0, DEL and C1 controls,
/// the Unicode line and paragraph separators, the bidi embeddings, overrides
/// and isolates, and the zero-width joiners and spaces.
pub fn text_safe(s: &str) -> String {
    s.chars()
        .map(|c| if is_unsafe(c) { ' ' } else { c })
        .collect()
}

fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// A mod's failure message as the broker keeps it: control and bidi characters
/// replaced ([`text_safe`]), every hidden character dropped
/// ([`crate::ctl::is_hidden_char`]), runs of spaces folded, cut to
/// [`FAIL_MSG_CAP`]. For display only.
pub fn fail_text(s: &str) -> String {
    let clean: String = text_safe(s)
        .chars()
        .filter(|c| !crate::ctl::is_hidden_char(*c))
        .collect();
    let folded = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&folded, FAIL_MSG_CAP)
}

/// The first `max` characters of `s`.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_respawn_crossing_fires_once_per_crossing_and_only_when_idle() {
        let mut m = ModState::default();
        let at = |m: &mut ModState, pct: u8, status: ModStatus, t: u64| {
            m.report(
                1,
                &Report {
                    status: Some(status),
                    context_pct: Some(pct),
                    ..Report::default()
                },
                t,
            );
        };
        at(&mut m, 70, ModStatus::WaitingPrompt, 1);
        assert!(!m.respawn_crossing(1, 80, 1), "under the threshold");
        at(&mut m, 85, ModStatus::Working, 2);
        assert!(!m.respawn_crossing(1, 80, 2), "over, but mid-turn");
        at(&mut m, 85, ModStatus::WaitingPrompt, 3);
        assert!(m.respawn_crossing(1, 80, 3), "over and idle: once");
        assert!(!m.respawn_crossing(1, 80, 3), "not again while over");
        at(&mut m, 90, ModStatus::Idle, 4);
        assert!(!m.respawn_crossing(1, 80, 4));
        at(&mut m, 20, ModStatus::WaitingPrompt, 5);
        assert!(
            !m.respawn_crossing(1, 80, 5),
            "dropped back: cleared, not fired"
        );
        at(&mut m, 81, ModStatus::WaitingPrompt, 6);
        assert!(
            m.respawn_crossing(1, 80, 6),
            "a second crossing fires again"
        );
        assert!(
            !m.respawn_crossing(2, 80, 6),
            "a pane that reported nothing"
        );
        m.forget(1);
        at(&mut m, 81, ModStatus::WaitingPrompt, 7);
        assert!(
            m.respawn_crossing(1, 80, 7),
            "forgotten: a fresh process starts clean"
        );
    }

    #[test]
    fn subagent_policies_round_trip_and_default_to_panes() {
        for m in [Subagents::Panes, Subagents::Native, Subagents::Deny] {
            assert_eq!(Subagents::parse(m.label()), Some(m));
        }
        assert_eq!(Subagents::parse("tasks"), None);
        // `deny` is also a tool rule the pane is launched with, so it holds
        // without the mod; `panes` and `native` add none.
        assert_eq!(subagent_deny("deny"), &["Agent", "Task"]);
        assert!(subagent_deny("panes").is_empty());
        assert!(subagent_deny("native").is_empty());
        assert_eq!(
            crate::trust::deny_args(&["Agent".to_string()], &[])
                .last()
                .map(String::as_str),
            Some("Agent"),
            "a bare tool name is a rule as it stands"
        );
        let (mode, keep) = session_subagents();
        assert!(["panes", "native", "deny"].contains(&mode.as_str()));
        let _ = keep;
    }

    #[test]
    fn every_status_label_round_trips() {
        for s in ModStatus::ALL {
            assert_eq!(ModStatus::parse(s.label()), Some(s), "{s:?}");
        }
        assert_eq!(ModStatus::parse("busy"), None);
        assert_eq!(ModStatus::parse(" working "), Some(ModStatus::Working));
    }

    #[test]
    fn hello_accepts_only_known_caps_in_canonical_order() {
        let mut m = ModState::default();
        let got = m.hello(
            3,
            Some(0),
            "0.1.0",
            Some("2.1.288"),
            &caps(&["inbox", "bogus", "status", "status"]),
            1_000,
        );
        assert_eq!(got, caps(&["status", "inbox"]));
        assert!(m.has_cap(3, "inbox"));
        assert!(!m.has_cap(3, "bogus"));
        assert!(!m.has_cap(3, "guard"));
        assert!(
            !m.has_cap(4, "status"),
            "a pane that never said hello has no caps"
        );
        let info = m.info(3).expect("hello recorded");
        assert_eq!(info.version, "0.1.0");
        assert_eq!(info.engine.as_deref(), Some("2.1.288"));
        assert_eq!(info.since_ms, 1_000);
        assert_eq!(m.parent_of(3), Some(0));
        assert_eq!(m.parent_of(4), None);
    }

    #[test]
    fn a_second_hello_replaces_the_info_and_keeps_the_reports() {
        let mut m = ModState::default();
        m.hello(1, None, "0.1.0", None, &caps(&["status"]), 10);
        m.report(
            1,
            &Report {
                context_pct: Some(40),
                ..Report::default()
            },
            20,
        );
        m.hello(1, Some(7), "0.2.0", None, &caps(&["status", "inbox"]), 30);
        assert_eq!(m.info(1).map(|i| i.version.as_str()), Some("0.2.0"));
        assert!(m.has_cap(1, "inbox"));
        assert_eq!(
            m.parent_of(1),
            Some(7),
            "a respawned pane's hello names its parent anew"
        );
        assert_eq!(m.report_for(1).and_then(|r| r.context_pct), Some(40));
    }

    #[test]
    fn a_status_report_counts_until_it_is_stale() {
        let mut m = ModState::default();
        let r = Report {
            status: Some(ModStatus::WaitingApproval),
            ..Report::default()
        };
        m.report(7, &r, 1_000);
        assert_eq!(m.status_for(7, 1_000), Some(ModStatus::WaitingApproval));
        assert_eq!(
            m.status_for(7, 1_000 + REPORT_STALE_MS),
            Some(ModStatus::WaitingApproval)
        );
        assert_eq!(
            m.status_for(7, 1_001 + REPORT_STALE_MS),
            None,
            "stale: back to the inference"
        );
        assert_eq!(m.status_for(8, 1_000), None, "never reported");
    }

    #[test]
    fn a_report_merges_fields_and_a_status_resets_the_reason() {
        let mut m = ModState::default();
        m.report(
            2,
            &Report {
                status: Some(ModStatus::Errored),
                reason: Some("api-error".to_string()),
                cost_usd: Some(0.5),
                ..Report::default()
            },
            1,
        );
        m.report(
            2,
            &Report {
                turns: Some(3),
                ..Report::default()
            },
            2,
        );
        let r = m.report_for(2).expect("reported");
        assert_eq!(r.status, Some((ModStatus::Errored, 1)));
        assert_eq!(r.reason.as_deref(), Some("api-error"));
        assert_eq!(r.cost_usd, Some(0.5));
        assert_eq!(r.turns, Some(3));
        m.report(
            2,
            &Report {
                status: Some(ModStatus::Working),
                ..Report::default()
            },
            3,
        );
        let r = m.report_for(2).expect("reported");
        assert_eq!(r.status, Some((ModStatus::Working, 3)));
        assert_eq!(
            r.reason, None,
            "a fresh status without a reason clears the old one"
        );
    }

    #[test]
    fn numbers_are_clamped_and_nonsense_is_dropped() {
        let mut m = ModState::default();
        m.report(
            1,
            &Report {
                context_pct: Some(250),
                cost_usd: Some(-1.0),
                ..Report::default()
            },
            1,
        );
        let r = m.report_for(1).expect("reported");
        assert_eq!(r.context_pct, Some(100));
        assert_eq!(r.cost_usd, None);
        m.report(
            1,
            &Report {
                cost_usd: Some(f64::NAN),
                ..Report::default()
            },
            2,
        );
        assert_eq!(m.report_for(1).and_then(|r| r.cost_usd), None);
    }

    #[test]
    fn answers_are_scrubbed_capped_and_numbered_across_panes() {
        let mut m = ModState::default();
        let a = m.report(
            1,
            &Report {
                answer: Some("done\r\nreally\x1b[31m".to_string()),
                ..Report::default()
            },
            1,
        );
        let b = m.report(
            2,
            &Report {
                answer: Some("x".repeat(ANSWER_CAP + 100)),
                ..Report::default()
            },
            2,
        );
        assert_eq!(a, Some(1));
        assert_eq!(b, Some(2));
        let (text, seq) = m
            .report_for(1)
            .and_then(|r| r.answer.clone())
            .expect("answer");
        assert_eq!(text, "done  really [31m");
        assert_eq!(seq, 1);
        let (text, _) = m
            .report_for(2)
            .and_then(|r| r.answer.clone())
            .expect("answer");
        assert_eq!(text.chars().count(), ANSWER_CAP);
        assert_eq!(
            m.report(
                1,
                &Report {
                    turns: Some(1),
                    ..Report::default()
                },
                3
            ),
            None,
            "no answer, no seq"
        );
    }

    #[test]
    fn touch_keys_are_relative_inside_the_root_and_absolute_outside() {
        assert_eq!(touch_key("/wt/a/src/x.rs", Some("/wt/a")), "src/x.rs");
        assert_eq!(touch_key("/wt/a/src/x.rs", Some("/wt/a/")), "src/x.rs");
        assert_eq!(
            touch_key("/wt/ab/src/x.rs", Some("/wt/a")),
            "/wt/ab/src/x.rs"
        );
        assert_eq!(touch_key("/etc/hosts", Some("/wt/a")), "/etc/hosts");
        assert_eq!(touch_key("/wt/a", Some("/wt/a")), "/wt/a");
        assert_eq!(touch_key("/x/y.rs", None), "/x/y.rs");
        assert_eq!(
            touch_key("C:\\wt\\a\\src\\x.rs", Some("C:\\wt\\a")),
            "src/x.rs"
        );
    }

    // Windows spells one path many ways — `C:\Wt\a`, `c:/wt/a`, `\\?\C:\wt\a` —
    // and the mod's real path need not match the worktree's spelling. The key
    // compares them as the filesystem does; a path that missed its root by
    // spelling alone used to stay absolute, so two worktrees never collided.
    #[test]
    fn touch_keys_compare_windows_paths_as_the_filesystem_does() {
        assert_eq!(touch_key("C:/wt/a/src/x.rs", Some("C:\\wt\\a")), "src/x.rs");
        assert_eq!(
            touch_key("c:\\WT\\A\\src\\x.rs", Some("C:\\wt\\a")),
            "src/x.rs"
        );
        assert_eq!(
            touch_key("\\\\?\\C:\\wt\\a\\src\\x.rs", Some("C:\\wt\\a")),
            "src/x.rs"
        );
        // Outside the root the key is the path in one spelling, so two panes
        // reporting the same file differently collide.
        assert_eq!(
            touch_key("C:\\Shared\\Notes.md", Some("C:\\wt\\a")),
            touch_key("c:/shared/notes.md", None)
        );
        assert_eq!(touch_key("\\\\?\\C:\\x\\y.rs", None), "c:/x/y.rs");
        // A unix path keeps its case: `/wt/A` and `/wt/a` are two directories.
        assert_eq!(touch_key("/wt/A/src/x.rs", Some("/wt/a")), "/wt/A/src/x.rs");
        // `who` finds a Windows file by any spelling of it; the real path is kept.
        let mut m = ModState::default();
        let real = "C:\\wt\\a\\Src\\Main.rs";
        m.touch(1, &[real.to_string()], Some("C:\\wt\\a"), 1);
        assert_eq!(m.who("src/main.rs").len(), 1);
        assert_eq!(m.who("Src\\Main.rs").len(), 1);
        assert_eq!(m.who("c:/wt/a/src/main.rs").len(), 1);
        assert_eq!(m.who(real)[0].2.path, real);
    }

    #[test]
    fn touches_collide_across_worktrees_and_are_announced_once() {
        let mut m = ModState::default();
        m.touch(1, &["/wt/a/src/x.rs".to_string()], Some("/wt/a"), 10);
        m.touch(2, &["/wt/b/src/y.rs".to_string()], Some("/wt/b"), 11);
        assert!(m.collisions(&[1, 2]).is_empty(), "different files");
        m.touch(
            2,
            &["/wt/b/src/x.rs".to_string(), "".to_string()],
            Some("/wt/b"),
            12,
        );
        let c = m.collisions(&[1, 2]);
        assert_eq!(
            c,
            vec![Collision {
                key: "src/x.rs".to_string(),
                panes: vec![1, 2]
            }]
        );
        assert!(
            m.collisions(&[1]).is_empty(),
            "a dead pane collides with nobody"
        );
        assert_eq!(m.collisions_new(&[1, 2]).len(), 1, "announced");
        assert!(m.collisions_new(&[1, 2]).is_empty(), "once");
        m.touch(1, &["/wt/a/src/x.rs".to_string()], Some("/wt/a"), 13);
        assert!(
            m.collisions_new(&[1, 2]).is_empty(),
            "a repeat touch is the same collision"
        );
        assert!(m.collisions_new(&[1]).is_empty(), "pane 2 gone: cleared");
        assert_eq!(m.collisions_new(&[1, 2]).len(), 1, "back: announced again");
        let t = m.touches_of(1);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, "src/x.rs");
        assert_eq!((t[0].1.first_ms, t[0].1.last_ms), (10, 13));
        let who = m.who("src/x.rs");
        assert_eq!(who.iter().map(|w| w.0).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(
            m.who("x.rs").len(),
            2,
            "a suffix on a path boundary matches"
        );
        assert_eq!(
            m.who("/wt/b/src/x.rs").len(),
            1,
            "the real path finds its pane"
        );
        assert!(m.who("c/x.rs").is_empty(), "not a path boundary");
        assert!(m.who("").is_empty());
        m.forget(2);
        assert!(m.who("src/x.rs").len() == 1);
    }

    #[test]
    fn a_pane_remembers_at_most_touch_cap_files() {
        let mut m = ModState::default();
        let paths: Vec<String> = (0..TOUCH_CAP + 5).map(|i| format!("/r/f{i}")).collect();
        m.touch(1, &paths, Some("/r"), 1);
        assert_eq!(m.touches_of(1).len(), TOUCH_CAP);
        m.touch(1, &["/r/f0".to_string()], Some("/r"), 2);
        assert_eq!(
            m.touches_of(1)[0].1.last_ms,
            2,
            "a known file is still updated"
        );
    }

    #[test]
    fn asks_are_queued_taken_once_answered_only_by_their_pane_and_capped() {
        let mut m = ModState::default();
        let id = m.ask(3, Some(0), "  what now?\n", 100).expect("queued");
        assert_eq!(id, 1);
        assert!(m.ask(3, None, "   ", 100).is_err(), "an empty question");
        assert!(m.ask_pending(1));
        assert!(m.reply_for(1).is_none());
        assert!(
            !m.reply(4, 1, "not mine", 101),
            "another pane cannot answer"
        );
        let taken = m.take_asks(3);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].text, "what now?");
        assert_eq!(taken[0].from, Some(0));
        assert!(m.take_asks(3).is_empty(), "taken once");
        assert!(m.reply(3, 1, " fine\x1b[0m ", 102));
        assert!(!m.reply(3, 1, "again", 103), "answered once");
        assert_eq!(m.reply_for(1).map(|r| r.text.as_str()), Some("fine [0m"));
        assert!(!m.ask_pending(1));
        assert!(!m.ask_pending(99), "never issued");
        for i in 0..ASK_QUEUE {
            m.ask(5, None, &format!("q{i}"), 200)
                .expect("under the cap");
        }
        assert!(m.ask(5, None, "one more", 200).is_err(), "capped");
        m.forget(5);
        assert!(
            m.ask(5, None, "fresh", 201).is_ok(),
            "forgotten: a fresh process starts clean"
        );
        assert!(!m.ask_pending(2), "its old questions are gone");
        // Replies expire.
        assert!(m.reply_for(1).is_some());
        m.ask(6, None, "tick", 102 + ASK_REPLY_TTL_MS + 1)
            .expect("queued");
        assert!(m.reply_for(1).is_none(), "expired on the next ask");
    }

    #[test]
    fn an_expired_reply_frees_its_slot_and_is_not_pending_again() {
        let mut m = ModState::default();
        // Fill the cap and answer every question.
        let ids: Vec<u64> = (0..ASK_QUEUE)
            .map(|i| m.ask(3, None, &format!("q{i}"), 100).expect("queued"))
            .collect();
        for a in m.take_asks(3) {
            assert!(m.reply(3, a.id, "done", 101));
        }
        assert!(
            m.ask(3, None, "more", 102).is_ok(),
            "answered: slots are free"
        );
        // Long after the replies expired, the pane must still take questions:
        // an expired reply is not an unanswered question.
        let later = 101 + ASK_REPLY_TTL_MS + 1;
        for i in 0..ASK_QUEUE - 1 {
            m.ask(3, None, &format!("late{i}"), later)
                .unwrap_or_else(|e| panic!("late{i}: {e}"));
        }
        assert!(!m.ask_pending(ids[0]), "expired: not pending");
        assert!(m.reply_for(ids[0]).is_none(), "expired: no reply");
        assert!(
            !m.reply(3, ids[0], "again", later),
            "expired: cannot be re-answered"
        );
    }

    #[test]
    fn forget_drops_asks_the_mod_took_but_never_answered() {
        let mut m = ModState::default();
        for i in 0..ASK_QUEUE {
            m.ask(3, None, &format!("q{i}"), 100).expect("queued");
        }
        let taken = m.take_asks(3);
        assert_eq!(taken.len(), ASK_QUEUE);
        assert!(m.reply(3, taken[0].id, "one answer", 101));
        // The mod died (or the pane respawned) with the rest in flight.
        m.forget(3);
        assert!(!m.ask_pending(taken[1].id), "in flight: gone with the pane");
        assert!(
            m.reply_for(taken[0].id).is_some(),
            "a reply already given stays readable"
        );
        assert!(
            m.ask(3, None, "fresh", 102).is_ok(),
            "the respawned pane starts with a clear queue"
        );
        assert!(
            !m.reply(3, taken[1].id, "late", 103),
            "a dropped ask cannot be answered"
        );
    }

    #[test]
    fn a_caption_is_kept_trimmed_and_cleared_by_an_empty_one() {
        let mut m = ModState::default();
        m.report(
            1,
            &Report {
                doing: Some("  running the\tfilter tests  ".to_string()),
                ..Report::default()
            },
            5,
        );
        assert_eq!(
            m.report_for(1).and_then(|r| r.doing.clone()),
            Some(("running the filter tests".to_string(), 5))
        );
        m.report(
            1,
            &Report {
                doing: Some("x".repeat(DOING_CAP + 9)),
                ..Report::default()
            },
            6,
        );
        assert_eq!(
            m.report_for(1)
                .and_then(|r| r.doing.as_ref().map(|(d, _)| d.chars().count())),
            Some(DOING_CAP)
        );
        m.report(
            1,
            &Report {
                doing: Some(String::new()),
                ..Report::default()
            },
            7,
        );
        assert_eq!(m.report_for(1).and_then(|r| r.doing.clone()), None);
    }

    #[test]
    fn text_safe_replaces_every_unsafe_character() {
        let s = "a\u{2028}b\u{202E}c\u{200B}d\u{FEFF}e\u{85}f\x7fg";
        assert_eq!(text_safe(s), "a b c d e f g");
        assert_eq!(text_safe("plain ünïcode ok"), "plain ünïcode ok");
    }

    #[test]
    fn forget_drops_the_pane_entirely() {
        let mut m = ModState::default();
        m.hello(5, None, "0.1.0", None, &caps(&["status"]), 1);
        m.report(
            5,
            &Report {
                status: Some(ModStatus::Working),
                ..Report::default()
            },
            1,
        );
        m.forget(5);
        assert!(m.info(5).is_none());
        assert!(m.report_for(5).is_none());
        assert_eq!(m.status_for(5, 1), None);
    }

    #[test]
    fn overlay_lays_fresh_reports_over_the_snapshot_by_session_id() {
        use crate::vendors::AgentState;
        let mut m = ModState::default();
        m.report(
            1,
            &Report {
                status: Some(ModStatus::WaitingApproval),
                ..Report::default()
            },
            1_000,
        );
        m.report(
            2,
            &Report {
                status: Some(ModStatus::Errored),
                ..Report::default()
            },
            1_000,
        );
        m.report(
            3,
            &Report {
                status: Some(ModStatus::Ended),
                ..Report::default()
            },
            1_000,
        );
        m.report(
            4,
            &Report {
                status: Some(ModStatus::Working),
                ..Report::default()
            },
            0,
        );
        let mut world = AgentState::default();
        let panes = vec![
            (1, Some("s1".to_string())),
            (2, Some("s2".to_string())),
            (3, Some("s3".to_string())),
            (4, Some("s4".to_string())),
            (5, None),
        ];
        // At the edge of the window the 1_000 ms reports still count; the
        // one from 0 ms is past it.
        m.overlay(&mut world, &panes, 1_000 + REPORT_STALE_MS);
        assert_eq!(
            world.status_for(Some("s1")),
            Some(agsess::Status::WaitingApproval)
        );
        assert_eq!(
            world.status_for(Some("s2")),
            Some(agsess::Status::WaitingPrompt),
            "errored shows as at-the-prompt"
        );
        assert!(world.errored_for(Some("s2")), "and the facts say errored");
        assert!(!world.errored_for(Some("s1")));
        assert!(world.collisions().is_empty());
        assert!(!world.mod_facts_for(Some("s1")).is_some_and(|f| f.colliding));
        m.touch(1, &["/w/a/f.rs".to_string()], Some("/w/a"), 1_000);
        m.touch(2, &["/w/b/f.rs".to_string()], Some("/w/b"), 1_000);
        m.report(
            1,
            &Report {
                doing: Some("editing f.rs".to_string()),
                ..Report::default()
            },
            1_000,
        );
        m.overlay(&mut world, &panes, 1_000 + REPORT_STALE_MS);
        assert_eq!(world.collisions().len(), 1);
        assert_eq!(world.collisions()[0].panes, vec![1, 2]);
        assert!(world.mod_facts_for(Some("s1")).is_some_and(|f| f.colliding));
        assert!(world.mod_facts_for(Some("s2")).is_some_and(|f| f.colliding));
        assert_eq!(
            world
                .mod_facts_for(Some("s1"))
                .and_then(|f| f.doing.as_deref()),
            Some("editing f.rs")
        );
        assert_eq!(
            world.mod_facts_for(Some("s3")).map(|f| f.status),
            Some(ModStatus::Ended),
            "ended lays no status but its facts are kept"
        );
        assert_eq!(
            world.status_for(Some("s3")),
            None,
            "ended overrides nothing"
        );
        assert_eq!(
            world.status_for(Some("s4")),
            None,
            "stale reports lay nothing"
        );
        assert_eq!(world.status_for(None), None);
        // A second overlay with nothing fresh clears what the first laid.
        m.forget(1);
        m.overlay(&mut world, &panes, 1_000);
        assert_eq!(world.status_for(Some("s1")), None);
        assert_eq!(
            world.status_for(Some("s2")),
            Some(agsess::Status::WaitingPrompt)
        );
    }

    // ---- health and the breaker ----

    fn seen(panes: &[(usize, bool)]) -> Vec<PaneSeen> {
        panes
            .iter()
            .map(|(pane, started)| PaneSeen {
                pane: *pane,
                started: *started,
                root: *pane,
            })
            .collect()
    }

    fn fails(counts: &[(&str, u64)], class: Option<FailClass>, at: Option<&str>) -> FailureReport {
        // As the mod counts it: `thrown` is what its own hooks threw, which
        // here is every count not keyed `ctl.` or `feature.`, unless the
        // report's class says they were timeouts.
        let own: u64 = counts
            .iter()
            .filter(|(h, _)| !h.starts_with("ctl.") && !h.starts_with("feature."))
            .map(|(_, n)| *n)
            .sum();
        let thrown = !matches!(
            class,
            Some(FailClass::Timeout | FailClass::Ctl | FailClass::Shape)
        );
        FailureReport {
            counts: counts.iter().map(|(h, n)| (h.to_string(), *n)).collect(),
            class,
            message: None,
            at: at.map(str::to_string),
            thrown: thrown.then_some(own.min(FAIL_COUNT_CAP)),
        }
    }

    /// Ticks a second apart from `from` to `to`, as a turning loop makes them.
    fn run(m: &mut ModState, from: u64, to: u64, live: &[(usize, bool)]) -> Vec<HealthEvent> {
        let mut all = Vec::new();
        let mut t = from;
        while t <= to {
            all.extend(m.health_tick(t, &seen(live)));
            t += 1_000;
        }
        all
    }

    fn tripped(events: &[HealthEvent]) -> Option<&HealthEvent> {
        events
            .iter()
            .find(|e| matches!(e, HealthEvent::Tripped { .. }))
    }

    /// A pane launched with the mod that said hello with these caps.
    fn modded(m: &mut ModState, pane: usize, list: &[&str]) {
        m.expect_mod(pane);
        m.hello(pane, None, "0.3.0", None, &caps(list), 0);
    }

    #[test]
    fn the_hello_window_opens_when_the_pane_is_seen_started_and_not_before() {
        let mut m = ModState::default();
        m.expect_mod(1);
        // Not started: an hour at a trust dialog is not a failure.
        assert!(run(&mut m, 0, 3_600_000, &[(1, false)]).is_empty());
        // Started: the window runs from here.
        let t0 = 3_601_000;
        assert!(m.health_tick(t0, &seen(&[(1, true)])).is_empty());
        let quiet = run(
            &mut m,
            t0 + 1_000,
            t0 + HELLO_WINDOW_MS - 1_000,
            &[(1, true)],
        );
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(m.aside(1), None);
        let events = m.health_tick(t0 + HELLO_WINDOW_MS, &seen(&[(1, true)]));
        assert_eq!(
            events,
            vec![HealthEvent::SetAside {
                pane: 1,
                why: Aside::NoHello
            }]
        );
        assert_eq!(m.aside(1), Some(Aside::NoHello));
        // Said once.
        assert!(m
            .health_tick(t0 + HELLO_WINDOW_MS + 1_000, &seen(&[(1, true)]))
            .is_empty());
    }

    #[test]
    fn a_hello_inside_the_window_is_healthy_and_a_pane_never_expected_is_never_judged() {
        let mut m = ModState::default();
        m.expect_mod(1);
        m.health_tick(0, &seen(&[(1, true), (2, true)]));
        m.hello(1, None, "0.3.0", None, &caps(&["status"]), 4_000);
        let events = run(&mut m, 1_000, 120_000, &[(1, true), (2, true)]);
        assert_eq!(events, vec![HealthEvent::Healthy { pane: 1 }]);
        assert_eq!(m.aside(1), None);
        // Pane 2 was never launched with the mod (a shell, a codex pane, a
        // shim placeholder): no hello is owed.
        assert_eq!(m.aside(2), None);
        assert!(m.has_cap(1, "status"));
    }

    #[test]
    fn a_hand_made_hello_from_a_pane_not_launched_with_the_mod_is_not_the_mod_working() {
        let mut m = ModState::default();
        // Any pane can run `atrium ctl hello`. It gets its caps, as before,
        // and proves nothing about the mod atrium installs.
        let accepted = m.hello(5, None, "9.9.9", None, &caps(&["status"]), 0);
        assert_eq!(accepted, caps(&["status"]));
        let events = run(&mut m, 0, 5_000, &[(5, true)]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HealthEvent::Healthy { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn a_pane_set_aside_keeps_its_hooks_status_and_a_late_hello_brings_its_mod_back() {
        let mut m = ModState::default();
        m.expect_mod(1);
        // Its status hooks reported before the window ran out.
        m.report(
            1,
            &Report {
                status: Some(ModStatus::WaitingPrompt),
                ..Report::default()
            },
            0,
        );
        run(&mut m, 0, HELLO_WINDOW_MS, &[(1, true)]);
        assert_eq!(m.aside(1), Some(Aside::NoHello));
        // No mod ever spoke for this pane: what is held is the status hooks',
        // which are the unmodded path, and it stays.
        assert_eq!(
            m.status_for(1, HELLO_WINDOW_MS),
            Some(ModStatus::WaitingPrompt)
        );
        // A pane whose mod did speak has that mod's last word dropped at
        // once: no stale `working` for two minutes.
        modded(&mut m, 2, &["status"]);
        m.report(
            2,
            &Report {
                status: Some(ModStatus::Working),
                doing: Some("running the tests".to_string()),
                ..Report::default()
            },
            HELLO_WINDOW_MS,
        );
        m.failures_reported(
            2,
            &[],
            &fails(&[("tool.call", 5)], Some(FailClass::Hook), None),
        );
        assert_eq!(m.aside(2), Some(Aside::Failures));
        assert_eq!(m.status_for(2, HELLO_WINDOW_MS), None);
        assert_eq!(m.report_for(2).and_then(|r| r.doing.clone()), None);
        // Until it says hello after all: then the mod plainly works there. It
        // gets its mod back, and its seat is no longer one atrium saw fail.
        let accepted = m.hello(
            1,
            None,
            "0.3.0",
            None,
            &caps(&["status", "inbox", "answer"]),
            HELLO_WINDOW_MS + 5_000,
        );
        assert_eq!(accepted, caps(&["status", "inbox", "answer"]));
        assert_eq!(m.aside(1), None);
        assert!(m.has_cap(1, "inbox"));
        // Pane 2, set aside for its own failures, is not restored by a hello.
        assert!(m
            .hello(
                2,
                None,
                "0.3.0",
                None,
                &caps(&["status"]),
                HELLO_WINDOW_MS + 5_000
            )
            .is_empty());
        assert!(!m.has_cap(2, "status"));
        // A bare status still counts from here on: atrium's status hooks
        // report one with no mod at all, and a pane without a mod keeps them.
        m.report(
            1,
            &Report {
                status: Some(ModStatus::WaitingPrompt),
                ..Report::default()
            },
            HELLO_WINDOW_MS + 6_000,
        );
        assert_eq!(
            m.status_for(1, HELLO_WINDOW_MS + 6_000),
            Some(ModStatus::WaitingPrompt)
        );
    }

    #[test]
    fn time_atrium_itself_was_stalled_is_held_against_no_pane() {
        let mut m = ModState::default();
        m.expect_mod(1);
        modded(&mut m, 2, &["status", "inbox"]);
        m.health_tick(0, &seen(&[(1, true), (2, true)]));
        m.health_tick(1_000, &seen(&[(1, true), (2, true)]));
        // The loop stalls for twenty seconds, three times, inside the window
        // (a roster spawning, a slow disk): nobody could have been answered.
        let mut t = 1_000;
        for _ in 0..3 {
            t += 20_000;
            let events = m.health_tick(t, &seen(&[(1, true), (2, true)]));
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, HealthEvent::SetAside { .. })),
                "{events:?}"
            );
        }
        assert_eq!(m.aside(1), None, "61 s of wall clock, 7 s of the loop's");
        // An hour asleep is the same.
        m.health_tick(t + 3_600_000, &seen(&[(1, true), (2, true)]));
        assert_eq!(m.aside(1), None);
        assert!(m.has_cap(2, "inbox"));
        // With the loop turning again the window runs out where it should:
        // 1 s + 3 x 2 s + 2 s were counted, 21 s are left.
        let from = t + 3_600_000;
        let events = run(&mut m, from + 1_000, from + 20_000, &[(1, true), (2, true)]);
        assert!(events.is_empty(), "{events:?}");
        let events = run(
            &mut m,
            from + 21_000,
            from + 22_000,
            &[(1, true), (2, true)],
        );
        assert_eq!(
            events,
            vec![HealthEvent::SetAside {
                pane: 1,
                why: Aside::NoHello
            }]
        );
    }

    #[test]
    fn a_health_pass_is_due_four_times_a_second_or_when_something_waits() {
        let mut m = ModState::default();
        assert!(m.health_due(0), "the first pass");
        m.health_tick(1_000, &[]);
        assert!(!m.health_due(1_020), "the loop's next turn, 20 ms on");
        assert!(m.health_due(1_000 + TICK_MIN_MS));
        modded(&mut m, 1, &["status"]);
        assert!(m.health_due(1_021), "a hello is waiting to be said");
    }

    #[test]
    fn a_mod_that_stops_collecting_loses_the_inbox_and_keeps_the_rest() {
        let mut m = ModState::default();
        modded(&mut m, 1, &["status", "inbox", "ask"]);
        let mut t = 0;
        // It asks every minute: alive.
        while t < 600_000 {
            if t % 60_000 == 0 {
                m.note_inbox(1);
            }
            let events = m.health_tick(t, &seen(&[(1, true)]));
            assert!(
                events
                    .iter()
                    .all(|e| matches!(e, HealthEvent::Healthy { .. })),
                "{events:?}"
            );
            t += 1_000;
        }
        assert!(m.has_cap(1, "inbox"));
        // Then it stops.
        let events = run(&mut m, t, t + INBOX_SILENT_MS + 2_000, &[(1, true)]);
        assert_eq!(events, vec![HealthEvent::InboxLost { pane: 1 }]);
        assert!(!m.has_cap(1, "inbox"));
        assert!(!m.has_cap(1, "ask"), "an ask rides the inbox");
        assert!(m.has_cap(1, "status"), "its reports still count");
        assert!(m.inbox_lost(1));
        assert_eq!(m.aside(1), None);
        // A fresh hello (the module reloaded) takes the cap again.
        m.hello(1, None, "0.3.0", None, &caps(&["status", "inbox"]), 0);
        assert!(m.has_cap(1, "inbox"));
    }

    #[test]
    fn two_panes_that_miss_the_window_trip_the_breaker_once() {
        let mut m = ModState::default();
        for p in [1, 2, 3, 4] {
            m.expect_mod(p);
        }
        // 1 and 2 start and never say hello; 3 says hello; 4 has not started
        // (a folder-trust dialog, a login).
        m.health_tick(0, &seen(&[(1, true), (2, false), (3, true), (4, false)]));
        m.hello(3, None, "0.3.0", None, &caps(&["status"]), 500);
        let live = [(1, true), (2, true), (3, true), (4, false), (9, true)];
        let events = run(&mut m, 1_000, HELLO_WINDOW_MS + 2_000, &live);
        let trips: Vec<&HealthEvent> = events
            .iter()
            .filter(|e| matches!(e, HealthEvent::Tripped { .. }))
            .collect();
        assert_eq!(trips.len(), 1, "{events:?}");
        assert_eq!(
            trips[0],
            &HealthEvent::Tripped {
                reason: "2 claude panes loaded it and never said hello".to_string(),
                // Every pane with the mod is left as it is, the one that has
                // not started included; pane 9 never had one.
                running: vec![1, 2, 3, 4],
                // Pane 3 said hello: the mod works on this machine, so this
                // is the session's trouble and is not written down.
                remember: false,
            }
        );
        // The same without any hello, the two seats being roots: remembered.
        let mut cold = ModState::default();
        cold.expect_mod(1);
        cold.expect_mod(2);
        let events = run(
            &mut cold,
            0,
            HELLO_WINDOW_MS + 1_000,
            &[(1, true), (2, true)],
        );
        assert!(
            matches!(
                tripped(&events),
                Some(HealthEvent::Tripped { remember: true, .. })
            ),
            "{events:?}"
        );
        assert_eq!(
            m.tripped(),
            Some("2 claude panes loaded it and never said hello")
        );
        // One pane alone does not trip it.
        let mut one = ModState::default();
        one.expect_mod(1);
        let events = run(&mut one, 0, HELLO_WINDOW_MS * 4, &[(1, true)]);
        assert!(tripped(&events).is_none());
        assert_eq!(one.tripped(), None);
    }

    #[test]
    fn a_pane_that_left_before_its_window_ran_out_is_not_a_failure() {
        let mut m = ModState::default();
        m.expect_mod(1);
        m.expect_mod(2);
        m.health_tick(0, &seen(&[(1, true), (2, true)]));
        // Both are killed ten seconds in.
        let events = run(&mut m, 1_000, HELLO_WINDOW_MS * 3, &[]);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(m.tripped(), None);
        assert!(!m.expects(1));
    }

    #[test]
    fn a_pane_with_no_mod_cannot_report_a_mods_failures() {
        let mut m = ModState::default();
        // Panes 1 and 2 were never launched with the mod and never said
        // hello: a shell, a codex pane, a claude started without it.
        for pane in [1, 2] {
            assert!(!m.has_a_mod(pane));
            assert_eq!(
                m.failures_reported(
                    pane,
                    &[],
                    &fails(&[("tool.call", 99)], Some(FailClass::Api), Some("hello"))
                ),
                None
            );
            assert_eq!(m.failures_for(pane), None);
            assert_eq!(m.aside(pane), None);
        }
        assert_eq!(m.failures_total(), 0);
        let events = run(&mut m, 0, 5_000, &[(1, true), (2, true)]);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(m.tripped(), None);
    }

    #[test]
    fn one_panes_reports_set_aside_its_own_mod_and_nobody_elses() {
        let mut m = ModState::default();
        modded(&mut m, 1, &["status", "inbox"]);
        modded(&mut m, 2, &["status", "inbox"]);
        m.health_tick(0, &seen(&[(1, true), (2, true)]));
        // Four failures: counted, shown, nothing else.
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(&[("tool.call", 4)], Some(FailClass::Hook), None)
            ),
            None
        );
        assert!(m.has_cap(1, "status"));
        // The fifth sets pane 1's mod aside.
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(&[("tool.call", 5)], Some(FailClass::Hook), None)
            ),
            Some(Aside::Failures)
        );
        assert!(!m.has_cap(1, "inbox"));
        assert!(m.has_cap(2, "inbox"), "pane 2 is untouched");
        // However loudly pane 1 goes on, the breaker stays shut: one voice.
        for _ in 0..50 {
            m.failures_reported(
                1,
                &[],
                &fails(&[("x", 1_000_000)], Some(FailClass::Api), Some("hello")),
            );
        }
        m.note_inbox(2);
        let events = run(&mut m, 1_000, 100_000, &[(1, true), (2, true)]);
        assert!(tripped(&events).is_none(), "{events:?}");
        assert_eq!(m.tripped(), None);
        assert_eq!(m.failures_for(1).map(PaneFailures::total), Some(1_000_005));
    }

    #[test]
    fn failed_exchanges_with_atrium_are_counted_and_never_set_a_mod_aside() {
        let mut m = ModState::default();
        modded(&mut m, 1, &["status", "inbox"]);
        // A broker that was slow a hundred times: shown, counted, no more.
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(
                    &[("ctl.report", 60), ("ctl.view", 40)],
                    Some(FailClass::Timeout),
                    Some("ctl.report")
                ),
            ),
            None
        );
        assert_eq!(m.aside(1), None);
        assert!(m.has_cap(1, "inbox"));
        let f = m.failures_for(1).expect("kept");
        assert_eq!((f.total(), f.thrown), (100, 0));
        assert_eq!(m.failures_total(), 100);
        // Four of its own hooks' on top: still under the line. The fifth is not.
        m.failures_reported(
            1,
            &[],
            &fails(&[("tool.call", 4)], Some(FailClass::Hook), None),
        );
        assert_eq!(m.aside(1), None);
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(
                    &[("tool.call", 4), ("guard", 1)],
                    Some(FailClass::Hook),
                    None
                )
            ),
            Some(Aside::Failures)
        );
    }

    #[test]
    fn only_what_its_hooks_threw_counts_and_only_within_ten_minutes() {
        // Timeouts of its own hooks (a guard on a slow disk): counted, shown,
        // and never toward setting the mod aside, however many.
        let mut m = ModState::default();
        modded(&mut m, 1, &["status", "inbox"]);
        m.health_tick(0, &seen(&[(1, true)]));
        for n in 1..=50u64 {
            assert_eq!(
                m.failures_reported(
                    1,
                    &[],
                    &fails(&[("guard", n)], Some(FailClass::Timeout), None)
                ),
                None
            );
        }
        assert_eq!(m.aside(1), None);
        assert_eq!(
            m.failures_for(1).map(|f| (f.total(), f.thrown)),
            Some((50, 0))
        );

        // Five accidents spread over ten hours do nothing: each has left the
        // window before the next arrives.
        let mut m = ModState::default();
        modded(&mut m, 1, &["status", "inbox"]);
        let mut t = 0;
        m.health_tick(t, &seen(&[(1, true)]));
        for n in 1..=5u64 {
            // Two hours of a turning loop, the mod asking for its inbox.
            for _ in 0..7_200 {
                t += 1_000;
                m.note_inbox(1);
                m.health_tick(t, &seen(&[(1, true)]));
            }
            assert_eq!(
                m.failures_reported(
                    1,
                    &[],
                    &fails(&[("tool.call", n)], Some(FailClass::Hook), None)
                ),
                None,
                "accident {n}"
            );
        }
        assert_eq!(m.aside(1), None);
        assert_eq!(m.failures_for(1).map(|f| f.thrown), Some(5));
        assert_eq!(
            m.failures_for(1).map(|f| f.thrown_recently(m.loop_ms)),
            Some(1)
        );
        // Four more within the next minutes: five in ten minutes. Set aside.
        for n in 6..=8u64 {
            t += 60_000 / 30;
            m.health_tick(t, &seen(&[(1, true)]));
            assert_eq!(
                m.failures_reported(
                    1,
                    &[],
                    &fails(&[("tool.call", n)], Some(FailClass::Hook), None)
                ),
                None
            );
        }
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(&[("tool.call", 9)], Some(FailClass::Hook), None)
            ),
            Some(Aside::Failures)
        );
        // A report that names no `thrown` (an older mod) sets nothing aside.
        let mut m = ModState::default();
        modded(&mut m, 1, &["status"]);
        let mut old = fails(&[("tool.call", 500)], Some(FailClass::Hook), None);
        old.thrown = None;
        assert_eq!(m.failures_reported(1, &[], &old), None);
    }

    #[test]
    fn a_mod_loaded_by_hand_loses_its_own_mod_and_is_nobodys_voice() {
        let mut m = ModState::default();
        // Two claude panes in a session that injects no mod said hello by
        // hand (`--plugin-dir`): never expected, so never voices.
        for p in [1, 2] {
            m.hello(p, None, "0.3.0", None, &caps(&["status"]), 0);
            assert_eq!(
                m.failures_reported(p, &[], &fails(&[("x", 9)], Some(FailClass::Hook), None)),
                Some(Aside::Failures)
            );
        }
        let events = run(&mut m, 0, 5_000, &[(1, true), (2, true)]);
        assert!(tripped(&events).is_none(), "{events:?}");
        assert_eq!(m.tripped(), None);
    }

    #[test]
    fn a_late_hello_withdraws_the_seat_from_what_atrium_saw_fail() {
        let mut m = ModState::default();
        m.expect_mod(1);
        m.expect_mod(2);
        // Pane 1 misses its window, then says hello after all.
        run(&mut m, 0, HELLO_WINDOW_MS, &[(1, true), (2, false)]);
        assert_eq!(m.aside(1), Some(Aside::NoHello));
        let accepted = m.hello(1, None, "0.3.0", None, &caps(&["status", "inbox"]), 0);
        assert_eq!(accepted, caps(&["status", "inbox"]));
        assert_eq!(m.aside(1), None);
        // Pane 2 then misses its own: one seat, not two. No trip.
        let events = run(
            &mut m,
            HELLO_WINDOW_MS + 1_000,
            HELLO_WINDOW_MS * 2 + 3_000,
            &[(1, true), (2, true)],
        );
        assert_eq!(m.aside(2), Some(Aside::NoHello));
        assert!(tripped(&events).is_none(), "{events:?}");
        // After a trip a hello from a pane launched before it is counted,
        // so the session can say the mod may be fine; it does not un-trip.
        m.expect_mod(3);
        let from = HELLO_WINDOW_MS * 2 + 4_000;
        let events = run(
            &mut m,
            from,
            from + HELLO_WINDOW_MS + 2_000,
            &[(1, true), (2, true), (3, true)],
        );
        assert!(tripped(&events).is_some(), "{events:?}");
        assert_eq!(m.hellos_since_trip(), 0);
        m.hello(3, None, "0.3.0", None, &caps(&["status"]), 0);
        assert_eq!(m.hellos_since_trip(), 1);
        assert!(m.tripped().is_some());
    }

    #[test]
    fn a_trip_is_worth_remembering_only_on_misses_under_two_roots_with_no_hello_at_all() {
        for (roots, healthy, want, why) in [
            (
                &[1usize, 2][..],
                false,
                true,
                "two roots missed, nobody said hello",
            ),
            (
                &[1, 2],
                true,
                false,
                "the mod works for someone on this machine",
            ),
            (&[1, 1], false, false, "one lead's two children"),
            (&[1, 1, 1, 1], false, false, "one lead's four children"),
            (&[1, 1, 2], false, true, "a second root as well"),
            (&[1], false, false, "one seat"),
            (&[], false, false, "reports alone"),
            (&[], true, false, "nothing seen"),
        ] {
            assert_eq!(worth_remembering(roots, healthy), want, "{why}");
        }
        // Through the tick: a lead's two children miss the window.
        let mut m = ModState::default();
        m.expect_mod(11);
        m.expect_mod(12);
        let live = vec![
            PaneSeen {
                pane: 10,
                started: true,
                root: 10,
            },
            PaneSeen {
                pane: 11,
                started: true,
                root: 10,
            },
            PaneSeen {
                pane: 12,
                started: true,
                root: 10,
            },
        ];
        let mut events = Vec::new();
        let mut t = 0;
        while t <= HELLO_WINDOW_MS + 1_000 {
            events.extend(m.health_tick(t, &live));
            t += 1_000;
        }
        // The session trips (atrium saw both), and nothing is written down.
        assert_eq!(
            tripped(&events),
            Some(&HealthEvent::Tripped {
                reason: "2 claude panes loaded it and never said hello".to_string(),
                running: vec![11, 12],
                remember: false,
            })
        );
    }

    #[test]
    fn a_lost_feature_costs_its_capability_and_not_the_mod() {
        let mut m = ModState::default();
        modded(
            &mut m,
            1,
            &["status", "inbox", "ask", "tools", "guard", "answer"],
        );
        m.health_tick(0, &seen(&[(1, true)]));
        // The engine has no `$.prompt.submit`: the inbox is gone, with the
        // ask that rides it, and everything else stays.
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(
                    &[("feature.inbox", 1)],
                    Some(FailClass::Api),
                    Some("feature.inbox")
                )
            ),
            None
        );
        assert_eq!(m.aside(1), None);
        assert!(!m.has_cap(1, "inbox") && !m.has_cap(1, "ask"));
        assert!(m.has_cap(1, "status") && m.has_cap(1, "tools") && m.has_cap(1, "answer"));
        assert_eq!(m.features_lost(1), vec!["inbox"]);
        let events = m.health_tick(1_000, &seen(&[(1, true)]));
        assert!(
            events.contains(&HealthEvent::FeatureLost {
                pane: 1,
                feature: "inbox"
            }),
            "{events:?}"
        );
        // A feature with no capability behind it is recorded, and costs none.
        m.failures_reported(
            1,
            &[],
            &fails(
                &[
                    ("feature.inbox", 1),
                    ("feature.captions", 1),
                    ("feature.nonsense", 1),
                ],
                Some(FailClass::Api),
                Some("feature.captions"),
            ),
        );
        assert_eq!(m.features_lost(1), vec!["captions", "inbox"]);
        assert!(m.has_cap(1, "tools") && m.has_cap(1, "guard"));
        assert_eq!(m.aside(1), None, "three api failures, no set-aside");
        // And it is no voice: two panes that each lost a feature trip nothing.
        modded(&mut m, 2, &["status", "tools"]);
        m.failures_reported(
            2,
            &[],
            &fails(
                &[("feature.tools", 1)],
                Some(FailClass::Api),
                Some("feature.tools"),
            ),
        );
        assert!(!m.has_cap(2, "tools"));
        let events = run(&mut m, 2_000, 6_000, &[(1, true), (2, true)]);
        assert!(tripped(&events).is_none(), "{events:?}");
        // A fresh hello (the module reloaded) starts over.
        m.hello(1, None, "0.3.0", None, &caps(&["status", "inbox"]), 0);
        assert!(m.has_cap(1, "inbox"));
        assert!(m.features_lost(1).is_empty());
    }

    #[test]
    fn an_api_failure_where_the_mod_cannot_work_sets_it_aside_and_two_voices_trip_for_the_session()
    {
        let mut m = ModState::default();
        modded(&mut m, 1, &["status", "inbox"]);
        modded(&mut m, 2, &["status", "inbox"]);
        m.health_tick(0, &seen(&[(1, true), (2, true)]));
        // An api failure in a hook's own body names no core site: counted.
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(&[("tool.call", 1)], Some(FailClass::Api), Some("tool.call"))
            ),
            None
        );
        for site in CORE_SITES {
            let mut each = ModState::default();
            modded(&mut each, 1, &["status"]);
            assert_eq!(
                each.failures_reported(
                    1,
                    &[],
                    &fails(&[(site, 1)], Some(FailClass::Api), Some(site))
                ),
                Some(Aside::Interface),
                "{site}"
            );
        }
        assert_eq!(
            m.failures_reported(
                1,
                &[],
                &fails(&[("report", 1)], Some(FailClass::Api), Some("report"))
            ),
            Some(Aside::Interface)
        );
        let events = m.health_tick(1_000, &seen(&[(1, true), (2, true)]));
        assert_eq!(
            events,
            vec![HealthEvent::SetAside {
                pane: 1,
                why: Aside::Interface
            }]
        );
        assert_eq!(m.tripped(), None);
        m.failures_reported(
            2,
            &[],
            &fails(&[("report", 1)], Some(FailClass::Api), Some("report")),
        );
        let events = m.health_tick(2_000, &seen(&[(1, true), (2, true)]));
        assert_eq!(
            tripped(&events),
            Some(&HealthEvent::Tripped {
                reason: "2 panes reported that their mod kept failing".to_string(),
                running: vec![1, 2],
                // A pane's word about itself: enough for this session, never
                // for the next.
                remember: false,
            }),
            "{events:?}"
        );
    }

    #[test]
    fn a_pane_and_the_children_it_spawned_are_one_voice() {
        let mut m = ModState::default();
        for p in [1, 2, 3, 7] {
            modded(&mut m, p, &["status"]);
        }
        let lie = fails(&[("x", 9)], Some(FailClass::Hook), None);
        // Pane 1 lies, spawns 2 and 3, and has them lie too.
        m.failures_reported(1, &[], &lie);
        m.failures_reported(2, &[1], &lie);
        m.failures_reported(3, &[2, 1], &lie);
        let live = [(1, true), (2, true), (3, true), (7, true)];
        let events = run(&mut m, 0, 5_000, &live);
        assert!(tripped(&events).is_none(), "{events:?}");
        // Each lost its own mod, all the same.
        assert!(m.aside(1).is_some() && m.aside(2).is_some() && m.aside(3).is_some());
        // A pane outside that subtree is a second voice.
        m.failures_reported(7, &[], &lie);
        let events = m.health_tick(6_000, &seen(&live));
        assert!(tripped(&events).is_some(), "{events:?}");
    }

    #[test]
    fn a_respawned_pane_is_the_voice_it_was() {
        // The innocent case: a human respawns a pane whose mod did not load,
        // and it does not load the second time either. One voice.
        let mut m = ModState::default();
        m.expect_mod(1);
        let mut t = HELLO_WINDOW_MS;
        run(&mut m, 0, t, &[(1, true)]);
        assert!(m.aside(1).is_some());
        let mut id = 1;
        for next in [11, 12, 13] {
            // What `ctl respawn` does: forget the old process, seat the new.
            m.forget(id);
            m.respawned(id, next);
            m.expect_mod(next);
            let events = run(
                &mut m,
                t + 1_000,
                t + HELLO_WINDOW_MS + 2_000,
                &[(next, true)],
            );
            assert!(m.aside(next).is_some(), "{events:?}");
            assert!(tripped(&events).is_none(), "{events:?}");
            t += HELLO_WINDOW_MS + 2_000;
            id = next;
        }
        assert_eq!(m.tripped(), None);
        // A second pane that misses it is the second voice.
        m.expect_mod(2);
        let events = run(
            &mut m,
            t + 1_000,
            t + HELLO_WINDOW_MS + 2_000,
            &[(id, true), (2, true)],
        );
        assert!(tripped(&events).is_some(), "{events:?}");

        // The hostile case: a lead respawns one child whose mod reports
        // failures, again and again. Still the child's one voice.
        let mut m = ModState::default();
        modded(&mut m, 5, &["status"]);
        let lie = fails(&[("x", 9)], Some(FailClass::Hook), None);
        let mut child = 20;
        modded(&mut m, child, &["status"]);
        m.failures_reported(child, &[5], &lie);
        for next in 21..30 {
            m.forget(child);
            m.respawned(child, next);
            modded(&mut m, next, &["status"]);
            assert_eq!(m.failures_reported(next, &[5], &lie), Some(Aside::Failures));
            child = next;
        }
        let events = run(&mut m, 0, 5_000, &[(5, true), (child, true)]);
        assert!(tripped(&events).is_none(), "{events:?}");
        assert_eq!(m.tripped(), None);
    }

    #[test]
    fn one_missed_window_and_one_reporting_pane_trip_the_session_only() {
        let mut m = ModState::default();
        m.expect_mod(1);
        modded(&mut m, 2, &["status"]);
        run(&mut m, 0, HELLO_WINDOW_MS, &[(1, true), (2, true)]);
        assert_eq!(m.tripped(), None);
        m.failures_reported(
            2,
            &[],
            &fails(&[("tool.call", 5)], Some(FailClass::Hook), None),
        );
        let events = m.health_tick(HELLO_WINDOW_MS + 1_000, &seen(&[(1, true), (2, true)]));
        assert_eq!(
            tripped(&events),
            Some(&HealthEvent::Tripped {
                reason: "1 claude pane loaded it and never said hello, \
                         1 pane reported that its mod kept failing"
                    .to_string(),
                running: vec![1, 2],
                remember: false,
            }),
            "{events:?}"
        );
        // A pane that both missed the window and reports is one voice.
        let mut m = ModState::default();
        m.expect_mod(1);
        run(&mut m, 0, HELLO_WINDOW_MS, &[(1, true)]);
        m.hello(1, None, "0.3.0", None, &caps(&["status"]), 0);
        m.failures_reported(
            1,
            &[],
            &fails(&[("tool.call", 50)], Some(FailClass::Hook), None),
        );
        let events = run(
            &mut m,
            HELLO_WINDOW_MS + 1_000,
            HELLO_WINDOW_MS + 4_000,
            &[(1, true)],
        );
        assert!(tripped(&events).is_none(), "{events:?}");
    }

    #[test]
    fn reported_failures_are_bounded_scrubbed_and_only_ever_grow() {
        let mut m = ModState::default();
        modded(&mut m, 1, &["status"]);
        let many: Vec<(String, u64)> = (0..100).map(|i| (format!("ctl.h{i}"), 1)).collect();
        m.failures_reported(
            1,
            &[],
            &FailureReport {
                counts: many,
                class: Some(FailClass::Shape),
                message: Some(format!(
                    "a\u{1b}[31mred\u{202e}\u{200b} {}",
                    "x".repeat(500)
                )),
                at: None,
                thrown: None,
            },
        );
        let f = m.failures_for(1).expect("kept");
        assert_eq!(f.counts.len(), FAIL_HOOKS_CAP);
        let msg = f.message.as_deref().expect("kept");
        assert!(msg.chars().count() <= FAIL_MSG_CAP);
        assert!(!msg.contains('\u{1b}') && !msg.contains('\u{202e}') && !msg.contains('\u{200b}'));
        // A lower count later (a reloaded mod starts over) lowers nothing.
        let mut m = ModState::default();
        modded(&mut m, 1, &["status"]);
        m.failures_reported(1, &[], &fails(&[("ctl.a", 3)], None, None));
        m.failures_reported(1, &[], &fails(&[("ctl.a", 1)], None, None));
        m.failures_reported(1, &[], &fails(&[("ctl.a", u64::MAX)], None, None));
        assert_eq!(
            m.failures_for(1).map(PaneFailures::total),
            Some(FAIL_COUNT_CAP)
        );
        assert_eq!(m.failures_total(), FAIL_COUNT_CAP);
        // The counter outlives the pane.
        m.forget(1);
        assert_eq!(m.failures_for(1), None);
        assert_eq!(m.failures_total(), FAIL_COUNT_CAP);
    }

    #[test]
    fn fail_counts_parse_strictly() {
        assert_eq!(
            parse_fail_counts("tool.call:2, whoami:1"),
            Ok(vec![
                ("tool.call".to_string(), 2),
                ("whoami".to_string(), 1)
            ])
        );
        assert_eq!(
            parse_fail_counts("a:1,a:7,a:3"),
            Ok(vec![("a".to_string(), 7)])
        );
        assert_eq!(
            parse_fail_counts("a:99999999999"),
            Ok(vec![("a".to_string(), FAIL_COUNT_CAP)])
        );
        for bad in [
            "",
            "a",
            "a:",
            ":1",
            "a:-1",
            "a:x",
            "a b:1",
            "a\u{1b}:1",
            "a:1,b",
        ] {
            assert!(parse_fail_counts(bad).is_err(), "{bad:?}");
        }
        let long = format!("{}:1", "h".repeat(FAIL_HOOK_CAP + 1));
        assert!(parse_fail_counts(&long).is_err());
        let many: Vec<String> = (0..=FAIL_HOOKS_CAP).map(|i| format!("h{i}:1")).collect();
        assert!(parse_fail_counts(&many.join(",")).is_err());
        for c in [
            FailClass::Api,
            FailClass::Timeout,
            FailClass::Ctl,
            FailClass::Shape,
            FailClass::Hook,
        ] {
            assert_eq!(FailClass::parse(c.label()), Some(c));
        }
        assert_eq!(FailClass::parse("nonsense"), None);
        assert_eq!(
            parse_fail_site("feature.inbox"),
            Ok("feature.inbox".to_string())
        );
        for bad in ["", "a b", "a:b", "a\u{202e}"] {
            assert!(parse_fail_site(bad).is_err(), "{bad:?}");
        }
        assert!(parse_fail_site(&"h".repeat(FAIL_HOOK_CAP + 1)).is_err());
    }

    #[test]
    fn the_chrome_flags_a_set_aside_pane_and_after_a_trip_every_pane_with_the_mod() {
        let mut m = ModState::default();
        m.expect_mod(1);
        modded(&mut m, 2, &["status"]);
        assert_eq!(m.note_for(1), None);
        run(
            &mut m,
            0,
            HELLO_WINDOW_MS,
            &[(1, true), (2, true), (3, true)],
        );
        assert_eq!(m.note_for(1), Some(NOTE_SET_ASIDE));
        assert_eq!(m.note_for(2), None);
        m.expect_mod(4);
        run(
            &mut m,
            HELLO_WINDOW_MS + 1_000,
            HELLO_WINDOW_MS * 2 + 2_000,
            &[(1, true), (2, true), (3, true), (4, true)],
        );
        assert!(m.tripped().is_some());
        assert_eq!(m.note_for(2), Some(NOTE_BACKED_OUT), "still runs with it");
        assert_eq!(m.note_for(3), None, "never had a mod");
        // The overlay carries it by pane, session id or not.
        let mut world = crate::vendors::AgentState::default();
        m.overlay(
            &mut world,
            &[(1, None), (2, Some("s2".to_string())), (3, None)],
            HELLO_WINDOW_MS * 2 + 2_000,
        );
        assert_eq!(world.mod_note_for(1), Some(NOTE_SET_ASIDE));
        assert_eq!(world.mod_note_for(2), Some(NOTE_BACKED_OUT));
        assert_eq!(world.mod_note_for(3), None);
    }

    #[test]
    fn forget_clears_a_panes_health_so_its_relaunch_starts_clean() {
        let mut m = ModState::default();
        m.expect_mod(1);
        run(&mut m, 0, HELLO_WINDOW_MS, &[(1, true)]);
        assert!(m.aside(1).is_some());
        m.forget(1);
        assert_eq!(m.aside(1), None);
        assert!(!m.expects(1));
        // What atrium saw still counts toward the breaker.
        m.expect_mod(2);
        run(
            &mut m,
            HELLO_WINDOW_MS + 1_000,
            HELLO_WINDOW_MS * 3,
            &[(2, true)],
        );
        assert!(m.tripped().is_some());
    }
}
