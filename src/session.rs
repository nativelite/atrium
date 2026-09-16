//! Session snapshot: capture, persist, and restore a window's grid layout and
//! pane roster so an atrium session can be resumed after a restart or crash.
//!
//! Three clean seams:
//!
//! * [`capture`] — pure: takes in-memory state, builds a [`Snapshot`]. No I/O,
//!   fully testable without a pty, window, or live session.
//! * [`save`] — atomic: serialises to a sibling `.tmp` file then renames over
//!   the target. A crash mid-write leaves the previous snapshot intact —
//!   matching the pattern in `reap::write_registry`.
//! * [`load`] — tolerant: a missing or corrupt file returns a clear `Err`,
//!   never panics.
//!
//! The on-disk format is a single JSON line (no pretty-printing), matching the
//! JSONL idiom in `audit.rs`. Versioned from the start so a future daemon can
//! detect and migrate old snapshots without a hard failure. Only identity
//! **names** are stored — never resolved secrets (name-only contract from
//! `identity.rs`).

use std::path::Path;

use json::{Number, Value};

/// On-disk schema version. Increment on any backward-incompatible change.
///
/// **v2** adds the [`PolicyRecord`] block and the per-pane capability fields
/// (`deny`, `can_spawn`, `depth`, `parent_pane`, `mode`). Reading stays
/// backward-compatible: every v2 key is optional on load, so a v1 file still
/// opens and recovery falls back to the command line for what it cannot know.
pub const VERSION: u32 = 2;

/// One pane's recoverable state. `Option` fields mirror what the live `Pane`
/// holds — `None` when the pane was opened without that attribute.
///
/// `argv` is the command vector the pane was spawned with — the minimum
/// needed for `atrium recover` to re-launch each agent. `worktree` is the git
/// worktree directory the agent ran in, if the fleet declared one.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneRecord {
    pub id: usize,
    pub role: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub identity: Option<String>,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    /// This pane's **own** deny entries (a fleet agent's `deny`), on top of the
    /// session's rules. Not derivable from `argv`: the `--disallowedTools` flags
    /// are built at spawn from the roster, so a recovery that replays argv alone
    /// gives the pane back every command its agent entry took away.
    pub deny: Vec<String>,
    /// May this pane create teammates with `ctl spawn`? A capability the roster
    /// grants (default false for a fleet agent), never an inference — a recovery
    /// that re-spawns at the spawn-path default hands it to panes that never had
    /// it.
    pub can_spawn: bool,
    /// Depth in the ctl spawn tree (0 for a root pane). Restored so the
    /// `--max-depth` recursion guard resumes where it left off instead of
    /// counting from zero again.
    pub depth: usize,
    /// The **pane id** of the pane whose `ctl spawn` created this one, if any.
    /// Deliberately the pane id rather than the parent's `AgentId`: agent ids are
    /// minted per process and are re-minted on recovery, so a stored agent id
    /// could name a different pane in the recovered session. The pane id is
    /// stable within the snapshot, and recovery maps it back to the new agent id.
    pub parent_pane: Option<usize>,
    /// The effective trust posture this pane ran at. A fleet agent may sit
    /// *below* the session ceiling (`"trust"` on its entry) and a ctl-spawned
    /// worker may be de-escalated at spawn; without this every recovered pane
    /// comes back at the session ceiling. Re-applied capped to the ceiling — a
    /// snapshot can lower a pane's posture, never raise it.
    pub mode: Option<crate::ctl::TrustMode>,
}

/// The serialisable grid layout: pane ids in left-to-right tree order and
/// which pane has focus. On restore, `Tree::grid_from_ids(&layout.ids)`
/// rebuilds a balanced grid; exact split proportions are a v2 concern.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutRecord {
    pub ids: Vec<usize>,
    pub focus: usize,
}

/// The **session policy** in force when the snapshot was taken: what a recovered
/// session must re-apply to be the same session and not merely the same layout.
///
/// None of this is derivable from the pane roster. The deny list, the compile
/// pool and the memory ceiling are applied once at `fleet up` (`fleet_cli.rs`)
/// and live in process globals; replaying each pane's `argv` restores none of
/// them. A recovery without this block came back with the fleet's guards off —
/// `git push` and `cargo build --release` allowed again, the compile pool and
/// the memory cap gone.
///
/// `Snapshot::policy` is `None` on a v1 snapshot, written before this block
/// existed; recovery then falls back to the command line exactly as before.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyRecord {
    /// The session-wide deny entries in force — `ATRIUM_DENY` plus the fleet's
    /// `deny`, already merged by [`crate::trust::session_deny`]. The built-in
    /// fail-safes and each pane's own rules are added on top at spawn, and
    /// `deny_args` dedupes, so re-supplying an entry is harmless.
    pub deny: Vec<String>,
    /// The compile pool size (a fleet's `build_jobs`), if a pool was created.
    pub build_jobs: Option<usize>,
    /// The session memory ceiling in MB (a fleet's `memory_mb`), if one was set.
    pub memory_mb: Option<u64>,
    /// The session trust ceiling every pane is capped at.
    pub trust: Option<crate::ctl::TrustMode>,
    /// Was the ctl control plane bound for this session?
    pub allow_ctl: bool,
    /// The ctl spawn-tree depth guard (`usize::MAX` == unlimited).
    pub max_depth: usize,
}

/// Who wrote a snapshot, for which project, when, and how that session ended —
/// what decides whether a launch should offer to resume it.
///
/// All of it is optional on read: a v1 snapshot, or one passed explicitly with
/// `atrium recover --snapshot`, simply has no meta and is never auto-offered.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionMeta {
    /// The project directory the session was launched in (canonical form, see
    /// `session_store::project_id`). Resuming is offered only in that project.
    pub project: Option<String>,
    /// The pid of the atrium that wrote the file.
    pub pid: Option<u32>,
    /// When the file was last written, epoch ms. A running atrium rewrites it on a
    /// heartbeat even when nothing changed, so a stale timestamp means the writer
    /// is gone — which, unlike the pid alone, survives a reboot reusing that pid.
    pub saved_at_ms: Option<u64>,
    /// The session ended on purpose (a normal quit), or was already resumed or
    /// dismissed. A clean session is never offered, though `atrium recover` can
    /// still restore it explicitly.
    pub clean: bool,
}

/// A versioned snapshot of a window's grid and pane roster. Designed to also
/// serve as the future daemon's persistent state model (ROADMAP Phase 2).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub layout: LayoutRecord,
    pub panes: Vec<PaneRecord>,
    /// The session policy to re-apply on recovery; `None` on a v1 snapshot.
    pub policy: Option<PolicyRecord>,
    /// Ownership and lifecycle; empty on a v1 snapshot.
    pub meta: SessionMeta,
}

/// Input for one pane in a [`capture`] call — a plain data struct so the
/// function is testable without a live `Pty`, `Window`, or terminal.
#[derive(Debug, Clone)]
pub struct PaneCapture {
    pub id: usize,
    pub role: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub identity: Option<String>,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub deny: Vec<String>,
    pub can_spawn: bool,
    pub depth: usize,
    pub parent_pane: Option<usize>,
    pub mode: Option<crate::ctl::TrustMode>,
}

/// Build a [`Snapshot`] from explicit window state. **Pure** — no I/O, no
/// side effects. Fully testable without a live window.
///
/// * `ordered_ids` — pane ids in layout (left-to-right tree) order, as
///   returned by `Tree::ids()`.
/// * `focus` — the focused pane id, as returned by `Tree::focus()`.
/// * `panes` — one [`PaneCapture`] per live pane (any order).
/// * `policy` — the session policy to re-apply on recovery, or `None` when the
///   caller has none to record (which yields a v1-shaped recovery).
pub fn capture(
    ordered_ids: Vec<usize>,
    focus: usize,
    panes: Vec<PaneCapture>,
    policy: Option<PolicyRecord>,
) -> Snapshot {
    Snapshot {
        version: VERSION,
        layout: LayoutRecord {
            ids: ordered_ids,
            focus,
        },
        panes: panes
            .into_iter()
            .map(|p| PaneRecord {
                id: p.id,
                role: p.role,
                argv: p.argv,
                cwd: p.cwd,
                identity: p.identity,
                session_id: p.session_id,
                worktree: p.worktree,
                deny: p.deny,
                can_spawn: p.can_spawn,
                depth: p.depth,
                parent_pane: p.parent_pane,
                mode: p.mode,
            })
            .collect(),
        policy,
        meta: SessionMeta::default(),
    }
}

/// This session's policy, recorded once at launch (`run()`) and read by every
/// snapshot. A global for the same reason `trust::FLEET_DENY` is one: the values
/// are decided on the launch path (`fleet up`, a plain launch, a recovery) but
/// needed on the snapshot path, which sees only windows and panes.
static POLICY: std::sync::OnceLock<PolicyRecord> = std::sync::OnceLock::new();

/// Record this session's policy. First call wins, matching the other
/// session-wide setters (`set_fleet_deny`, `set_fleet_mb`).
pub fn set_policy(policy: PolicyRecord) {
    let _ = POLICY.set(policy);
}

/// This session's recorded policy, if the launch path set one.
pub fn policy() -> Option<PolicyRecord> {
    POLICY.get().cloned()
}

// ---- serialisation -------------------------------------------------------

fn opt_str(v: Option<&str>) -> Value {
    v.map(|s| Value::String(s.to_string()))
        .unwrap_or(Value::Null)
}

/// A `usize` as a JSON integer. `usize::MAX` — what `--max-depth 0` (unlimited)
/// becomes — exceeds `i64`, so it is clamped to `i64::MAX`. That is not the same
/// number, but it is the same guard: no spawn tree reaches 9.2e18 deep.
fn num(n: usize) -> Value {
    Value::Number(Number::Int(n.min(i64::MAX as usize) as i64))
}

fn opt_num(n: Option<usize>) -> Value {
    n.map(num).unwrap_or(Value::Null)
}

fn str_array(v: &[String]) -> Value {
    Value::Array(v.iter().map(|s| Value::String(s.clone())).collect())
}

fn pane_to_json(p: &PaneRecord) -> Value {
    Value::Object(vec![
        ("id".to_string(), Value::Number(Number::Int(p.id as i64))),
        ("role".to_string(), opt_str(p.role.as_deref())),
        ("argv".to_string(), str_array(&p.argv)),
        ("cwd".to_string(), opt_str(p.cwd.as_deref())),
        ("identity".to_string(), opt_str(p.identity.as_deref())),
        ("session_id".to_string(), opt_str(p.session_id.as_deref())),
        ("worktree".to_string(), opt_str(p.worktree.as_deref())),
        ("deny".to_string(), str_array(&p.deny)),
        ("can_spawn".to_string(), Value::Bool(p.can_spawn)),
        ("depth".to_string(), num(p.depth)),
        ("parent_pane".to_string(), opt_num(p.parent_pane)),
        (
            "mode".to_string(),
            opt_str(p.mode.map(|m| m.policy_label())),
        ),
    ])
}

fn policy_to_json(p: &PolicyRecord) -> Value {
    Value::Object(vec![
        ("deny".to_string(), str_array(&p.deny)),
        ("build_jobs".to_string(), opt_num(p.build_jobs)),
        (
            "memory_mb".to_string(),
            p.memory_mb
                .map(|mb| Value::Number(Number::Int(mb.min(i64::MAX as u64) as i64)))
                .unwrap_or(Value::Null),
        ),
        (
            "trust".to_string(),
            opt_str(p.trust.map(|m| m.policy_label())),
        ),
        ("allow_ctl".to_string(), Value::Bool(p.allow_ctl)),
        ("max_depth".to_string(), num(p.max_depth)),
    ])
}

fn snapshot_to_json(s: &Snapshot) -> Value {
    let ids: Vec<Value> = s
        .layout
        .ids
        .iter()
        .map(|&id| Value::Number(Number::Int(id as i64)))
        .collect();
    Value::Object(vec![
        (
            "version".to_string(),
            Value::Number(Number::Int(s.version as i64)),
        ),
        (
            "layout".to_string(),
            Value::Object(vec![
                ("ids".to_string(), Value::Array(ids)),
                (
                    "focus".to_string(),
                    Value::Number(Number::Int(s.layout.focus as i64)),
                ),
            ]),
        ),
        (
            "panes".to_string(),
            Value::Array(s.panes.iter().map(pane_to_json).collect()),
        ),
        (
            "policy".to_string(),
            s.policy.as_ref().map(policy_to_json).unwrap_or(Value::Null),
        ),
        ("meta".to_string(), meta_to_json(&s.meta)),
    ])
}

fn meta_to_json(m: &SessionMeta) -> Value {
    Value::Object(vec![
        ("project".to_string(), opt_str(m.project.as_deref())),
        (
            "pid".to_string(),
            m.pid
                .map(|p| Value::Number(Number::Int(p as i64)))
                .unwrap_or(Value::Null),
        ),
        (
            "saved_at_ms".to_string(),
            m.saved_at_ms
                .map(|t| Value::Number(Number::Int(t.min(i64::MAX as u64) as i64)))
                .unwrap_or(Value::Null),
        ),
        ("clean".to_string(), Value::Bool(m.clean)),
    ])
}

// ---- deserialisation helpers ---------------------------------------------

fn get<'a>(obj: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    // Last-wins, matching the fleet/context parsers.
    obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn obj_of(v: &Value) -> Option<&Vec<(String, Value)>> {
    match v {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

fn opt_str_from(v: &Value) -> Option<Option<String>> {
    match v {
        Value::String(s) => Some(Some(s.clone())),
        Value::Null => Some(None),
        _ => None,
    }
}

// The v2 fields are read **tolerantly**: a missing or malformed key falls back to
// the documented default rather than failing the whole load. A v1 snapshot (no
// such keys at all) must still open — a session that cannot be recovered because
// its snapshot predates a schema bump is the failure mode this guards against.

/// A deny list, read **strictly**: absent or `null` is an empty list (nothing was
/// denied), but a present-and-malformed value is `None` — which fails the whole
/// load.
///
/// The tolerant reading is wrong for this one field. Every other v2 key degrades
/// to a default that is *safe* when it is wrong; a deny list that degrades to
/// `[]` silently re-allows `git push` and `cargo build --release` on a recovered
/// fleet, which is the exact failure this schema exists to prevent. A corrupt
/// guard costs you the recovery, not the guard — and the error says to pass the
/// rules on the command line instead.
fn deny_list(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|a| match a {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn int_of(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(Number::Int(n))) => Some(*n),
        _ => None,
    }
}

fn usize_or(v: Option<&Value>, default: usize) -> usize {
    match int_of(v) {
        Some(n) if n >= 0 => n as usize,
        _ => default,
    }
}

fn opt_usize(v: Option<&Value>) -> Option<usize> {
    int_of(v).filter(|n| *n >= 0).map(|n| n as usize)
}

fn opt_u64(v: Option<&Value>) -> Option<u64> {
    int_of(v).filter(|n| *n >= 0).map(|n| n as u64)
}

fn bool_or(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        _ => default,
    }
}

/// The **session ceiling** keyword back to a mode. An unknown or absent keyword
/// reads as `None` — "not recorded" — so recovery falls back to the command
/// line rather than guessing a ceiling.
fn mode_of(v: Option<&Value>) -> Option<crate::ctl::TrustMode> {
    match v {
        Some(Value::String(s)) => crate::ctl::TrustMode::from_policy_keyword(s),
        _ => None,
    }
}

/// A **pane's own** posture keyword back to a mode. Same parse, opposite failure
/// direction, and the difference matters:
///
/// `None` from this field means "fall back to the session ceiling", which is the
/// most permissive posture available. So a pane recorded at `plan` whose keyword
/// is corrupt — or renamed by a future version — would come back at `automode`,
/// silently promoted by damaged data. Absent stays "not recorded"; **present but
/// unreadable fails closed to `Off`**, where atrium relaxes nothing and claude
/// prompts as it normally would. Wrong-but-safe, matching `can_spawn`'s v1
/// default rather than contradicting the field's own promise that a snapshot can
/// only ever lower a posture.
fn pane_mode_of(v: Option<&Value>) -> Option<crate::ctl::TrustMode> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(
            crate::ctl::TrustMode::from_policy_keyword(s).unwrap_or(crate::ctl::TrustMode::Off),
        ),
        _ => Some(crate::ctl::TrustMode::Off),
    }
}

fn pane_from_json(v: &Value) -> Option<PaneRecord> {
    let o = obj_of(v)?;
    let id = get(o, "id").and_then(|v| match v {
        Value::Number(Number::Int(n)) => Some(*n as usize),
        _ => None,
    })?;
    let role = get(o, "role").and_then(opt_str_from)?;
    let argv = get(o, "argv").and_then(|v| match v {
        Value::Array(arr) => arr
            .iter()
            .map(|a| match a {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    })?;
    let cwd = get(o, "cwd").and_then(opt_str_from)?;
    let identity = get(o, "identity").and_then(opt_str_from)?;
    let session_id = get(o, "session_id").and_then(opt_str_from)?;
    let worktree = get(o, "worktree").and_then(opt_str_from)?;
    // v1 snapshots carry no `can_spawn`. Default it from what the pane *was*: a
    // pane with a role is a fleet agent or a ctl-spawned worker, and the roster
    // default for those is false; a pane with no role is one the human opened,
    // which is where the spawn path's `true` belongs. Guessing `true` for every
    // v1 pane would hand the capability to agents that never had it.
    let can_spawn = bool_or(get(o, "can_spawn"), role.is_none());
    Some(PaneRecord {
        id,
        role,
        argv,
        cwd,
        identity,
        session_id,
        worktree,
        deny: deny_list(get(o, "deny"))?,
        can_spawn,
        depth: usize_or(get(o, "depth"), 0),
        parent_pane: opt_usize(get(o, "parent_pane")),
        mode: pane_mode_of(get(o, "mode")),
    })
}

/// The policy block. Three outcomes, and keeping them apart is the point:
///
/// * absent or `null` -> `Some(None)`: a v1 snapshot, or one written without a
///   policy. Recovery falls back to the command line and says so.
/// * a readable object -> `Some(Some(policy))`.
/// * present but unreadable -> `None`, which fails the whole load.
///
/// Collapsing the third case into the first is the subtle version of the bug
/// this schema exists to fix: a corrupted policy block would read as "this
/// session had no guards" and the recovery would look entirely normal while
/// running with the fleet's deny list, compile pool and memory cap gone.
fn policy_from_json(v: Option<&Value>) -> Option<Option<PolicyRecord>> {
    let v = match v {
        None | Some(Value::Null) => return Some(None),
        Some(v) => v,
    };
    let o = obj_of(v)?;
    Some(Some(PolicyRecord {
        deny: deny_list(get(o, "deny"))?,
        build_jobs: opt_usize(get(o, "build_jobs")),
        memory_mb: opt_u64(get(o, "memory_mb")),
        trust: mode_of(get(o, "trust")),
        allow_ctl: bool_or(get(o, "allow_ctl"), false),
        max_depth: usize_or(get(o, "max_depth"), crate::ctl::DEFAULT_MAX_DEPTH),
    }))
}

fn snapshot_from_json(v: &Value) -> Option<Snapshot> {
    let o = obj_of(v)?;
    let version = get(o, "version").and_then(|v| match v {
        Value::Number(Number::Int(n)) => Some(*n as u32),
        _ => None,
    })?;
    let layout_o = get(o, "layout").and_then(obj_of)?;
    let ids = get(layout_o, "ids").and_then(|v| match v {
        Value::Array(arr) => arr
            .iter()
            .map(|a| match a {
                Value::Number(Number::Int(n)) => Some(*n as usize),
                _ => None,
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    })?;
    let focus = get(layout_o, "focus").and_then(|v| match v {
        Value::Number(Number::Int(n)) => Some(*n as usize),
        _ => None,
    })?;
    let panes = get(o, "panes").and_then(|v| match v {
        Value::Array(arr) => arr.iter().map(pane_from_json).collect::<Option<Vec<_>>>(),
        _ => None,
    })?;
    Some(Snapshot {
        version,
        layout: LayoutRecord { ids, focus },
        panes,
        policy: policy_from_json(get(o, "policy"))?,
        meta: meta_from_json(get(o, "meta")),
    })
}

/// The meta block, tolerantly. Every failure direction here is safe: a missing
/// project or a stale/absent timestamp only makes a session *less* likely to be
/// offered, and nothing in the meta grants authority.
fn meta_from_json(v: Option<&Value>) -> SessionMeta {
    let Some(o) = v.and_then(obj_of) else {
        return SessionMeta::default();
    };
    SessionMeta {
        project: get(o, "project").and_then(opt_str_from).flatten(),
        pid: opt_u64(get(o, "pid")).and_then(|p| u32::try_from(p).ok()),
        saved_at_ms: opt_u64(get(o, "saved_at_ms")),
        clean: bool_or(get(o, "clean"), false),
    }
}

// ---- I/O -----------------------------------------------------------------

/// Write `snap` to `path` **atomically**: serialises to a sibling `.tmp` file
/// then renames over `path`. A crash mid-write leaves the previous snapshot
/// intact — matching the atomic-write pattern in `reap::write_registry`.
///
/// Written owner-only on unix (`0600`, see [`crate::session_store::write_private`]):
/// the file decides how much authority a recovered session gets.
pub fn save(path: &Path, snap: &Snapshot) -> std::io::Result<()> {
    let body = format!("{}\n", snapshot_to_json(snap));
    crate::session_store::write_private(path, &body)
}

/// Load a snapshot from `path`. Returns a clear `Err` for a missing,
/// unreadable, non-UTF-8, malformed-JSON, or structurally-invalid file.
/// Never panics.
///
/// Two validations beyond "does it parse", because what is read here decides how
/// much authority a recovered session gets:
///
/// * **Version.** A file from a *newer* atrium is refused rather than read with
///   this version's meaning — the doc on [`VERSION`] promises a migration path,
///   and applying v2 semantics to a v3 file is how that promise quietly breaks.
/// * **Unique pane ids.** Ids address panes in the layout tree and are what the
///   parent remap resolves against; a duplicate makes the recovered spawn tree
///   silently wrong (the remap and `Window::pane` both take the first match,
///   leaving a live pty unreachable from the tree).
pub fn load(path: &Path) -> Result<Snapshot, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read snapshot {}: {e}", path.display()))?;
    let v = json::parse(text.trim())
        .map_err(|e| format!("snapshot {}: JSON parse error: {e:?}", path.display()))?;
    let snap = snapshot_from_json(&v).ok_or_else(|| {
        format!(
            "snapshot {}: unexpected schema — corrupt, or a guard field (a `deny` list) \
             it could not read. Pass the session's rules on the command line instead.",
            path.display()
        )
    })?;
    if snap.version > VERSION {
        return Err(format!(
            "snapshot {}: schema v{} was written by a newer atrium (this one reads v{VERSION}) \
             — upgrade atrium to recover this session",
            path.display(),
            snap.version
        ));
    }
    let mut seen: Vec<usize> = Vec::with_capacity(snap.panes.len());
    for p in &snap.panes {
        if seen.contains(&p.id) {
            return Err(format!(
                "snapshot {}: duplicate pane id {} — refusing to rebuild an ambiguous session",
                path.display(),
                p.id
            ));
        }
        seen.push(p.id);
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PaneCapture` with the v2 capability fields at their "human pane"
    /// values, so each test names only the fields it is about.
    fn cap(id: usize, role: Option<&str>, argv: &[&str]) -> PaneCapture {
        PaneCapture {
            id,
            role: role.map(str::to_string),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            identity: None,
            session_id: None,
            worktree: None,
            deny: Vec::new(),
            can_spawn: true,
            depth: 0,
            parent_pane: None,
            mode: None,
        }
    }

    fn sample_policy() -> PolicyRecord {
        PolicyRecord {
            deny: vec!["git push".to_string(), "cargo bench".to_string()],
            build_jobs: Some(10),
            memory_mb: Some(32768),
            trust: Some(crate::ctl::TrustMode::Auto),
            allow_ctl: true,
            max_depth: 6,
        }
    }

    fn sample_panes() -> (Vec<usize>, usize, Vec<PaneCapture>) {
        let ids = vec![0, 1, 2, 3];
        let focus = 1;
        let panes = vec![
            PaneCapture {
                id: 0,
                role: Some("lead".to_string()),
                argv: vec![
                    "claude".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                ],
                cwd: Some("/proj".to_string()),
                identity: Some("work".to_string()),
                session_id: Some("sess-abc".to_string()),
                worktree: Some("/proj/.atrium-worktrees/lead".to_string()),
                deny: vec!["git push".to_string()],
                can_spawn: true,
                depth: 0,
                parent_pane: None,
                mode: Some(crate::ctl::TrustMode::Auto),
            },
            PaneCapture {
                id: 1,
                role: None,
                argv: vec!["bash".to_string()],
                cwd: None,
                identity: None,
                session_id: None,
                worktree: None,
                deny: Vec::new(),
                can_spawn: true,
                depth: 0,
                parent_pane: None,
                mode: None,
            },
            PaneCapture {
                id: 2,
                role: Some("dev_1".to_string()),
                argv: vec!["claude".to_string()],
                cwd: Some("/proj/feature".to_string()),
                identity: None,
                session_id: Some("sess-xyz".to_string()),
                worktree: None,
                deny: vec!["cargo test --workspace".to_string()],
                can_spawn: false,
                depth: 1,
                parent_pane: Some(0),
                mode: Some(crate::ctl::TrustMode::Edits),
            },
            PaneCapture {
                id: 3,
                role: Some("dev_2".to_string()),
                argv: vec!["claude".to_string(), "--continue".to_string()],
                cwd: None,
                identity: Some("personal".to_string()),
                session_id: None,
                worktree: Some("/proj/.atrium-worktrees/dev_2".to_string()),
                deny: Vec::new(),
                can_spawn: false,
                depth: 2,
                parent_pane: Some(2),
                mode: None,
            },
        ];
        (ids, focus, panes)
    }

    #[test]
    fn capture_builds_correct_snapshot() {
        let (ids, focus, panes) = sample_panes();
        let snap = capture(ids.clone(), focus, panes, Some(sample_policy()));
        assert_eq!(snap.version, VERSION);
        assert_eq!(snap.layout.ids, ids);
        assert_eq!(snap.layout.focus, focus);
        assert_eq!(snap.panes.len(), 4);
        assert_eq!(snap.panes[0].role.as_deref(), Some("lead"));
        assert_eq!(snap.panes[1].role, None);
        assert_eq!(snap.panes[0].session_id.as_deref(), Some("sess-abc"));
        assert_eq!(snap.panes[1].identity, None);
        assert_eq!(
            snap.panes[3].worktree.as_deref(),
            Some("/proj/.atrium-worktrees/dev_2")
        );
        // The capability fields are the point of v2: they must survive capture.
        assert_eq!(snap.panes[0].deny, vec!["git push".to_string()]);
        assert!(snap.panes[0].can_spawn, "the lead could spawn");
        assert!(!snap.panes[2].can_spawn, "the worker could not");
        assert_eq!(snap.panes[2].depth, 1);
        assert_eq!(snap.panes[2].parent_pane, Some(0));
        assert_eq!(snap.panes[2].mode, Some(crate::ctl::TrustMode::Edits));
        assert_eq!(snap.policy.as_ref().map(|p| p.build_jobs), Some(Some(10)));
    }

    #[test]
    fn round_trip_save_and_load() {
        let (ids, focus, panes) = sample_panes();
        let snap = capture(ids, focus, panes, Some(sample_policy()));
        let path =
            std::env::temp_dir().join(format!("atrium_session_rt_{}.json", std::process::id()));
        save(&path, &snap).expect("save must succeed");
        let loaded = load(&path).expect("load must succeed");
        assert_eq!(snap, loaded);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_err_not_panic() {
        let path = std::env::temp_dir().join("atrium_session_no_such_file_XXXXX_test.json");
        let result = load(&path);
        assert!(result.is_err(), "missing file must return Err");
        assert!(
            !result.unwrap_err().is_empty(),
            "error message must not be empty"
        );
    }

    #[test]
    fn load_corrupt_json_returns_err_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "atrium_session_corrupt_{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"not valid json {{{").expect("test setup write");
        let result = load(&path);
        assert!(result.is_err(), "corrupt JSON must return Err");
        assert!(!result.unwrap_err().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_wrong_schema_returns_err_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "atrium_session_badschema_{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"{\"version\":1,\"oops\":true}").expect("test setup write");
        let result = load(&path);
        assert!(result.is_err(), "wrong schema must return Err");
        let _ = std::fs::remove_file(&path);
    }

    // --- v2 policy + capabilities -----------------------------------------

    /// The whole point of the v2 block: a snapshot that records the session's
    /// guards must hand them back byte-for-byte. A recovery that reads a smaller
    /// deny list than was in force silently re-allows a denied command.
    #[test]
    fn policy_round_trips_through_disk() {
        let (ids, focus, panes) = sample_panes();
        let snap = capture(ids, focus, panes, Some(sample_policy()));
        let path =
            std::env::temp_dir().join(format!("atrium_session_pol_{}.json", std::process::id()));
        save(&path, &snap).expect("save must succeed");
        let loaded = load(&path).expect("load must succeed");
        let pol = loaded.policy.expect("policy must survive the round trip");
        assert_eq!(pol.deny, sample_policy().deny);
        assert_eq!(pol.build_jobs, Some(10));
        assert_eq!(pol.memory_mb, Some(32768));
        assert_eq!(pol.trust, Some(crate::ctl::TrustMode::Auto));
        assert!(pol.allow_ctl);
        assert_eq!(pol.max_depth, 6);
        let _ = std::fs::remove_file(&path);
    }

    /// The meta block decides whether a launch offers to resume, so it must
    /// survive the disk exactly; and a v1 file must read as "no meta" rather than
    /// fail.
    #[test]
    fn meta_round_trips_and_is_empty_on_v1() {
        let (ids, focus, panes) = sample_panes();
        let mut snap = capture(ids, focus, panes, Some(sample_policy()));
        snap.meta = SessionMeta {
            project: Some("d:\\projects\\rationale".to_string()),
            pid: Some(62996),
            saved_at_ms: Some(1_757_990_000_000),
            clean: false,
        };
        let path =
            std::env::temp_dir().join(format!("atrium_session_meta_{}.json", std::process::id()));
        save(&path, &snap).expect("save must succeed");
        assert_eq!(load(&path).expect("load").meta, snap.meta);
        let v1 = br#"{"version":1,"layout":{"ids":[0],"focus":0},"panes":[
            {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null}]}"#;
        std::fs::write(&path, v1).expect("test setup write");
        assert_eq!(load(&path).expect("v1 loads").meta, SessionMeta::default());
        let _ = std::fs::remove_file(&path);
    }

    /// An unlimited depth guard (`--max-depth 0` becomes `usize::MAX`) exceeds
    /// `i64` and is clamped on write. It must still read back as effectively
    /// unlimited rather than wrapping to a small — or negative — number.
    #[test]
    fn unlimited_max_depth_survives_as_unlimited() {
        let mut pol = sample_policy();
        pol.max_depth = usize::MAX;
        let snap = capture(vec![0], 0, vec![cap(0, None, &["bash"])], Some(pol));
        let path =
            std::env::temp_dir().join(format!("atrium_session_depth_{}.json", std::process::id()));
        save(&path, &snap).expect("save must succeed");
        let loaded = load(&path).expect("load must succeed");
        assert!(
            loaded.policy.expect("policy").max_depth > 1_000_000,
            "an unlimited guard must not come back as a small cap"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A v1 snapshot — written before the policy block existed — must still
    /// open. The failure this guards against is a schema bump making every
    /// crashed session unrecoverable.
    #[test]
    fn v1_snapshot_still_loads_with_no_policy() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_v1_{}.json", std::process::id()));
        let v1 = br#"{"version":1,"layout":{"ids":[0,1],"focus":0},"panes":[
            {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null},
            {"id":1,"role":"builder","argv":["claude"],"cwd":"/p","identity":null,"session_id":"s1","worktree":null}]}"#;
        std::fs::write(&path, v1).expect("test setup write");
        let loaded = load(&path).expect("a v1 snapshot must still load");
        assert_eq!(loaded.version, 1);
        assert!(loaded.policy.is_none(), "v1 records no policy");
        assert!(loaded.panes[0].deny.is_empty());
        assert_eq!(loaded.panes[0].depth, 0);
        assert_eq!(loaded.panes[0].mode, None);
        let _ = std::fs::remove_file(&path);
    }

    /// The conservative default for a v1 pane: a pane with a role was a fleet
    /// agent or a ctl worker, whose roster default is "may not spawn". Reading
    /// those back as `can_spawn: true` is exactly the escalation v2 exists to
    /// close, so the v1 fallback must not reintroduce it.
    #[test]
    fn v1_pane_with_a_role_does_not_inherit_spawn_capability() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_v1cs_{}.json", std::process::id()));
        let v1 = br#"{"version":1,"layout":{"ids":[0,1],"focus":0},"panes":[
            {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null},
            {"id":1,"role":"builder","argv":["claude"],"cwd":null,"identity":null,"session_id":null,"worktree":null}]}"#;
        std::fs::write(&path, v1).expect("test setup write");
        let loaded = load(&path).expect("load must succeed");
        assert!(loaded.panes[0].can_spawn, "a human pane keeps the default");
        assert!(
            !loaded.panes[1].can_spawn,
            "a roled pane must not be handed the capability"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A pane's own posture must fail **closed**. `None` from this field means
    /// "use the session ceiling", so reading a corrupt or renamed keyword as
    /// `None` would promote a pane recorded at `plan` to the ceiling — damaged
    /// data handing out authority, which is the opposite of what the field
    /// promises.
    #[test]
    fn an_unreadable_pane_mode_falls_back_to_the_least_authority() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_mode_{}.json", std::process::id()));
        let body = br#"{"version":2,"layout":{"ids":[0,1,2],"focus":0},"panes":[
            {"id":0,"role":"a","argv":["claude"],"cwd":null,"identity":null,"session_id":null,"worktree":null,"mode":"nonsense"},
            {"id":1,"role":"b","argv":["claude"],"cwd":null,"identity":null,"session_id":null,"worktree":null,"mode":7},
            {"id":2,"role":"c","argv":["claude"],"cwd":null,"identity":null,"session_id":null,"worktree":null}]}"#;
        std::fs::write(&path, body).expect("test setup write");
        let panes = load(&path).expect("load must succeed").panes;
        assert_eq!(
            panes[0].mode,
            Some(crate::ctl::TrustMode::Off),
            "an unknown keyword must fail closed, not read as 'unrecorded'"
        );
        assert_eq!(
            panes[1].mode,
            Some(crate::ctl::TrustMode::Off),
            "a non-string mode must fail closed too"
        );
        assert_eq!(
            panes[2].mode, None,
            "an ABSENT mode is genuinely unrecorded and defers to the ceiling"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A deny list is the one field that must not degrade quietly: reading a
    /// corrupt one as `[]` silently re-allows every command the fleet denied.
    #[test]
    fn a_malformed_deny_list_fails_the_load_instead_of_reading_as_no_rules() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_deny_{}.json", std::process::id()));
        for body in [
            br#"{"version":2,"layout":{"ids":[0],"focus":0},"panes":[
                {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null}],
                "policy":{"deny":"git push"}}"#.to_vec(),
            br#"{"version":2,"layout":{"ids":[0],"focus":0},"panes":[
                {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null,"deny":[1,2]}]}"#.to_vec(),
        ] {
            std::fs::write(&path, &body).expect("test setup write");
            let err = load(&path).expect_err("a corrupt guard must fail the load");
            assert!(
                err.contains("deny"),
                "the error must name the guard it could not read: {err:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A file from a newer atrium must be refused, not read with this version's
    /// meaning — the whole point of carrying a version.
    #[test]
    fn a_newer_schema_is_refused_rather_than_misread() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_v9_{}.json", std::process::id()));
        let body = br#"{"version":9,"layout":{"ids":[0],"focus":0},"panes":[
            {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null}]}"#;
        std::fs::write(&path, body).expect("test setup write");
        let err = load(&path).expect_err("a newer schema must be refused");
        assert!(err.contains("newer atrium"), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    /// Duplicate pane ids make the recovered spawn tree ambiguous: the parent
    /// remap and the layout both resolve an id to the first match, so the second
    /// pane runs while being unreachable from the tree.
    #[test]
    fn duplicate_pane_ids_are_refused() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_dup_{}.json", std::process::id()));
        let body = br#"{"version":2,"layout":{"ids":[0,1],"focus":0},"panes":[
            {"id":1,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null},
            {"id":1,"role":"worker","argv":["claude"],"cwd":null,"identity":null,"session_id":null,"worktree":null}]}"#;
        std::fs::write(&path, body).expect("test setup write");
        let err = load(&path).expect_err("an ambiguous roster must be refused");
        assert!(err.contains("duplicate pane id"), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    /// A policy block whose keys are absent or the wrong type must degrade to
    /// documented defaults, not fail the load — a corrupt guard should cost the
    /// guard, not the whole session.
    #[test]
    fn malformed_policy_keys_fall_back_to_defaults() {
        let path =
            std::env::temp_dir().join(format!("atrium_session_badpol_{}.json", std::process::id()));
        let body = br#"{"version":2,"layout":{"ids":[0],"focus":0},"panes":[
            {"id":0,"role":null,"argv":["bash"],"cwd":null,"identity":null,"session_id":null,"worktree":null}],
            "policy":{"build_jobs":"ten","trust":"nonsense","allow_ctl":"yes","max_depth":"deep"}}"#;
        std::fs::write(&path, body).expect("test setup write");
        let pol = load(&path)
            .expect("a malformed policy must not fail the load")
            .policy
            .expect("the block is present");
        assert!(pol.deny.is_empty(), "an absent deny list is an empty one");
        assert_eq!(pol.build_jobs, None);
        assert_eq!(pol.trust, None, "an unknown keyword is 'not recorded'");
        assert!(!pol.allow_ctl);
        assert_eq!(pol.max_depth, crate::ctl::DEFAULT_MAX_DEPTH);
        let _ = std::fs::remove_file(&path);
    }
}
