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

use std::collections::HashMap;

/// The capabilities a mod may declare and the broker will act on. Anything
/// else in a `hello` is dropped from `accepted` and never consulted.
pub const CAPS: &[&str] = &["status", "inbox", "answer", "context", "tools", "guard"];

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
}

/// The broker's memory of every pane's mod, keyed by agent id.
#[derive(Debug, Default)]
pub struct ModState {
    panes: HashMap<usize, (Option<ModInfo>, PaneReport)>,
    answer_seq: u64,
}

impl ModState {
    /// Record a `hello`. Idempotent: a reload re-sends it and replaces the
    /// previous info; reports already made are kept. Returns the capabilities
    /// accepted, in [`CAPS`] order, de-duplicated.
    pub fn hello(
        &mut self,
        pane: usize,
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
        };
        self.panes.entry(pane).or_default().0 = Some(info);
        accepted
    }

    /// Record a `report`. Fields given replace what was held; fields absent
    /// are kept. Text is scrubbed of control characters and capped. Returns
    /// the sequence number of the answer, when the report carried one.
    pub fn report(&mut self, pane: usize, r: &Report, now_ms: u64) -> Option<u64> {
        let slot = &mut self.panes.entry(pane).or_default().1;
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
    /// process whose mod will say hello again.
    pub fn forget(&mut self, pane: usize) {
        self.panes.remove(&pane);
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
        for (pane, session) in panes {
            let Some(id) = session.as_deref() else {
                continue;
            };
            if let Some(s) = self
                .status_for(*pane, now_ms)
                .and_then(ModStatus::as_agsess)
            {
                world.set_status_override(id, s);
            }
        }
    }
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
    }

    #[test]
    fn a_second_hello_replaces_the_info_and_keeps_the_reports() {
        let mut m = ModState::default();
        m.hello(1, "0.1.0", None, &caps(&["status"]), 10);
        m.report(
            1,
            &Report {
                context_pct: Some(40),
                ..Report::default()
            },
            20,
        );
        m.hello(1, "0.2.0", None, &caps(&["status", "inbox"]), 30);
        assert_eq!(m.info(1).map(|i| i.version.as_str()), Some("0.2.0"));
        assert!(m.has_cap(1, "inbox"));
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
    fn text_safe_replaces_every_unsafe_character() {
        let s = "a\u{2028}b\u{202E}c\u{200B}d\u{FEFF}e\u{85}f\x7fg";
        assert_eq!(text_safe(s), "a b c d e f g");
        assert_eq!(text_safe("plain ünïcode ok"), "plain ünïcode ok");
    }

    #[test]
    fn forget_drops_the_pane_entirely() {
        let mut m = ModState::default();
        m.hello(5, "0.1.0", None, &caps(&["status"]), 1);
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
