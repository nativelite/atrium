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
/// pane's worktree, or its cwd) when it lies inside it, else the path itself.
/// Two worktrees of one repo hold the same relative paths, and an edit to
/// `src/x.rs` in each is the merge conflict the radar exists to catch; a
/// shared absolute path outside both collides on its own name.
pub fn touch_key(path: &str, root: Option<&str>) -> String {
    let path = path.trim();
    if let Some(root) = root.map(str::trim).filter(|r| !r.is_empty()) {
        let root = root.trim_end_matches(['/', '\\']);
        if let Some(rest) = path.strip_prefix(root) {
            if let Some(rel) = rest.strip_prefix(['/', '\\']) {
                if !rel.is_empty() {
                    return rel.replace('\\', "/");
                }
            }
        }
    }
    path.to_string()
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
        let accepted: Vec<String> = CAPS
            .iter()
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

    /// Did the pane's mod declare `cap`, and did the broker accept it?
    pub fn has_cap(&self, pane: usize, cap: &str) -> bool {
        self.info(pane)
            .is_some_and(|i| i.caps.iter().any(|c| c == cap))
    }

    /// The pane's reported status, if one was reported within
    /// [`REPORT_STALE_MS`] of `now_ms`.
    pub fn status_for(&self, pane: usize, now_ms: u64) -> Option<ModStatus> {
        let (s, at) = self.panes.get(&pane)?.1.status?;
        (now_ms.saturating_sub(at) <= REPORT_STALE_MS).then_some(s)
    }

    /// Everything reported for the pane.
    pub fn report_for(&self, pane: usize) -> Option<&PaneReport> {
        self.panes.get(&pane).map(|(_, r)| r)
    }

    /// Drop everything about a pane: it exited, or was respawned as a new
    /// process whose mod will say hello again. Replies it already gave stay
    /// readable until they expire; questions it never took are dropped.
    pub fn forget(&mut self, pane: usize) {
        self.panes.remove(&pane);
        self.respawn_flagged.remove(&pane);
        self.touched.remove(&pane);
        if let Some(q) = self.asks.remove(&pane) {
            for a in q {
                self.issued.remove(&a.id);
            }
        }
    }

    /// Record the real paths `pane` edited. `root` is the pane's worktree or
    /// cwd, which keys the touch ([`touch_key`]). A path past [`TOUCH_CAP`]
    /// distinct keys is dropped.
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
    /// worktree. Sorted by pane.
    pub fn who(&self, path: &str) -> Vec<(usize, &str, &Touch)> {
        let q = path.trim().trim_end_matches('/');
        if q.is_empty() {
            return Vec::new();
        }
        let tail = format!("/{q}");
        let hit = |s: &str| s == q || s.ends_with(&tail);
        let mut v: Vec<(usize, &str, &Touch)> = self
            .touched
            .iter()
            .flat_map(|(pane, m)| {
                m.iter()
                    .filter(move |(k, t)| hit(k) || hit(&t.path))
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
        self.replies
            .retain(|_, r| now_ms.saturating_sub(r.at_ms) <= ASK_REPLY_TTL_MS);
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

/// May a pane's mod spend a small model call on its caption? On unless a
/// fleet said `captions: false`; the tool-derived caption is free and always on.
pub fn session_captions() -> bool {
    SESSION_CAPTIONS.get().copied().unwrap_or(true)
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
}
