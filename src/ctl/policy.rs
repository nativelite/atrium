//! The pure guards the server applies: who may spawn what, with which flags,
//! reaching which panes, under which identity.

use super::AgentId;
use crate::bind;

/// Why a spawn was refused. Each maps to a clear reply the caller can act on.
#[derive(Debug, Clone, PartialEq)]
pub enum SpawnDenied {
    EmptyCommand,
    NotAllowed(String),
    DepthExceeded { attempted: usize, max: usize },
}

impl SpawnDenied {
    pub fn message(&self) -> String {
        match self {
            SpawnDenied::EmptyCommand => "spawn needs a command (e.g. `-- claude`)".to_string(),
            SpawnDenied::NotAllowed(stem) => format!(
                "{stem:?} is not on the agent allowlist ({}); ctl spawns agents only \
                 (a claude wrapper is named in `claude_aliases` / ATRIUM_CLAUDE_ALIASES)",
                bind::AGENT_STEMS.join(", ")
            ),
            SpawnDenied::DepthExceeded { attempted, max } => {
                format!("spawn depth {attempted} exceeds --max-depth {max} (recursion guard)")
            }
        }
    }
}

/// The **pure** spawn guard: is this command allowed to spawn at this depth?
/// `caller_depth` is the depth of the requesting pane (a human/root pane is 0);
/// the new worker would be at `caller_depth + 1`. `extra_allow` extends the
/// built-in agent allowlist ([`bind::AGENT_STEMS`]) with operator-approved stems
/// (the `ATRIUM_CTL_ALLOW` knob). Returns the new worker's depth on success.
/// Unit-tested in isolation — this is the heart of the safety model.
pub fn evaluate_spawn(
    argv: &[String],
    caller_depth: usize,
    max_depth: usize,
    extra_allow: &[String],
) -> Result<usize, SpawnDenied> {
    let Some(first) = argv.first() else {
        return Err(SpawnDenied::EmptyCommand);
    };
    if first.is_empty() {
        return Err(SpawnDenied::EmptyCommand);
    }
    // Cross-platform stem (splits on `/` and `\` on every OS) so the allowlist
    // check holds for a Windows-authored command on macOS/Linux — see
    // `bind::command_stem`.
    let stem = bind::command_stem(first);
    let allowed = bind::is_agent_stem(&stem) || extra_allow.iter().any(|s| s == &stem);
    if !allowed {
        return Err(SpawnDenied::NotAllowed(stem));
    }
    let attempted = caller_depth + 1;
    if attempted > max_depth {
        return Err(SpawnDenied::DepthExceeded {
            attempted,
            max: max_depth,
        });
    }
    Ok(attempted)
}

/// Permission-posture flags that **atrium** owns, not the spawning agent. A hosted
/// agent running `atrium ctl spawn -- claude …` must not be able to hand its
/// teammates a stronger posture than the human chose at launch — e.g. appending
/// `--dangerously-skip-permissions` (full bypass) or `--allowedTools "Bash(*)"`
/// (auto-allow everything) when the operator launched in safe `--trust` mode.
/// atrium applies the launch posture itself (see `spawn_pane_full`), so these are
/// stripped from agent-supplied argv.
const GOVERNED_FLAGS: [&str; 3] = [
    "--dangerously-skip-permissions",
    "--permission-mode",
    "--allowedTools",
];

/// Flags a *worker-supplied* argv may carry, per vendor.
///
/// This is an **allowlist**, and that inversion is the point. The governed-flag
/// list below is a denylist of three names, and a denylist guarding a surface
/// this large fails by omission: `--allowed-tools` (claude's own documented
/// alias of `--allowedTools`) sailed through it, and so did `--mcp-config`,
/// `--plugin-dir`, `--plugin-url` and `--settings` — each of which reaches code
/// execution *outside* the tool-permission system entirely. An stdio MCP server
/// is a command claude launches at startup; a plugin carries hooks. None of
/// those names appears anywhere else in atrium, so a worker could pass one
/// verbatim in **every** mode, `plan` included.
///
/// So: name what a teammate is allowed to choose, and refuse the rest. The set
/// is deliberately small — a spawn request selects a model and a session to
/// resume, and nothing that changes what the agent is permitted to do. atrium
/// supplies the posture flags itself.
fn worker_allowed_flags(stem: &str) -> Option<&'static [&'static str]> {
    // A claude alias (`claude2`, a second account's shim named in
    // `claude_aliases`) IS claude: same flags, same posture. Checked before the
    // literal match because `is_agent_stem` already counts the alias as an
    // agent, and an agent with no policy here is refused below — which is what
    // happened to every alias-based roster once `fleet up` began vetting `cmd`.
    if crate::bind::is_claude_stem(stem) {
        // Model/effort selection and session continuation only.
        return Some(&["--model", "--effort", "--continue", "-c", "--resume", "-r"]);
    }
    match stem {
        // codex takes `--model`; its approval flags are atrium's to set (see
        // `trust::codex_trust_args`), never the caller's.
        "codex" => Some(&["--model", "-m"]),
        // Any other AGENT stem — gemini, aider, cursor-agent — is one atrium will
        // bind but has no trust posture for, so it cannot cap what the agent may
        // do. Refuse rather than guess.
        _ => None,
    }
}

/// The result of vetting a worker-supplied spawn argv.
pub enum ArgvVerdict {
    /// Safe to spawn. `stripped` names governed flags removed (surfaced in the
    /// reply's `note` — never silent).
    Ok {
        argv: Vec<String>,
        stripped: Vec<String>,
    },
    /// Refused, with the reason to hand back to the caller.
    Refused(String),
}

/// Vet a ctl-spawn argv: refuse an unknown vendor or an unlisted flag, then
/// strip the governed permission flags atrium sets itself.
///
/// Pure, so the whole policy is unit-testable without spawning anything.
pub fn vet_spawn_argv(argv: &[String]) -> ArgvVerdict {
    let Some(program) = argv.first() else {
        return ArgvVerdict::Refused("spawn needs a command".to_string());
    };
    // Match the stem the way the rest of atrium does, so a path or a `.cmd` shim
    // resolves identically here and at bind time.
    let stem = std::path::Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| program.clone());
    let stem = stem.rsplit(['/', '\\']).next().unwrap_or(&stem).to_string();

    let allowed = match worker_allowed_flags(&stem) {
        Some(a) => a,
        // Not an agent CLI at all — a shell, a build tool. There are no
        // permission flags here to escape through, and whether the command may be
        // spawned in the first place is the spawn allowlist's job, not this
        // function's. Defer to it rather than duplicating (and disagreeing with)
        // that policy here.
        None if !crate::bind::is_agent_stem(&stem) => {
            let (argv, stripped) = sanitize_spawn_argv(argv);
            return ArgvVerdict::Ok { argv, stripped };
        }
        // An agent stem atrium will bind but has no posture for: it would run
        // uncapped, so a teammate must not be able to start one.
        None => {
            return ArgvVerdict::Refused(format!(
                "refusing to spawn {stem:?}: no trust posture is defined for it, so atrium \
                 cannot cap what it may do"
            ))
        }
    };

    for a in argv.iter().skip(1) {
        if !a.starts_with('-') {
            continue; // a positional value (a prompt, a model name)
        }
        let head = a.split('=').next().unwrap_or(a);
        if GOVERNED_FLAGS.contains(&head) {
            continue; // governed: stripped below, and reported
        }
        if !allowed.contains(&head) {
            return ArgvVerdict::Refused(format!(
                "refusing to spawn {stem:?}: flag {head:?} is not one a teammate may \
                 choose (allowed: {})",
                allowed.join(", ")
            ));
        }
    }

    let (argv, stripped) = sanitize_spawn_argv(argv);
    ArgvVerdict::Ok { argv, stripped }
}

/// Strip atrium-governed permission flags (and their values) from a ctl-spawn
/// argv, returning `(cleaned, stripped)` where `stripped` names the flags removed
/// (surfaced in the spawn reply's `note` — never silent, per the guardrails).
/// `--dangerously-skip-permissions` is a bare switch; `--permission-mode` and
/// `--allowedTools` each consume the following token as their value. Both
/// `--flag value` and `--flag=value` spellings are handled. Pure.
pub fn sanitize_spawn_argv(argv: &[String]) -> (Vec<String>, Vec<String>) {
    let takes_value = |f: &str| f == "--permission-mode" || f == "--allowedTools";
    let mut cleaned = Vec::with_capacity(argv.len());
    let mut stripped: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        // Match either `--flag` or the `--flag=value` form.
        let head = a.split('=').next().unwrap_or(a);
        if let Some(&flag) = GOVERNED_FLAGS.iter().find(|&&g| g == head) {
            if !stripped.iter().any(|s| s == flag) {
                stripped.push(flag.to_string());
            }
            // Drop a *separate* value token only for `--flag value` (not
            // `--flag=value`, whose value rides along in this same arg).
            if takes_value(flag) && !a.contains('=') && i + 1 < argv.len() {
                i += 1;
            }
            i += 1;
            continue;
        }
        cleaned.push(a.clone());
        i += 1;
    }
    (cleaned, stripped)
}

/// Resolve a `send`/`status` target string to a pane's agent id. A numeric
/// target matches by agent id; otherwise it matches by role label. `candidates`
/// is `(agent_id, role)` for every live pane. Returns a clear error when the
/// target is unknown or a role is ambiguous (matches more than one pane). Pure.
pub fn resolve_target(
    target: &str,
    candidates: &[(AgentId, Option<String>)],
) -> Result<AgentId, String> {
    if let Ok(id) = target.parse::<usize>().map(AgentId) {
        if candidates.iter().any(|(cid, _)| *cid == id) {
            return Ok(id);
        }
        return Err(format!("no pane with id {id}"));
    }
    let hits: Vec<AgentId> = candidates
        .iter()
        .filter(|(_, role)| role.as_deref() == Some(target))
        .map(|(cid, _)| *cid)
        .collect();
    match hits.as_slice() {
        [] => Err(format!("no pane with id or role {target:?}")),
        [one] => Ok(*one),
        many => Err(format!(
            "role {target:?} is ambiguous ({} panes: {}); use a pane id",
            many.len(),
            many.iter()
                .map(AgentId::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Is `target` inside the subtree rooted at `root` (i.e. `root` itself or a
/// descendant of it)? `parents` maps each `agent_id` to its parent. This is the
/// **subtree-scoping guard** (Decision 3): a non-privileged caller may only
/// `send`/`status` panes in its own subtree. Pure; cycle-guarded.
pub fn in_subtree(target: AgentId, root: AgentId, parents: &[(AgentId, Option<AgentId>)]) -> bool {
    if target == root {
        return true;
    }
    let parent_of = |id: AgentId| parents.iter().find(|(i, _)| *i == id).and_then(|(_, p)| *p);
    let mut cur = target;
    // The tree is shallow (depth-capped) but guard against a malformed cycle.
    for _ in 0..4096 {
        match parent_of(cur) {
            Some(p) if p == root => return true,
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

/// The **credential-delegation guard** (design §5): which identity may a
/// `ctl spawn` run its worker under? A spawned worker (non-privileged caller)
/// may only delegate an identity it itself holds — its **own** identity, or the
/// **session/fleet default** atrium launched with — so an IC can't mint itself
/// `wif:prod` that its lead was never granted. The **operator** (human root,
/// `privileged`) is the trust root and may delegate any identity in the vault.
/// Requesting `None` (no identity) is always allowed. Pure; unit-tested.
///
/// * `requested` — the `--identity` name the spawn asked for (or `None`).
/// * `caller_identity` — the requesting pane's own identity name.
/// * `session_default` — the identity atrium itself was launched under.
pub fn delegation_allowed(
    requested: Option<&str>,
    caller_identity: Option<&str>,
    session_default: Option<&str>,
    privileged: bool,
) -> Result<(), String> {
    let Some(req) = requested else {
        return Ok(()); // no identity delegated — nothing to escalate
    };
    if privileged || Some(req) == caller_identity || Some(req) == session_default {
        return Ok(());
    }
    Err(format!(
        "identity {req:?} is not yours to delegate; a worker may only pass down \
         its own identity or the session default"
    ))
}

/// Environment knob naming a file to append the ctl audit log to as JSONL, one
/// object per request (opt-in, on top of `--allow-ctl`; design §5 "optionally
/// on-disk"). Unset ⇒ the log is kept in memory only, readable via `ctl audit`.
/// Only identity **names** are ever written — never resolved secret values.
pub const ENV_AUDIT: &str = "ATRIUM_CTL_AUDIT";

/// Environment knob (comma-separated) that extends the ctl agent allowlist
/// beyond [`bind::AGENT_STEMS`]. Opt-in on top of `--allow-ctl` and set by the
/// human who launches atrium, so it never weakens the confused-agent guard for a
/// session that did not ask for it. Empty/unset ⇒ agents-only (`{claude}`).
pub const ENV_ALLOW: &str = "ATRIUM_CTL_ALLOW";

/// The extra-allow list: the global config's `ctl_allow`, then [`ENV_ALLOW`]
/// (trimmed, empties dropped).
pub fn extra_allow_from_env() -> Vec<String> {
    let mut v = crate::config::get().ctl_allow.clone();
    v.extend(parse_allow_list(ENV_ALLOW));
    v
}

/// Parse a comma-separated allowlist from an environment variable.
///
/// Shared by [`extra_allow_from_env`] here and [`crate::trust::extra_allow_from_env`],
/// which read DIFFERENT variables gating DIFFERENT security postures — the ctl
/// spawn allowlist and claude's own tool allowlist. The two functions had
/// byte-identical bodies, and a review flagged that as duplication to merge.
///
/// Do NOT merge them. They are deliberately separate policies, and collapsing
/// them would let one variable widen the other's gate — a security bug, not a
/// cleanup. What was actually worth sharing is the PARSING, so the two cannot
/// drift on how they split, trim, or treat empties.
pub fn parse_allow_list(var: &str) -> Vec<String> {
    std::env::var(var)
        .ok()
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
