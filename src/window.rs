//! A hosted pane and the window of split panes it lives in: the state every
//! phase of the run loop reads and writes.

use crate::*;

/// One hosted terminal: a pty, its emulator (for tiled compositing), its
/// passthrough filter (for the passthrough / zoom path), and bar metadata. Each
/// pane has a stable `id` the window's split tree refers to.
pub(crate) struct Pane {
    /// This pane's output, from its reader thread; `None` until the loop starts
    /// one. **Declared before `pty` on purpose**: fields drop in order, and the
    /// inbox must go first so its reader stops before the `Pty`'s drop waits on
    /// the console host (see `atrium::events::PaneInbox`).
    pub(crate) inbox: Option<atrium::events::PaneInbox>,
    pub(crate) id: usize,
    pub(crate) pty: pty::Pty,
    pub(crate) term: vterm::Term,
    pub(crate) filter: atrium::filter::Passthrough,
    pub(crate) title: String,
    pub(crate) activity: bool,
    /// When this pane last produced output, on a **monotonic** clock. Stamped in
    /// the run loop wherever `activity` is set; the ctl `list`/`status` reply
    /// reports `now - last_activity` as `idle_ms` (via [`atrium::ipc::idle_ms`]),
    /// splitting the overloaded "idle" into busy vs. genuinely-stale. Monotonic so
    /// an NTP step can't make idle jump or go negative. Initialized at spawn.
    pub(crate) last_activity: std::time::Instant,
    /// Unsent human input in this pane's prompt, inferred from forwarded keys. A
    /// queued `ctl send` waits while it holds, so a delivery is never spliced
    /// onto — and submitted with — what the human is typing.
    pub(crate) draft: atrium::deliver::Draft,
    pub(crate) exited: bool,
    /// The session id atrium injected via `--session-id` when this pane is an
    /// agent it launched (§3.3). `None` for shells and agents atrium did not
    /// bind. The binder maps this to the pane's live `agsess::Status`.
    pub(crate) session_id: Option<String>,
    /// When atrium launched this pane (epoch ms, agsess clock). Used to *adopt* a
    /// session for a non-claude agent pane: atrium cannot hand it a `--session-id`,
    /// so after launch it associates the pane with the newest session discovered
    /// under the vendor's root *at or after* this instant (see
    /// [`atrium::vendors::adopt_session_for`]).
    pub(crate) launch_ms: u64,
    /// The working directory this pane's agent was launched in, if known. A tie-
    /// breaker for session adoption (prefer a discovered session whose `cwd`
    /// matches). `None` for panes opened in atrium's own cwd.
    pub(crate) cwd: Option<String>,
    /// The credential identity **name** this pane's agent runs under, if any
    /// (path B). Only the name is stored — never the resolved secret. On every
    /// spawn atrium re-resolves the env via `akey` and injects it for that child
    /// alone; the resolved values live only for the spawn call and are dropped
    /// immediately. Surfaced in the chrome as a `·<name>` tag.
    pub(crate) identity: Option<String>,
    /// Process-global agent id (see [`NEXT_AGENT`]): the ctl spawn-tree key,
    /// stable across windows. Injected into the pane as `ATRIUM_PANE` so an agent
    /// inside can attribute its own `ctl spawn` calls.
    pub(crate) agent_id: AgentId,
    /// The pane's **capability token** — an unguessable secret injected into its
    /// env as `ATRIUM_TOKEN` and matched by the ctl server to authenticate requests
    /// from this pane (identity comes from the token, not the self-reported
    /// `ATRIUM_PANE`). Empty for panes spawned before the ctl endpoint existed.
    /// The pane's capability token, or `None` when OS entropy was unavailable at
    /// spawn and atrium refused to mint a guessable one. `None` must never
    /// authenticate: a pane without a token has no ctl access, by construction.
    pub(crate) token: Option<String>,
    /// The ctl role label this pane was spawned under (`dev_1`), if any. `None`
    /// for the human's own panes and shells.
    pub(crate) role: Option<String>,
    /// The agent id of the pane whose `ctl spawn` created this one. `None` for a
    /// root pane the human opened.
    pub(crate) parent: Option<AgentId>,
    /// Depth in the spawn tree: 0 for a root/human pane, parent.depth + 1 for a
    /// ctl-spawned worker. The `--max-depth` recursion guard is checked against
    /// this.
    pub(crate) depth: usize,
    /// May this pane create teammates with `atrium ctl spawn`?
    ///
    /// A capability, not an inference. It used to be implied by having ctl access
    /// at all, so every agent in a fleet could spawn - in a seven-agent review
    /// fleet, all seven, when only the lead should. A fleet declares it per agent
    /// (default FALSE); a pane the human opened directly keeps the old behaviour,
    /// because there the human IS the caller.
    ///
    /// Checked before the depth cap and the trust ceiling, not instead of them.
    pub(crate) can_spawn: bool,
    /// True once the pane's process has produced any output. Until then, a
    /// *passthrough* (single/zoomed) pane shows the animated startup splash
    /// instead of a blank screen (the tiled path uses a per-pane blank check in
    /// the compositor). Set on the pane's first byte in the drain.
    pub(crate) painted: bool,
    /// True while this pane's app has mouse tracking enabled (it emitted a
    /// DECSET 1000/1002/1003), sniffed from its output. Scroll-wheel notches are
    /// only forwarded to a pane that wants mouse — so hovering a claude tile
    /// scrolls it, while a bare shell never receives stray mouse bytes.
    pub(crate) mouse_wanted: bool,
    /// The command vector this pane was spawned with — the user command before
    /// atrium appends trust flags or `--session-id`. Used by `atrium recover`.
    pub(crate) argv: Vec<String>,
    /// The git worktree this pane runs in, if any.
    pub(crate) worktree: Option<String>,
    /// This pane's own deny entries (its fleet agent's `deny`). Kept on the pane
    /// because `argv` does not carry them — `--disallowedTools` is built at spawn
    /// — so a snapshot has something to record and `atrium recover` something to
    /// re-apply. Empty for a pane with no per-agent rules.
    pub(crate) deny: Vec<String>,
    /// The effective trust posture this pane was spawned at, which may sit below
    /// the session ceiling (a fleet agent's `trust`, a de-escalated ctl worker).
    /// Recorded so recovery restores the pane's own posture, not the ceiling.
    pub(crate) mode: atrium::ctl::TrustMode,
    /// The last element of `argv` is this fleet agent's kickoff prompt. Set by
    /// the fleet launcher; a resume drops it so a mid-task agent is not told to
    /// start over.
    pub(crate) kickoff: bool,
    /// The worktree instructions folded into this pane's system prompt at spawn,
    /// kept so a resume can fold them in again (`argv` never carried them).
    pub(crate) norms: Option<String>,
    /// The fleet context-store variables this pane was given, for the same reason.
    pub(crate) context_env: Vec<(String, String)>,
}

/// One window: a split tree over a set of panes, plus a zoom flag. Windows are
/// the 0.1 "switchable full-screen" concept; each can now itself be a tiled
/// split tree (design doc §3: "windows and splits coexist").
pub(crate) struct Window {
    pub(crate) panes: Vec<Pane>,
    pub(crate) tree: Tree,
    pub(crate) zoomed: bool,
    pub(crate) next_id: usize,
}

impl Window {
    pub(crate) fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub(crate) fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// The pane that stands for this window in the bar: the focused one (so a
    /// zoomed or focused fleet agent reads as itself), else the first.
    pub(crate) fn bar_pane(&self) -> Option<&Pane> {
        self.pane(self.tree.focus()).or_else(|| self.panes.first())
    }

    pub(crate) fn focused_mut(&mut self) -> Option<&mut Pane> {
        let f = self.tree.focus();
        self.pane_mut(f)
    }

    /// Tiled iff more than one pane and not zoomed. A single pane, or a zoomed
    /// pane, is passthrough.
    pub(crate) fn tiled(&self) -> bool {
        self.panes.len() > 1 && !self.zoomed
    }
}
