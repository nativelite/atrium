//! The `atrium.fleet.json` schema: the typed fleet, its agents, and the parser.

/// The whole `atrium.fleet.json`: a name → [`Fleet`] map. Order-preserving so
/// `fleet ls` lists fleets in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleets {
    /// `(name, fleet)` pairs in document order.
    pub fleets: Vec<(String, Fleet)>,
}

impl Fleets {
    /// Look a fleet up by name. `None` if there is no such fleet.
    pub fn get(&self, name: &str) -> Option<&Fleet> {
        self.fleets.iter().find(|(n, _)| n == name).map(|(_, f)| f)
    }

    /// The fleet names in file order, for `fleet ls`.
    pub fn names(&self) -> Vec<&str> {
        self.fleets.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// One named fleet: an optional layout grid, an optional default identity, and
/// its agents. A fleet always has at least one agent ([`parse`] rejects an empty
/// `agents` list).
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    /// Optional `"RxC"` grid; `None` → auto balanced-grid from the agent count.
    pub grid: Option<String>,
    /// Optional default identity applied to every agent that does not name its
    /// own. `None` → agents run under their own identity or ambient creds.
    pub identity: Option<String>,
    /// Optional trust posture for the whole fleet (`"plan"`, `"accept"`,
    /// `"automode"`, `"skip"`), used when the command line does not pass
    /// `--trust`.
    ///
    /// A fleet is exactly the case where a CLI flag is the wrong home for this:
    /// you launch it to run hands-off, so it needs a posture, and typing one
    /// every time is a flag you will eventually get wrong. Declared here it lives
    /// with the roster it applies to, is reviewable in a diff, and different
    /// fleets can differ. It remains a REQUEST, not an override — the session
    /// ceiling still caps it, so a fleet file asking for `skip` inside a `plan`
    /// session gets `plan`.
    pub trust: Option<String>,
    /// Optional: bring the control plane up for this fleet, as `--allow-ctl`
    /// does. `true` in the file is equivalent to passing the flag.
    ///
    /// A fleet whose agents coordinate — the whole reason to run one — is dead
    /// without ctl, and silently so: the panes come up, the kickoffs tell them to
    /// publish to a bus that is not there, and nothing happens. That failure has
    /// already been hit in practice. If the file is meant to be the complete,
    /// reviewable definition of a spin-up, the control plane belongs in it rather
    /// than in a flag the operator has to remember.
    pub allow_ctl: Option<bool>,
    /// Optional context-mode configuration for the whole fleet. Absent → no
    /// context injection; present → [`crate::context::parse_block`] resolves the
    /// provider and share level tolerantly (unknown values warn to stderr and
    /// degrade, never reject the file).
    pub context: Option<crate::context::ContextCfg>,
    /// Optional canonical bus-topic vocabulary. When present, the fleet DECLARES
    /// how its agents talk: the bus runs strict — publishing or subscribing to a
    /// topic outside this list is rejected, so the roster converges on one shared
    /// set instead of fragmenting into `review` / `review-gate` / `reviews`.
    /// Absent → the bus is soft-gated (a novel topic needs an explicit `--new`).
    /// Names are normalized (lowercased/trimmed) by the bus.
    pub topics: Option<Vec<String>>,
    /// Fleet-level shorthand for the per-agent [`Agent::worktree`] key: `true`
    /// means "every agent gets its **own** worktree, named after itself" — the
    /// common full-fan-out case without repeating the key on each agent. An agent
    /// that *also* names a `worktree` explicitly keeps that value (so you can fan
    /// most agents out yet still group two onto one tree). Absent/`false` → no
    /// shorthand; only agents that name a `worktree` get one.
    pub worktrees: Option<bool>,
    /// Where the per-agent worktree directories are created. Absent → the default
    /// sibling `../.atrium-worktrees/<fleet>/` beside the repo (kept out of the
    /// tree so the worktrees never show up as untracked files inside it). Set it
    /// to give two concurrent sessions on the same repo **distinct** bases so they
    /// don't collide on the same directories/branches. Resolved relative to the
    /// fleet file's directory; an absolute path is used as-is.
    pub worktree_base: Option<String>,
    /// Untracked files/dirs to **link** into each fresh worktree (`git worktree
    /// add` only checks out tracked files, so a `.env` an agent needs is missing).
    /// atrium hardlinks files / junctions dirs (no admin on Windows), copying only
    /// as a fallback. Build caches are never seeded — those belong to a per-agent
    /// env var. Default empty/opt-in: atrium seeds nothing unless told.
    pub worktree_seed: Option<Vec<String>>,
    /// Size of the session's shared compile pool ([`crate::buildpool`]): how many
    /// compiler jobs every agent's builds share in total. `0` turns the pool off.
    /// Absent → one job per core, bounded by RAM. `ATRIUM_BUILD_JOBS` still wins,
    /// because a checked-in fleet file can't know the machine it runs on.
    pub build_jobs: Option<usize>,
    /// A fixed ceiling, in MiB, on the committed memory of everything the agents
    /// run ([`crate::memguard`]). `0` turns the guard off. Absent → the dynamic
    /// ceiling that tracks the machine's free commit. A fixed value never exceeds
    /// the dynamic one, and `ATRIUM_MEMORY_MB` still wins. Windows only.
    pub memory_mb: Option<u64>,
    /// Commands no agent in the session may run — including workers a lead
    /// spawns later over ctl. Each entry is a claude permission rule
    /// (`"Bash(git push --force*)"`) or a bare command prefix
    /// (`"cargo test --workspace"`). Added to `ATRIUM_DENY` and the built-in
    /// fail-safes ([`crate::trust::deny_args`]). Claude agents only.
    pub deny: Vec<String>,
    /// Other commands that are claude — a second account's shim (`claude2`), a
    /// wrapper, a renamed install — so their panes get every claude rule (trust
    /// posture, deny list, `--session-id`, the ctl directive and worktree norms)
    /// and `ctl spawn` accepts them. Added to `ATRIUM_CLAUDE_ALIASES` for the
    /// whole session ([`crate::bind::set_claude_aliases`]).
    pub claude_aliases: Vec<String>,
    /// The agents, in file order — one pane each.
    pub agents: Vec<Agent>,
}

/// One agent in a fleet. `name` and `cmd` are required; everything else is
/// optional (absent → default). Unknown JSON fields are ignored on parse, so the
/// schema can grow without breaking older files.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Agent {
    /// The agent's label. Becomes the pane's `role`, which names its tile in the
    /// tiled border (the command stem stays the pane `title`).
    pub name: String,
    /// The command and its args, e.g. `["claude"]` or `["claude", "--continue"]`.
    pub cmd: Vec<String>,
    /// Per-agent identity; overrides the fleet default when set.
    pub identity: Option<String>,
    /// Working directory the agent is spawned in (so its `CLAUDE.md` auto-loads).
    /// Resolved relative to the fleet file's directory.
    pub cwd: Option<String>,
    /// Extra directories the agent may read (→ `--add-dir <dirs…>`). Resolved
    /// relative to the fleet file's directory.
    pub add_dirs: Vec<String>,
    /// A system-prompt suffix (→ `--append-system-prompt <prompt>`).
    pub prompt: Option<String>,
    /// The model (→ `--model <model>`).
    pub model: Option<String>,
    /// The reasoning effort (→ `--effort <effort>`).
    pub effort: Option<String>,
    /// May this agent create teammates with `atrium ctl spawn`? Defaults to
    /// **false** for a fleet agent.
    ///
    /// Spawning was never a capability an agent had or lacked - it was implied by
    /// having ctl access at all, then bounded after the fact by a depth cap, the
    /// spawn allowlist and the trust ceiling. So in a seven-agent review fleet
    /// every reviewer could create teammates; none should, and nothing said so.
    ///
    /// Declaring it per agent puts the answer in the file the human approves,
    /// rather than leaving it inferred from a depth cap three layers down. It
    /// composes with the existing checks instead of replacing them: an agent with
    /// `can_spawn` still cannot exceed the trust ceiling or the depth cap.
    pub can_spawn: Option<bool>,
    /// Per-agent trust posture (`"plan"` / `"accept"` / `"automode"` / `"skip"`),
    /// overriding the fleet/session posture for THIS agent only. The reason it
    /// exists is mixed-model fleets: `automode` is gated per model on Anthropic's
    /// side (haiku reports "this model does not have automode"), and it also
    /// carries no command allowlist — so a fleet launched `--trust automode` leaves
    /// its haiku agents unable to run commands hands-off. Setting `"trust":
    /// "accept"` on those agents gives them `acceptEdits` + the allowlist instead,
    /// which every model honors. It is a REQUEST, capped to the session ceiling
    /// like every other trust: an agent can de-escalate (accept under an automode
    /// session) but never escalate past what the human approved at launch.
    pub trust: Option<String>,
    /// Opt-in per-agent working-tree isolation, as a **group name**. A distinct
    /// value gives this agent its own `git worktree` + branch (solo isolation); a
    /// value **shared** with other agents makes them co-develop one worktree +
    /// branch (a squad on one concern); **absent** leaves the agent in the main
    /// tree — exactly today's behavior, fully backward-compatible. Nothing engages
    /// unless at least one agent sets this (or the fleet sets `worktrees: true`),
    /// so coding isolation is a layer you switch on, not a change to what atrium is.
    pub worktree: Option<String>,
    /// Commands this agent may not run, on top of the fleet's `deny` and the
    /// built-in fail-safes ([`crate::trust::deny_args`]). Each entry is a claude
    /// permission rule or a bare command prefix. Claude agents only.
    pub deny: Vec<String>,
    /// An initial **user** prompt appended as the final positional argument, so
    /// the agent starts working the moment the fleet comes up instead of waiting
    /// for the human to type. For claude this is `claude … "<kickoff>"`, which
    /// seeds an interactive session with that first message. Absent ⇒ the agent
    /// idles until prompted (the previous behavior).
    pub kickoff: Option<String>,
}

impl Agent {
    /// The **pure arg-builder**: the full launch argv for this agent, as atrium
    /// hands it to the spawn path — the base `cmd`, then the fleet extras in a
    /// fixed order. Each extra appears exactly when its field is set and is
    /// omitted when absent.
    ///
    /// `add_dirs` and `cwd` are resolved by the caller against the fleet file's
    /// directory *before* this is called, so `add_dirs` here already holds the
    /// resolved directory strings. The `--session-id` inject is NOT added here —
    /// it lives in the shared spawn path so fleet and non-fleet panes bind
    /// identically.
    ///
    /// Order: `cmd… [--add-dir d1 d2…] [--append-system-prompt P] [--model M]
    /// [--effort E]`.
    pub fn args(&self, add_dirs: &[String]) -> Vec<String> {
        let mut v = self.cmd.clone();
        if !add_dirs.is_empty() {
            v.push("--add-dir".to_string());
            for d in add_dirs {
                v.push(d.clone());
            }
        }
        if let Some(p) = &self.prompt {
            v.push("--append-system-prompt".to_string());
            v.push(p.clone());
        }
        if let Some(m) = &self.model {
            v.push("--model".to_string());
            v.push(m.clone());
        }
        if let Some(e) = &self.effort {
            v.push("--effort".to_string());
            v.push(e.clone());
        }
        // The kickoff is the trailing **positional** prompt, after every flag, so
        // the agent (claude) treats it as the first user message and starts.
        if let Some(k) = &self.kickoff {
            v.push(k.clone());
        }
        v
    }
}

/// Parse an `atrium.fleet.json` text into the typed [`Fleets`] map.
///
/// Rejects, with a clear message and *no* partial result, any of:
/// * malformed JSON (the `json` crate's line/column error is surfaced),
/// * a top-level that is not an object, or one missing the `fleets` object,
/// * a fleet whose `agents` is missing, not an array, or empty,
/// * an agent missing `name` or `cmd`, or whose `cmd` is not a non-empty array
///   of strings.
///
/// Unknown fields (top-level, per-fleet, per-agent) are ignored — forward
/// compatibility is a design requirement (§6b).
pub fn parse(text: &str) -> Result<Fleets, String> {
    let root = json::parse(text).map_err(|e| format!("malformed JSON: {e}"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| "fleet file must be a JSON object".to_string())?;
    let fleets_val = obj
        .iter()
        .rev()
        .find(|(k, _)| k == "fleets")
        .map(|(_, v)| v)
        .ok_or_else(|| "fleet file has no \"fleets\" object".to_string())?;
    let fleet_entries = fleets_val
        .as_object()
        .ok_or_else(|| "\"fleets\" must be an object of name → fleet".to_string())?;

    let mut fleets = Vec::with_capacity(fleet_entries.len());
    for (name, fval) in fleet_entries {
        let fleet = parse_fleet(name, fval)?;
        fleets.push((name.clone(), fleet));
    }
    Ok(Fleets { fleets })
}

fn parse_fleet(name: &str, val: &json::Value) -> Result<Fleet, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| format!("fleet {name:?} must be an object"))?;
    let get = |key: &str| obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);

    let grid = match get("grid") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"grid\" must be a string like \"2x2\""))?
                .to_string(),
        ),
        None => None,
    };
    let allow_ctl = match get("allow_ctl") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| format!("fleet {name:?}: \"allow_ctl\" must be true or false"))?,
        ),
        None => None,
    };
    let trust = match get("trust") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"trust\" must be a string"))?
                .to_string(),
        ),
        None => None,
    };
    let identity = match get("identity") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"identity\" must be a string"))?
                .to_string(),
        ),
        None => None,
    };

    let context = get("context").map(|v| crate::context::parse_block(name, v));

    // Optional canonical bus-topic vocabulary (declares strict-mode topics).
    let topics = match get("topics") {
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("fleet {name:?}: \"topics\" must be an array of strings"))?;
            let mut out = Vec::with_capacity(arr.len());
            for t in arr {
                out.push(
                    t.as_str()
                        .ok_or_else(|| {
                            format!("fleet {name:?}: every \"topics\" entry must be a string")
                        })?
                        .to_string(),
                );
            }
            Some(out)
        }
        None => None,
    };

    // Fleet-level worktree knobs. All opt-in; absent leaves behavior unchanged.
    let worktrees = match get("worktrees") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| format!("fleet {name:?}: \"worktrees\" must be true or false"))?,
        ),
        None => None,
    };
    let worktree_base = match get("worktree_base") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"worktree_base\" must be a string"))?
                .to_string(),
        ),
        None => None,
    };
    let worktree_seed = match get("worktree_seed") {
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                format!("fleet {name:?}: \"worktree_seed\" must be an array of strings")
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for e in arr {
                out.push(
                    e.as_str()
                        .ok_or_else(|| {
                            format!(
                                "fleet {name:?}: every \"worktree_seed\" entry must be a string"
                            )
                        })?
                        .to_string(),
                );
            }
            Some(out)
        }
        None => None,
    };

    let build_jobs = match get("build_jobs") {
        Some(v) => Some(
            v.as_i64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| {
                    format!("fleet {name:?}: \"build_jobs\" must be a whole number, 0 or more")
                })?,
        ),
        None => None,
    };
    let memory_mb = match get("memory_mb") {
        Some(v) => Some(
            v.as_i64()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| {
                    format!("fleet {name:?}: \"memory_mb\" must be a whole number, 0 or more")
                })?,
        ),
        None => None,
    };

    let deny = deny_list(get("deny"), &format!("fleet {name:?}"))?;
    let claude_aliases = str_list(
        get("claude_aliases"),
        "claude_aliases",
        &format!("fleet {name:?}"),
    )?;

    let agents_val =
        get("agents").ok_or_else(|| format!("fleet {name:?} has no \"agents\" array"))?;
    let agent_items = agents_val
        .as_array()
        .ok_or_else(|| format!("fleet {name:?}: \"agents\" must be an array"))?;
    if agent_items.is_empty() {
        return Err(format!("fleet {name:?} has zero agents"));
    }
    let mut agents = Vec::with_capacity(agent_items.len());
    for (i, av) in agent_items.iter().enumerate() {
        agents.push(parse_agent(name, i, av)?);
    }

    Ok(Fleet {
        grid,
        trust,
        allow_ctl,
        identity,
        context,
        topics,
        worktrees,
        worktree_base,
        worktree_seed,
        build_jobs,
        memory_mb,
        deny,
        claude_aliases,
        agents,
    })
}

/// A `deny` list: an array of strings, or an error naming `whose` field.
fn deny_list(v: Option<&json::Value>, whose: &str) -> Result<Vec<String>, String> {
    str_list(v, "deny", whose)
}

/// An optional array-of-strings key; absent is empty, anything else is an error
/// naming the key and its owner.
fn str_list(v: Option<&json::Value>, key: &str, whose: &str) -> Result<Vec<String>, String> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let err = || format!("{whose}: {key:?} must be an array of strings");
    v.as_array()
        .ok_or_else(err)?
        .iter()
        .map(|e| e.as_str().map(str::to_string).ok_or_else(err))
        .collect()
}

fn parse_agent(fleet: &str, idx: usize, val: &json::Value) -> Result<Agent, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| format!("fleet {fleet:?} agent #{idx}: must be an object"))?;
    let get = |key: &str| obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);

    let name = get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("fleet {fleet:?} agent #{idx}: missing string \"name\""))?
        .to_string();

    let cmd_val = get("cmd")
        .ok_or_else(|| format!("fleet {fleet:?} agent {name:?}: missing \"cmd\" array"))?;
    let cmd_items = cmd_val
        .as_array()
        .ok_or_else(|| format!("fleet {fleet:?} agent {name:?}: \"cmd\" must be an array"))?;
    let mut cmd = Vec::with_capacity(cmd_items.len());
    for c in cmd_items {
        cmd.push(
            c.as_str()
                .ok_or_else(|| {
                    format!("fleet {fleet:?} agent {name:?}: \"cmd\" entries must be strings")
                })?
                .to_string(),
        );
    }
    if cmd.is_empty() {
        return Err(format!("fleet {fleet:?} agent {name:?}: \"cmd\" is empty"));
    }

    let str_field = |key: &str| -> Result<Option<String>, String> {
        match get(key) {
            Some(v) => Ok(Some(
                v.as_str()
                    .ok_or_else(|| {
                        format!("fleet {fleet:?} agent {name:?}: {key:?} must be a string")
                    })?
                    .to_string(),
            )),
            None => Ok(None),
        }
    };

    let identity = str_field("identity")?;
    let cwd = str_field("cwd")?;
    let prompt = str_field("prompt")?;
    let model = str_field("model")?;
    let effort = str_field("effort")?;
    let can_spawn =
        match get("can_spawn") {
            Some(v) => Some(v.as_bool().ok_or_else(|| {
                format!("fleet agent {name:?}: \"can_spawn\" must be true or false")
            })?),
            None => None,
        };
    let kickoff = str_field("kickoff")?;
    let worktree = str_field("worktree")?;
    let trust = str_field("trust")?;

    let add_dirs = match get("add_dirs") {
        Some(v) => {
            let items = v.as_array().ok_or_else(|| {
                format!("fleet {fleet:?} agent {name:?}: \"add_dirs\" must be an array")
            })?;
            let mut dirs = Vec::with_capacity(items.len());
            for d in items {
                dirs.push(
                    d.as_str()
                        .ok_or_else(|| {
                            format!(
                                "fleet {fleet:?} agent {name:?}: \"add_dirs\" entries must be strings"
                            )
                        })?
                        .to_string(),
                );
            }
            dirs
        }
        None => Vec::new(),
    };

    let deny = deny_list(get("deny"), &format!("fleet {fleet:?} agent {name:?}"))?;
    Ok(Agent {
        deny,
        name,
        cmd,
        identity,
        cwd,
        add_dirs,
        prompt,
        model,
        effort,
        can_spawn,
        trust,
        worktree,
        kickoff,
    })
}

#[cfg(test)]
mod tests;
