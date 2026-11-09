//! The host terminal's title and the "needs you" notifications.
//!
//! When atrium sits in a background tab or window, nothing tells the operator
//! what the fleet is doing or that an agent is blocked on them. The status bar
//! knows (`?`, `N waiting`, `N decisions need you`), but only when you look.
//! This module surfaces it outside the pane: the host terminal's title names
//! the session and summarizes its state, and a *transition* into "needs you"
//! rings the terminal.
//!
//! Title (`OSC 2`, written only when it changes): `atrium · <name> · <state>`,
//! where the state is, in precedence order, `? builder needs you · 2 working`
//! / `? 2 need you` / `? 1 decision needs you`, then `● 3 working`, then
//! `○ idle`. `Idle`, `WaitingPrompt` and an unbound pane count as idle; an
//! exited pane is neither.
//!
//! Notify on transitions only: a pane newly `WaitingApproval` (per pane, with
//! a 30 s cooldown) or a newly pending human-addressed decision (by bus `seq`,
//! so a resolve followed by a new decision is not hidden by an equal count).
//! A pane's own `BEL` (from a bound agent pane) or `OSC 9` toast rings too,
//! under the same per-pane cooldown, as a moment: the title's state word is
//! the status, not the bell. The pane's title (`OSC 0`/`2`) is not a ring; it
//! is the pane's badge in the tile label and the bar entry ([`pane_badge`]).
//! All three come from the pane's emulator (`vterm::Notice`), so the loop
//! reads them whether the pane is tiled or passthrough.
//! Channels: `BEL`; an `OSC 9` toast (iTerm2, ConEmu, fwterm); and, for
//! Windows Terminal, which ignores a plain `OSC 9`, the tab progress indicator
//! `OSC 9;4;2;0` (red) while anything needs you and `OSC 9;4;0;0` to clear.
//! The `notify` setting, in `config.json` and per fleet, is `both` (default),
//! `bell`, `toast` or `off`.
//!
//! Everything here is pure: the loop builds a [`Summary`] from the facts the
//! bar already uses, hands it to the [`Tracker`] once a tick, and writes the
//! bytes it gets back right before the frame, so they ride the same console
//! write.

use agsess::Status;

/// Which channels a "needs you" transition uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    Both,
    Bell,
    Toast,
    Off,
}

impl Notify {
    pub const DEFAULT: Notify = Notify::Both;

    /// `both`, `bell`, `toast`, `off`; anything else is `None`.
    pub fn parse(s: &str) -> Option<Notify> {
        match s {
            "both" => Some(Notify::Both),
            "bell" => Some(Notify::Bell),
            "toast" => Some(Notify::Toast),
            "off" => Some(Notify::Off),
            _ => None,
        }
    }

    /// The accepted spellings, for an error message.
    pub const VALUES: &'static str = "both, bell, toast or off";

    pub fn bell(self) -> bool {
        matches!(self, Notify::Both | Notify::Bell)
    }

    pub fn toast(self) -> bool {
        matches!(self, Notify::Both | Notify::Toast)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Notify::Both => "both",
            Notify::Bell => "bell",
            Notify::Toast => "toast",
            Notify::Off => "off",
        }
    }
}

/// What a session is called and how it notifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// The fleet's name (or its `title`), else the project directory's name.
    pub name: String,
    pub notify: Notify,
}

impl Setup {
    /// The current directory's base name, `atrium` when there is none.
    pub fn project_dir_name() -> String {
        std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "atrium".to_string())
    }
}

/// One pane, as the summary needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneFact {
    /// Stable for the pane's life (its agent id).
    pub key: u64,
    /// Its role, else its title.
    pub label: String,
    pub exited: bool,
    /// The bound agent's status; `None` for an unbound pane.
    pub status: Option<Status>,
    /// The program rang `BEL` since the last tick (a `vterm::Notice::Bell`).
    pub bell: bool,
    /// The program's own notification (`OSC 9 ; text`, a `Notice::Toast`)
    /// since the last tick, the last one when it sent several.
    pub toast: Option<String>,
}

/// What a pane said this tick that is worth ringing for: a bell from a bound
/// agent pane, or a notification from any pane. A moment, not a state: it is
/// in the summary of the tick it arrived in and in no title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ring {
    pub key: u64,
    pub label: String,
    /// The pane's own words, when it sent an `OSC 9` toast.
    pub toast: Option<String>,
}

/// The session's state, reduced to what the title and the ring need.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// Panes waiting on the human: `(key, label)`.
    pub needs_you: Vec<(u64, String)>,
    /// Open decisions addressed to the human, by bus `seq`.
    pub decisions: Vec<u64>,
    /// Panes that rang or toasted this tick ([`Ring`]). Not a state: the title
    /// ignores it, and `needs()` does not count it.
    pub rings: Vec<Ring>,
    pub working: usize,
    pub idle: usize,
    pub exited: usize,
}

impl Summary {
    pub fn build(panes: &[PaneFact], human_decisions: &[u64]) -> Summary {
        let mut s = Summary {
            decisions: human_decisions.to_vec(),
            ..Summary::default()
        };
        for p in panes {
            if p.exited {
                s.exited += 1;
                continue;
            }
            match p.status {
                Some(Status::WaitingApproval) => s.needs_you.push((p.key, p.label.clone())),
                Some(Status::Working) => s.working += 1,
                Some(Status::WaitingPrompt) | Some(Status::Idle) | None => s.idle += 1,
            }
            // A shell's bell (a completion with nothing to complete) is not a
            // call for the human; a bound agent's is. A notification is
            // deliberate from any program.
            if (p.bell && p.status.is_some()) || p.toast.is_some() {
                s.rings.push(Ring {
                    key: p.key,
                    label: p.label.clone(),
                    toast: p.toast.clone(),
                });
            }
        }
        s
    }

    /// How many things need the human: waiting panes plus open decisions.
    pub fn needs(&self) -> usize {
        self.needs_you.len() + self.decisions.len()
    }
}

/// The state half of the title.
pub fn state_text(s: &Summary) -> String {
    let needs = s.needs();
    if needs > 0 {
        let mut t = match (&s.needs_you[..], s.decisions.len()) {
            ([(_, label)], 0) => format!("? {} needs you", text_safe(label)),
            ([], 1) => "? 1 decision needs you".to_string(),
            ([], n) => format!("? {n} decisions need you"),
            (_, _) => format!("? {needs} need you"),
        };
        if s.working > 0 {
            t.push_str(&format!(" \u{00b7} {} working", s.working));
        }
        return t;
    }
    if s.working > 0 {
        return format!("\u{25cf} {} working", s.working);
    }
    "\u{25cb} idle".to_string()
}

/// The whole title: `atrium · <name> · <state>`.
pub fn title_text(name: &str, s: &Summary) -> String {
    format!(
        "atrium \u{00b7} {} \u{00b7} {}",
        text_safe(name),
        state_text(s)
    )
}

/// A string safe to put inside an OSC: control characters (C0, DEL, C1,
/// `BEL` and `ESC` included), the Unicode line and paragraph separators, and
/// the bidi and zero-width format characters become spaces, and the result is
/// capped at 200 characters. The same list as the bus wake's `wake_safe`.
pub fn text_safe(s: &str) -> String {
    s.chars()
        .map(|c| if is_unsafe(c) { ' ' } else { c })
        .take(200)
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

/// Save the host's title (xterm title stack) at startup.
pub const TITLE_PUSH: &str = "\x1b[22;2t";
/// Restore it at teardown: an empty title first, for hosts with no stack
/// (ConPTY restores the tab's own name on an empty title), then the pop.
pub const TITLE_RESET: &str = "\x1b]2;\x07\x1b[23;2t";

pub fn title_osc(text: &str) -> String {
    format!("\x1b]2;{}\x07", text_safe(text))
}

pub fn bell() -> &'static str {
    "\x07"
}

/// An `OSC 9` toast (iTerm2, ConEmu, fwterm).
pub fn toast_osc(text: &str) -> String {
    format!("\x1b]9;{}\x07", text_safe(text))
}

/// Windows Terminal's tab progress indicator: red while something needs you,
/// cleared otherwise.
pub fn progress_osc(on: bool) -> &'static str {
    if on {
        "\x1b]9;4;2;0\x07"
    } else {
        "\x1b]9;4;0;0\x07"
    }
}

/// Columns a pane's own title may take in a tile label or a bar entry.
pub const PANE_BADGE_MAX: usize = 24;

/// A pane's own title as its badge: what the program set with `OSC 0`/`2`
/// (`vterm::Term::title`), scrubbed like every OSC payload, trimmed, and cut
/// to [`PANE_BADGE_MAX`] characters with an ellipsis. `None` when there is
/// none, when it is blank, or when it only names the program again
/// (`claude`, or ConPTY's `C:\WINDOWS\system32\cmd.exe` for a `cmd` pane):
/// the label already says that.
pub fn pane_badge(own: Option<&str>, program: &str) -> Option<String> {
    let own = text_safe(own?);
    let own = own.trim();
    if own.is_empty() {
        return None;
    }
    // ConPTY titles the pane with the program's path, which on Windows uses
    // a backslash; `Path` on unix knows only `/`, so split on both by hand.
    let file = own.rsplit(['\\', '/']).next().unwrap_or(own);
    let stem = match file.rfind('.') {
        Some(0) | None => file,
        Some(dot) => &file[..dot],
    };
    if own.eq_ignore_ascii_case(program) || stem.eq_ignore_ascii_case(program) {
        return None;
    }
    let n = own.chars().count();
    if n <= PANE_BADGE_MAX {
        return Some(own.to_string());
    }
    let mut cut: String = own.chars().take(PANE_BADGE_MAX - 1).collect();
    cut.push('\u{2026}');
    Some(cut)
}

/// A transition worth ringing for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PaneNeedsYou {
        key: u64,
        label: String,
    },
    DecisionNeedsYou {
        seq: u64,
    },
    /// The pane itself rang or toasted ([`Ring`]).
    PaneRang {
        key: u64,
        label: String,
        toast: Option<String>,
    },
}

/// Rings between two summaries, with a per-pane cooldown.
#[derive(Debug, Clone, Default)]
pub struct Edges {
    /// Pane key -> when it last rang (ms).
    last_rang: Vec<(u64, u64)>,
}

pub const COOLDOWN_MS: u64 = 30_000;

impl Edges {
    /// The events `next` holds that `prev` did not: a pane newly waiting (and
    /// not rung within the cooldown), a decision newly pending, a pane that
    /// rang this tick (under the same per-pane cooldown, so an agent that
    /// rings as it asks is heard once).
    pub fn diff(&mut self, prev: &Summary, next: &Summary, now_ms: u64) -> Vec<Event> {
        let mut events = Vec::new();
        for (key, label) in &next.needs_you {
            if prev.needs_you.iter().any(|(k, _)| k == key) {
                continue;
            }
            if !self.ring(*key, now_ms) {
                continue;
            }
            events.push(Event::PaneNeedsYou {
                key: *key,
                label: label.clone(),
            });
        }
        for seq in &next.decisions {
            if !prev.decisions.contains(seq) {
                events.push(Event::DecisionNeedsYou { seq: *seq });
            }
        }
        for r in &next.rings {
            if !self.ring(r.key, now_ms) {
                continue;
            }
            events.push(Event::PaneRang {
                key: r.key,
                label: r.label.clone(),
                toast: r.toast.clone(),
            });
        }
        events
    }

    /// Whether pane `key` may ring now; stamps it when it may.
    fn ring(&mut self, key: u64, now_ms: u64) -> bool {
        let recently = self
            .last_rang
            .iter()
            .any(|(k, t)| *k == key && now_ms.saturating_sub(*t) < COOLDOWN_MS);
        if recently {
            return false;
        }
        self.last_rang.retain(|(k, _)| *k != key);
        self.last_rang.push((key, now_ms));
        true
    }
}

/// The loop's one object: feed it a [`Summary`] a tick, write what it returns.
#[derive(Debug, Clone)]
pub struct Tracker {
    setup: Setup,
    prev: Summary,
    edges: Edges,
    last_title: Option<String>,
    progress_on: bool,
}

impl Tracker {
    pub fn new(setup: Setup) -> Tracker {
        Tracker {
            setup,
            prev: Summary::default(),
            edges: Edges::default(),
            last_title: None,
            progress_on: false,
        }
    }

    pub fn setup(&self) -> &Setup {
        &self.setup
    }

    /// The bytes for this tick: the title when it changed, a ring on a
    /// transition, the tab indicator when the needs state flipped. Empty most
    /// ticks.
    pub fn tick(&mut self, next: Summary, now_ms: u64) -> Vec<u8> {
        let mut out = String::new();
        let title = title_text(&self.setup.name, &next);
        if self.last_title.as_deref() != Some(title.as_str()) {
            out.push_str(&title_osc(&title));
            self.last_title = Some(title);
        }
        let events = self.edges.diff(&self.prev, &next, now_ms);
        if !events.is_empty() && self.setup.notify != Notify::Off {
            if self.setup.notify.bell() {
                out.push_str(bell());
            }
            if self.setup.notify.toast() {
                let text = match &events[..] {
                    [Event::PaneNeedsYou { label, .. }] => format!("atrium: {label} needs you"),
                    [Event::DecisionNeedsYou { .. }] => "atrium: a decision needs you".to_string(),
                    // The pane's own words when it gave some; else that it rang.
                    [Event::PaneRang {
                        label,
                        toast: Some(toast),
                        ..
                    }] => format!("atrium: {label} \u{00b7} {toast}"),
                    [Event::PaneRang {
                        label, toast: None, ..
                    }] => {
                        format!("atrium: {label} rang")
                    }
                    many => format!("atrium: {} need you", many.len()),
                };
                out.push_str(&toast_osc(&text));
            }
        }
        if self.setup.notify.toast() {
            let on = next.needs() > 0;
            if on != self.progress_on {
                out.push_str(progress_osc(on));
                self.progress_on = on;
            }
        }
        self.prev = next;
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(key: u64, label: &str, status: Option<Status>) -> PaneFact {
        PaneFact {
            key,
            label: label.to_string(),
            exited: false,
            status,
            bell: false,
            toast: None,
        }
    }

    fn ringing(key: u64, label: &str, status: Option<Status>) -> PaneFact {
        PaneFact {
            bell: true,
            ..pane(key, label, status)
        }
    }

    #[test]
    fn a_bell_from_a_bound_pane_is_a_ring_and_not_a_state() {
        let s = Summary::build(
            &[
                ringing(1, "builder", Some(Status::Working)),
                ringing(2, "sh", None),
                PaneFact {
                    exited: true,
                    ..ringing(3, "gone", Some(Status::Working))
                },
            ],
            &[],
        );
        assert_eq!(
            s.rings,
            vec![Ring {
                key: 1,
                label: "builder".to_string(),
                toast: None
            }],
            "a bound pane's bell rings; a shell's and an exited pane's do not"
        );
        assert!(
            s.needs_you.is_empty() && s.needs() == 0,
            "a bell is not a state"
        );
        assert_eq!(
            state_text(&s),
            "\u{25cf} 1 working",
            "the state word is the status"
        );
    }

    #[test]
    fn a_toast_rings_from_any_pane_and_carries_its_words() {
        let s = Summary::build(
            &[PaneFact {
                toast: Some("tests done".to_string()),
                ..pane(4, "sh", None)
            }],
            &[],
        );
        assert_eq!(
            s.rings,
            vec![Ring {
                key: 4,
                label: "sh".to_string(),
                toast: Some("tests done".to_string())
            }]
        );
        assert_eq!(state_text(&s), "\u{25cb} idle");
    }

    #[test]
    fn diff_rings_a_bell_under_the_per_pane_cooldown() {
        let mut e = Edges::default();
        let quiet = Summary::build(&[pane(1, "b", Some(Status::Working))], &[]);
        let rang = Summary::build(&[ringing(1, "b", Some(Status::Working))], &[]);
        assert_eq!(
            e.diff(&quiet, &rang, 0),
            vec![Event::PaneRang {
                key: 1,
                label: "b".to_string(),
                toast: None
            }]
        );
        assert!(
            e.diff(&quiet, &rang, COOLDOWN_MS - 1).is_empty(),
            "a second bell inside the cooldown is not heard"
        );
        assert_eq!(e.diff(&quiet, &rang, COOLDOWN_MS).len(), 1);
        // The cooldown is shared with the status ring: a pane that just rang
        // does not ring again for asking, and one that rings as it asks is
        // heard once, as the ask.
        let asks = Summary::build(&[ringing(1, "b", Some(Status::WaitingApproval))], &[]);
        assert!(
            e.diff(&quiet, &asks, COOLDOWN_MS + 1).is_empty(),
            "it rang a moment ago"
        );
        let ev = e.diff(&quiet, &asks, 2 * COOLDOWN_MS + 1);
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert!(matches!(ev[0], Event::PaneNeedsYou { .. }), "{ev:?}");
    }

    #[test]
    fn tracker_toasts_the_panes_own_words_for_its_ring() {
        let mut t = Tracker::new(setup(Notify::Both));
        t.tick(Summary::default(), 0);
        let said = Summary::build(
            &[PaneFact {
                toast: Some("build green".to_string()),
                ..ringing(1, "builder", Some(Status::Working))
            }],
            &[],
        );
        let out = String::from_utf8(t.tick(said, 10)).unwrap();
        assert!(
            out.contains("\x07\x1b]9;atrium: builder \u{00b7} build green\x07"),
            "{out:?}"
        );
        assert!(
            !out.contains(progress_osc(true)),
            "a ring is not a needs state: {out:?}"
        );
        let bare = Summary::build(&[ringing(2, "lead", Some(Status::Working))], &[]);
        let out = String::from_utf8(t.tick(bare, 20)).unwrap();
        assert!(out.contains("\x1b]9;atrium: lead rang\x07"), "{out:?}");
    }

    #[test]
    fn pane_badge_is_the_programs_title_trimmed_and_scrubbed() {
        assert_eq!(pane_badge(None, "claude"), None);
        assert_eq!(pane_badge(Some("  "), "claude"), None);
        assert_eq!(
            pane_badge(Some("claude"), "claude"),
            None,
            "the label says that"
        );
        assert_eq!(pane_badge(Some("Claude"), "claude"), None);
        assert_eq!(
            pane_badge(Some("C:\\WINDOWS\\system32\\cmd.exe"), "cmd"),
            None,
            "ConPTY's path to the program"
        );
        assert_eq!(
            pane_badge(Some("\u{2733} fix tests"), "claude").as_deref(),
            Some("\u{2733} fix tests")
        );
        assert_eq!(
            pane_badge(Some("a\x07b\x1b]0;x\x07c"), "claude").as_deref(),
            Some("a b ]0;x c"),
            "controls become spaces; `]` is not one"
        );
        let long = "x".repeat(40);
        let cut = pane_badge(Some(&long), "claude").unwrap();
        assert_eq!(cut.chars().count(), PANE_BADGE_MAX);
        assert!(cut.ends_with('\u{2026}'), "{cut:?}");
        let exact = "y".repeat(PANE_BADGE_MAX);
        assert_eq!(
            pane_badge(Some(&exact), "claude").as_deref(),
            Some(exact.as_str())
        );
    }

    fn setup(notify: Notify) -> Setup {
        Setup {
            name: "proj".to_string(),
            notify,
        }
    }

    #[test]
    fn summary_counts_panes_by_status() {
        let mut gone = pane(9, "old", Some(Status::Working));
        gone.exited = true;
        let s = Summary::build(
            &[
                pane(1, "builder", Some(Status::WaitingApproval)),
                pane(2, "lead", Some(Status::Working)),
                pane(3, "sh", None),
                pane(4, "rev", Some(Status::WaitingPrompt)),
                gone,
            ],
            &[7],
        );
        assert_eq!(s.needs_you, vec![(1, "builder".to_string())]);
        assert_eq!(s.decisions, vec![7]);
        assert_eq!((s.working, s.idle, s.exited), (1, 2, 1));
        assert_eq!(s.needs(), 2);
    }

    #[test]
    fn title_prefers_needs_you_then_working_then_idle() {
        let t = |panes: &[PaneFact], d: &[u64]| state_text(&Summary::build(panes, d));
        assert_eq!(t(&[], &[]), "\u{25cb} idle");
        assert_eq!(
            t(
                &[
                    pane(1, "a", Some(Status::Working)),
                    pane(2, "b", Some(Status::Working))
                ],
                &[]
            ),
            "\u{25cf} 2 working"
        );
        assert_eq!(
            t(
                &[
                    pane(1, "builder", Some(Status::WaitingApproval)),
                    pane(2, "a", Some(Status::Working)),
                    pane(3, "b", Some(Status::Working)),
                ],
                &[]
            ),
            "? builder needs you \u{00b7} 2 working"
        );
        assert_eq!(
            t(
                &[
                    pane(1, "a", Some(Status::WaitingApproval)),
                    pane(2, "b", Some(Status::WaitingApproval)),
                ],
                &[]
            ),
            "? 2 need you"
        );
        assert_eq!(t(&[], &[4]), "? 1 decision needs you");
        assert_eq!(t(&[], &[4, 5]), "? 2 decisions need you");
        assert_eq!(
            t(&[pane(1, "a", Some(Status::WaitingApproval))], &[4]),
            "? 2 need you"
        );
        assert_eq!(
            title_text("proj", &Summary::default()),
            "atrium \u{00b7} proj \u{00b7} \u{25cb} idle"
        );
    }

    #[test]
    fn title_text_scrubs_control_characters_and_caps_length() {
        let s = Summary::build(
            &[pane(
                1,
                "a\x07b\x1b]0;x\x07c\u{202e}d",
                Some(Status::WaitingApproval),
            )],
            &[],
        );
        let t = state_text(&s);
        assert!(
            !t.contains('\x07') && !t.contains('\x1b') && !t.contains('\u{202e}'),
            "{t:?}"
        );
        assert!(t.contains("a b"), "{t:?}");
        let long = "x".repeat(500);
        assert_eq!(text_safe(&long).chars().count(), 200);
        assert_eq!(title_osc("a\x07b"), "\x1b]2;a b\x07");
    }

    #[test]
    fn diff_rings_only_on_a_newly_waiting_pane() {
        let mut e = Edges::default();
        let none = Summary::default();
        let one = Summary::build(&[pane(1, "builder", Some(Status::WaitingApproval))], &[]);
        let ev = e.diff(&none, &one, 1_000);
        assert_eq!(
            ev,
            vec![Event::PaneNeedsYou {
                key: 1,
                label: "builder".to_string()
            }]
        );
        assert!(
            e.diff(&one, &one, 2_000).is_empty(),
            "still waiting is not new"
        );
    }

    #[test]
    fn diff_honors_the_per_pane_cooldown() {
        let mut e = Edges::default();
        let none = Summary::default();
        let one = Summary::build(&[pane(1, "b", Some(Status::WaitingApproval))], &[]);
        assert_eq!(e.diff(&none, &one, 0).len(), 1);
        assert!(e.diff(&none, &one, COOLDOWN_MS - 1).is_empty());
        assert_eq!(e.diff(&none, &one, COOLDOWN_MS).len(), 1);
    }

    #[test]
    fn diff_rings_for_a_newly_pending_human_decision() {
        let mut e = Edges::default();
        let a = Summary::build(&[], &[1]);
        let b = Summary::build(&[], &[2]);
        assert_eq!(e.diff(&a, &b, 0), vec![Event::DecisionNeedsYou { seq: 2 }]);
        assert!(e.diff(&b, &b, 0).is_empty());
    }

    #[test]
    fn tracker_writes_the_title_only_when_it_changes() {
        let mut t = Tracker::new(setup(Notify::Off));
        let idle = Summary::default();
        let first = String::from_utf8(t.tick(idle.clone(), 0)).unwrap();
        assert_eq!(
            first,
            "\x1b]2;atrium \u{00b7} proj \u{00b7} \u{25cb} idle\x07"
        );
        assert!(t.tick(idle.clone(), 20).is_empty());
        let busy = Summary::build(&[pane(1, "a", Some(Status::Working))], &[]);
        let third = String::from_utf8(t.tick(busy, 40)).unwrap();
        assert!(third.contains("\u{25cf} 1 working"), "{third:?}");
        assert!(third.ends_with('\x07'), "no bell under Off: {third:?}");
    }

    #[test]
    fn tracker_channels_follow_notify() {
        let waiting = Summary::build(&[pane(1, "builder", Some(Status::WaitingApproval))], &[]);
        let idle = Summary::default();
        let bytes = |n: Notify| {
            let mut t = Tracker::new(setup(n));
            t.tick(idle.clone(), 0);
            let on = String::from_utf8(t.tick(waiting.clone(), 100)).unwrap();
            let off = String::from_utf8(t.tick(idle.clone(), 200)).unwrap();
            (on, off)
        };
        let (on, off) = bytes(Notify::Both);
        assert!(on.contains("\x1b]2;"), "title changed");
        assert!(
            on.contains("\x07\x1b]9;atrium: builder needs you\x07"),
            "{on:?}"
        );
        assert!(on.ends_with(progress_osc(true)), "{on:?}");
        assert!(off.ends_with(progress_osc(false)), "{off:?}");
        let (on, _) = bytes(Notify::Bell);
        assert!(!on.contains("\x1b]9;"), "{on:?}");
        assert!(
            on.matches('\x07').count() >= 2,
            "title terminator + bell: {on:?}"
        );
        let (on, _) = bytes(Notify::Toast);
        assert!(
            on.contains("\x1b]9;atrium: builder needs you\x07"),
            "{on:?}"
        );
        assert!(on.contains(progress_osc(true)));
        assert!(!on.contains("\x07\x07"), "no bare bell: {on:?}");
        let (on, off) = bytes(Notify::Off);
        assert!(
            !on.contains("\x1b]9;") && !off.contains("\x1b]9;"),
            "{on:?}"
        );
        assert_eq!(
            on.matches('\x07').count(),
            1,
            "only the title's terminator: {on:?}"
        );
    }

    #[test]
    fn escapes_are_exact() {
        assert_eq!(TITLE_PUSH, "\x1b[22;2t");
        assert_eq!(TITLE_RESET, "\x1b]2;\x07\x1b[23;2t");
        assert_eq!(bell(), "\x07");
        assert_eq!(toast_osc("hi"), "\x1b]9;hi\x07");
        assert_eq!(progress_osc(true), "\x1b]9;4;2;0\x07");
        assert_eq!(progress_osc(false), "\x1b]9;4;0;0\x07");
    }

    #[test]
    fn notify_parses_its_four_values_and_rejects_others() {
        assert_eq!(Notify::parse("both"), Some(Notify::Both));
        assert_eq!(Notify::parse("bell"), Some(Notify::Bell));
        assert_eq!(Notify::parse("toast"), Some(Notify::Toast));
        assert_eq!(Notify::parse("off"), Some(Notify::Off));
        assert_eq!(Notify::parse("loud"), None);
        assert!(Notify::Both.bell() && Notify::Both.toast());
        assert!(Notify::Bell.bell() && !Notify::Bell.toast());
        assert!(!Notify::Toast.bell() && Notify::Toast.toast());
        assert!(!Notify::Off.bell() && !Notify::Off.toast());
        assert_eq!(Notify::DEFAULT, Notify::Both);
    }
}
