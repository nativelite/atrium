//! The launch plan: every directory the fleet grants each agent, measured against
//! an anchor, in the form the banner discloses and the spawn then uses.

use super::banner::show_str;
use super::{real_path, resolve_dir, sanitize, shorten, show_path, Fleet, Stores, MAX_LOUD_LINES};
use std::path::{Path, PathBuf};

/// Where a grant lands relative to the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Resolved, symlinks followed, to a path under the anchor.
    Inside,
    /// Resolved to a path outside the anchor. Allowed — and always printed.
    Outside,
    /// atrium could not resolve it. Printed as loudly as `Outside`: a check that
    /// treats "cannot tell" as "fine" is the fail-open shape this repo keeps
    /// shipping.
    Unverifiable,
}

/// Which field of the agent entry produced a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Cwd,
    AddDir,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Cwd => "cwd",
            Field::AddDir => "add-dir",
        }
    }
}

/// One directory this fleet grants one agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub field: Field,
    /// The string exactly as it appears in the fleet file.
    pub raw: String,
    /// What the child is actually spawned with — the resolved path when it
    /// resolves, so the banner and the launch cannot disagree.
    pub given: PathBuf,
    /// Where it really lands, symlinks followed. `None` ⇒ unresolvable.
    pub real: Option<PathBuf>,
    pub exists: bool,
    pub reach: Reach,
    /// Set when the destination is, or contains, a known credential store.
    pub holds: Option<String>,
}

impl Grant {
    /// Resolve one `cwd` / `add_dirs` entry and classify it.
    pub fn probe(anchor: &Path, base: &Path, field: Field, raw: &str, stores: &Stores) -> Grant {
        let joined = resolve_dir(base, raw);
        let real = real_path(&joined);
        let reach = match &real {
            Some(r) if r.starts_with(anchor) => Reach::Inside,
            Some(_) => Reach::Outside,
            None => Reach::Unverifiable,
        };
        let holds = stores.describe(real.as_deref());
        Grant {
            field,
            raw: raw.to_string(),
            // An unresolvable path keeps the pre-change behaviour (the plain
            // join) — this layer discloses, it does not refuse, so an agent must
            // still launch with something.
            given: real.clone().unwrap_or_else(|| joined.clone()),
            exists: joined.exists(),
            real,
            reach,
            holds,
        }
    }

    /// Can this grant be folded into the terse "N other dirs inside" count?
    ///
    /// Only when all three of "inside", "exists" and "no credential store" hold.
    /// A path that does not exist is never quiet: a typo'd or `~`-prefixed entry
    /// (atrium does no tilde expansion, so `"~/notes"` becomes `<base>/~/notes`)
    /// is exactly the entry most likely to be wrong, and counting it as inside
    /// is a positive false statement rather than a mere gap.
    pub fn is_quiet(&self) -> bool {
        self.reach == Reach::Inside && self.exists && self.holds.is_none()
    }

    /// How alarming this grant is, lowest first.
    ///
    /// The banner is capped, so ORDER is a security property: without this a
    /// roster could bury `--add-dir ~/.ssh` behind fifty innocuous outside
    /// directories and push it past the cap, which is the same trick as burying
    /// a flag in `cmd` and hoping the review skims.
    pub fn severity(&self) -> u8 {
        match (self.holds.is_some(), self.reach, self.exists) {
            (true, _, _) => 0,
            (_, Reach::Unverifiable, _) => 1,
            (_, Reach::Outside, _) => 2,
            (_, Reach::Inside, false) => 3,
            (_, Reach::Inside, true) => 4,
        }
    }

    /// The word that starts this grant's disclosure line.
    pub fn tag(&self) -> &'static str {
        match (self.reach, self.exists, self.holds.is_some()) {
            (Reach::Unverifiable, _, _) => "UNVERIFIABLE",
            (_, _, true) => "CREDENTIALS",
            (Reach::Outside, _, _) => "OUTSIDE",
            (Reach::Inside, false, _) => "MISSING",
            (Reach::Inside, true, _) => "inside",
        }
    }

    /// The destination half of the line: where it really lands.
    pub fn destination(&self) -> String {
        match &self.real {
            Some(r) => show_path(r),
            None => "(atrium cannot resolve this path)".to_string(),
        }
    }

    /// The trailing clause: what is there, or that nothing is yet.
    pub fn note(&self) -> String {
        let mut n = String::new();
        if let Some(h) = &self.holds {
            n.push_str(" — ");
            n.push_str(h);
        }
        if !self.exists && self.reach != Reach::Unverifiable {
            n.push_str(" (does not exist yet)");
        }
        n
    }
}

/// What "inside" is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// The repository the fleet file is checked into.
    Repo,
    /// The fleet file's own directory (no enclosing repo).
    FleetDir,
    /// The directory `atrium` was run from — used for the user-global fleet file,
    /// whose own directory (`~/.config/atrium`) contains no project at all.
    InvokingDir,
}

/// The directory grants are measured against, and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub dir: PathBuf,
    pub kind: AnchorKind,
}

impl AnchorKind {
    /// Why this directory is the anchor — printed once, so an operator can tell
    /// whether "outside" is a strong statement or a weak one.
    pub fn what(self) -> &'static str {
        match self {
            AnchorKind::Repo => "the enclosing git repository",
            AnchorKind::FleetDir => "the fleet file's own directory",
            AnchorKind::InvokingDir => "the directory you ran atrium in",
        }
    }
}

/// Choose the anchor.
///
/// Anchoring on the fleet file's own directory — the obvious choice — was
/// measured and is wrong twice over. A monorepo whose fleet file lives in
/// `tools/` marks every same-repo path OUTSIDE (18 loud lines for a 6-agent
/// roster, scrolling the trust posture off a 24-row terminal), and the
/// documented user-global file at `~/.config/atrium/fleet.json` can produce
/// *nothing but* OUTSIDE, because that directory holds no project by
/// construction. A tag that appears on 100% of lines carries no information and
/// trains the operator to skim past it — which is the whole control.
///
/// So: the repo the file is checked into (the unit a reviewer already trusts),
/// else the file's directory; and for the global file, the directory atrium was
/// run in, since that is the project the operator meant.
///
/// The walk up stops short of `$HOME` and of any ancestor of it: a home
/// directory that happens to be a git repo would make "inside" cover `~/.ssh`,
/// i.e. mean nothing. (If the fleet file itself sits in `$HOME` the anchor still
/// ends up there — that case is caught by the credential-store tag on the grant,
/// not by the anchor.)
///
/// Stated plainly, because it is the weak seam here: the anchor is read from the
/// filesystem, and whoever writes the fleet file can usually write the
/// filesystem too. A `.git` created in an ancestor of a fleet file that is NOT
/// in a repo widens the anchor and makes a grant under that ancestor read as
/// "inside". Two things bound it — a real repo's own `.git` is found first, so
/// the ordinary case cannot be widened, and the chosen anchor is printed on the
/// banner's first line, so an anchor nobody recognises is itself visible. It is
/// not a defence against someone who controls both the file and the tree; it
/// makes the tag honest about which directory it is a statement *about*.
pub fn anchor_for(
    fleet_dir: &Path,
    invoking_dir: &Path,
    global: bool,
    home: Option<&Path>,
) -> Anchor {
    let (start, kind) = if global {
        (invoking_dir, AnchorKind::InvokingDir)
    } else {
        (fleet_dir, AnchorKind::FleetDir)
    };
    let base = real_path(start).unwrap_or_else(|| start.to_path_buf());
    let mut cur: &Path = &base;
    loop {
        let covers_home = home.is_some_and(|h| h.starts_with(cur));
        if !covers_home && cur.join(".git").exists() {
            return Anchor {
                dir: cur.to_path_buf(),
                kind: AnchorKind::Repo,
            };
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    Anchor { dir: base, kind }
}

/// One agent's disclosed launch: the label its pane wears, its credentials, its
/// directories, and whether atrium can see what it will do at all.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPlan {
    /// The agent's name, already defanged — this is both the banner label and
    /// the pane `role`, which the status bar and overview paint unfiltered.
    pub label: String,
    /// The identity (akey credential) this agent comes up on, if any.
    pub identity: Option<String>,
    /// `Some(stem)` when `cmd` is not an agent CLI atrium knows — a shell, a build
    /// tool. See `Plan::build` for why that is disclosed.
    pub opaque: Option<String>,
    pub cwd: Option<Grant>,
    pub add_dirs: Vec<Grant>,
}

impl AgentPlan {
    /// Every directory this agent is granted, cwd first.
    pub fn grants(&self) -> impl Iterator<Item = &Grant> {
        self.cwd.iter().chain(self.add_dirs.iter())
    }
}

/// One deduplicated disclosure line: the rendered text, the destination it
/// names, and every agent that asked for it.
#[derive(Debug, Clone, PartialEq)]
struct Loud {
    severity: u8,
    line: String,
    dest: String,
    who: Vec<String>,
}

/// The whole fleet, resolved once: what will be granted, in the form the banner
/// prints AND the form the spawn uses.
///
/// Built once in `fleet_up` and read by both the disclosure and
/// `spawn_fleet_window`. That is structural, not tidiness: the previous version
/// resolved `cwd`/`add_dirs` a second time inside the spawn, so the banner and
/// the launch were two independent computations that could differ — and a
/// symlink flipped during the operator's Enter window made them differ, turning
/// an acknowledged "inside" into a live grant on a credential store.
///
/// # What this does NOT cover — read this before relying on it
///
/// * **A symlink inside a granted directory.** `--add-dir <d>` grants `d`'s
///   whole subtree, and a link at `d/keys -> ~/.ssh` is reachable through it.
///   Only `d` is classified; walking the subtree is unbounded, racy, and still
///   wrong a second later. An add_dir grants its transitive symlink closure.
/// * **Anything `cmd` does.** `vet_spawn_argv` refuses a literal `--add-dir` in
///   `cmd`, but `["sh","-c","claude --add-dir ~/.ssh"]` is a shell command atrium
///   neither parses nor should. That is why a non-agent `cmd` is disclosed as
///   such: atrium can bound the directories *it* grants, not what a program it
///   starts grants itself.
/// * **The rest of the entry.** `prompt` and `kickoff` go into the child's argv
///   and are not shown here (they are text, not access).
/// * **TOCTOU, narrowed but not closed.** The child gets the path that was
///   resolved and shown, so re-pointing the *named* link after the ack no longer
///   moves the grant; re-pointing a directory component of that resolved path
///   between the ack and the spawn still would. Closing it needs `openat`
///   /`O_NOFOLLOW` plumbing through the spawn path.
/// * **It is not a sandbox.** Same-uid: everything atrium can read the agent can
///   read on its own (see `warden.rs`). This bounds what atrium *hands over*, and
///   makes it visible at approval time. Nothing more.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub file: PathBuf,
    pub anchor: Anchor,
    pub agents: Vec<AgentPlan>,
}

impl Plan {
    pub fn build(fleet: &Fleet, file: &Path, base: &Path, anchor: Anchor, stores: &Stores) -> Plan {
        let agents = fleet
            .agents
            .iter()
            .map(|a| {
                let stem =
                    crate::bind::command_stem(a.cmd.first().map(String::as_str).unwrap_or(""));
                AgentPlan {
                    label: sanitize(&a.name),
                    identity: a
                        .identity
                        .clone()
                        .or_else(|| fleet.identity.clone())
                        .map(|i| sanitize(&i)),
                    opaque: if crate::bind::is_agent_stem(&stem) {
                        None
                    } else {
                        Some(sanitize(&stem))
                    },
                    cwd: a
                        .cwd
                        .as_ref()
                        .map(|d| Grant::probe(&anchor.dir, base, Field::Cwd, d, stores)),
                    add_dirs: a
                        .add_dirs
                        .iter()
                        .map(|d| Grant::probe(&anchor.dir, base, Field::AddDir, d, stores))
                        .collect(),
                }
            })
            .collect();
        Plan {
            file: file.to_path_buf(),
            anchor,
            agents,
        }
    }

    /// The grants that must be named, deduplicated by (what, where) with the
    /// agents that asked for them listed together.
    ///
    /// Without this the documented `["../shared","../protos"]` pattern across ten
    /// agents printed twenty near-identical loud lines carrying two facts, and on
    /// 80 columns each wrapped: the banner defeated itself and pushed the posture
    /// line off the screen.
    fn loud(&self) -> Vec<Loud> {
        let mut out: Vec<Loud> = Vec::new();
        for a in &self.agents {
            for g in a.grants().filter(|g| !g.is_quiet()) {
                // An absolute entry resolves to itself; printing the same long
                // path twice on one line doubles its width for no information.
                let raw = show_str(&g.raw);
                let dest = g.destination();
                let line = if raw.trim_matches('"') == dest {
                    format!("{} {} {}{}", g.tag(), g.field.label(), dest, g.note())
                } else {
                    format!(
                        "{} {} {} -> {}{}",
                        g.tag(),
                        g.field.label(),
                        raw,
                        dest,
                        g.note()
                    )
                };
                match out.iter_mut().find(|l| l.line == line) {
                    Some(l) => {
                        if !l.who.contains(&a.label) {
                            l.who.push(a.label.clone());
                        }
                    }
                    None => out.push(Loud {
                        severity: g.severity(),
                        line,
                        dest: g.destination(),
                        who: vec![a.label.clone()],
                    }),
                }
            }
        }
        // Worst first, so neither the line cap nor the verdict's short list can
        // be filled with noise while the credential store scrolls away. Stable,
        // so file order still decides between equals.
        out.sort_by_key(|l| l.severity);
        out
    }

    /// The destinations an operator would want named, worst first.
    fn escapes(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for l in self.loud() {
            if !v.contains(&l.dest) {
                v.push(l.dest);
            }
        }
        v
    }

    /// The banner, one line per entry (the caller prefixes them).
    pub fn banner_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "file {} — dirs measured against {} ({})",
            show_path(&self.file),
            show_path(&self.anchor.dir),
            self.anchor.kind.what()
        ));
        let loud = self.loud();
        for Loud { line, who, .. } in loud.iter().take(MAX_LOUD_LINES) {
            let mut names = who
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if who.len() > 3 {
                names.push_str(&format!(" +{}", who.len() - 3));
            }
            lines.push(format!("  {line}   [{names}]"));
        }
        if loud.len() > MAX_LOUD_LINES {
            lines.push(format!(
                "  …and {} more disclosed dir(s), listed in {}",
                loud.len() - MAX_LOUD_LINES,
                show_path(&self.file)
            ));
        }
        let quiet = self
            .agents
            .iter()
            .flat_map(|a| a.grants())
            .filter(|g| g.is_quiet())
            .count();
        if quiet > 0 {
            lines.push(format!(
                "{quiet} other dir(s) resolve inside {}",
                show_path(&self.anchor.dir)
            ));
        }
        // Identity selects a credential through akey and was disclosed nowhere:
        // a roster could bring an agent up on `wif:prod` and nothing said so.
        //
        // Collapsed when the whole fleet shares one identity (the common case,
        // and "ambient" repeated once per agent is a quarter of the banner's
        // bulk carrying no information).
        let first = self.agents.first().map(|a| a.identity.clone());
        let uniform = self
            .agents
            .iter()
            .all(|a| Some(a.identity.clone()) == first);
        let ident = |a: &AgentPlan| a.identity.clone().unwrap_or_else(|| "ambient".to_string());
        if uniform {
            if let Some(a) = self.agents.first() {
                lines.push(format!(
                    "identities — all {} agent(s) on {}",
                    self.agents.len(),
                    ident(a)
                ));
            }
        } else {
            let ids: Vec<String> = self
                .agents
                .iter()
                .map(|a| format!("{}={}", a.label, ident(a)))
                .collect();
            lines.push(format!("identities — {}", shorten(&ids.join(", "), 200)));
        }
        let opaque: Vec<String> = self
            .agents
            .iter()
            .filter_map(|a| a.opaque.as_ref().map(|s| format!("{} ({s})", a.label)))
            .collect();
        if !opaque.is_empty() {
            lines.push(format!(
                "atrium cannot see what these will read — they do not run an agent CLI: {}",
                shorten(&opaque.join(", "), 200)
            ));
        }
        lines
    }

    /// The one line printed immediately before the blocking Enter.
    ///
    /// Measured failure it fixes: on a 24-row terminal a six-agent roster's
    /// banner scrolls, and the only line guaranteed to still be on screen at the
    /// prompt was a bare count — the one line that omitted the payload. This
    /// names the destinations.
    pub fn verdict(&self) -> String {
        let esc = self.escapes();
        if esc.is_empty() {
            return format!(
                "every agent dir resolves inside {}",
                shorten(&show_path(&self.anchor.dir), 80)
            );
        }
        // Rendered tighter than the lines above: this one has to fit on a
        // terminal row next to the count, and the reader has the full paths
        // three lines up.
        let shown: Vec<String> = esc.iter().take(3).map(|d| shorten(d, 56)).collect();
        let more = if esc.len() > 3 {
            format!(" (+{} more, listed above)", esc.len() - 3)
        } else {
            String::new()
        };
        format!(
            "GRANTS {} dir(s) that are not plain subdirectories of {}: {}{more}",
            esc.len(),
            shorten(&show_path(&self.anchor.dir), 56),
            shown.join(", ")
        )
    }
}

#[cfg(test)]
mod tests;
