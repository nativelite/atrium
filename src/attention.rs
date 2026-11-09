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
}

/// The session's state, reduced to what the title and the ring need.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// Panes waiting on the human: `(key, label)`.
    pub needs_you: Vec<(u64, String)>,
    /// Open decisions addressed to the human, by bus `seq`.
    pub decisions: Vec<u64>,
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

/// A transition worth ringing for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PaneNeedsYou { key: u64, label: String },
    DecisionNeedsYou { seq: u64 },
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
    /// not rung within the cooldown), a decision newly pending.
    pub fn diff(&mut self, prev: &Summary, next: &Summary, now_ms: u64) -> Vec<Event> {
        let mut events = Vec::new();
        for (key, label) in &next.needs_you {
            if prev.needs_you.iter().any(|(k, _)| k == key) {
                continue;
            }
            let recently = self
                .last_rang
                .iter()
                .any(|(k, t)| k == key && now_ms.saturating_sub(*t) < COOLDOWN_MS);
            if recently {
                continue;
            }
            self.last_rang.retain(|(k, _)| k != key);
            self.last_rang.push((*key, now_ms));
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
        events
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
        }
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
