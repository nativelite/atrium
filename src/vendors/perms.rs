//! What a fleet file may set about a codex pane beyond its posture: the
//! `codex` block, at fleet level (every codex pane of the session, the ones a
//! lead spawns later included) and per agent (that pane only, merged over the
//! fleet's). The posture (`trust`) still decides the sandbox and whether the
//! pane may write at all; this block refines what happens inside it, in the
//! terms codex itself has — its approval policy, its filesystem rules and its
//! network rules — and becomes the permission profile the pane launches under
//! ([`super::codex_profile`]).
//!
//! ```json
//! "codex": {
//!   "approval": "never",
//!   "git": "write",
//!   "read_only": ["../secrets"],
//!   "network": true,
//!   "domains": ["api.github.com", "*.crates.io"],
//!   "unix_sockets": ["/run/user/1000/podman.sock"]
//! }
//! ```
//!
//! Every key is optional. A key the block does not have is an error, not a
//! silent no-op: a misspelt `read_onyl` must not read as enforced.

use super::CodexFsRule;

/// codex's `--ask-for-approval` policies, as its CLI spells them.
pub const APPROVALS: [&str; 4] = ["never", "on-request", "on-failure", "untrusted"];

/// What a pane may do to its checkout's git directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAccess {
    /// The git dir and common dir writable, hooks and config read-only
    /// ([`super::codex_git_rules`]): the pane can commit. The default at
    /// `accept` and `automode`.
    Write,
    /// codex's own default: `.git` read-only, no commit from inside the sandbox.
    Read,
}

/// The `codex` block of a fleet file, before merging. `None`/empty = not set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexPerms {
    /// `approval`: one of [`APPROVALS`]. atrium's default is `never` (a refused
    /// command returns to the model); `on-request` lets codex ask in the pane,
    /// which atrium's delivery gate respects as an open dialog.
    pub approval: Option<String>,
    /// `git`: see [`GitAccess`].
    pub git: Option<GitAccess>,
    /// `read_only`: paths kept read-only inside what the pane may write.
    /// Relative paths are resolved against the fleet file's directory at
    /// launch, like `cwd` and `add_dirs`.
    pub read_only: Vec<String>,
    /// `network`: `true` opens the network through codex's proxy, every host
    /// when `domains` is empty and those hosts only otherwise; `false`, the
    /// default, keeps the sandbox's own no-network, the control socket aside.
    pub network: Option<bool>,
    /// `domains`: codex's `network.domains` allow list (`"api.github.com"`,
    /// `"*.crates.io"`). Implies nothing on its own: `network` must be `true`.
    pub domains: Vec<String>,
    /// `unix_sockets`: unix sockets the pane's commands may connect to, on top
    /// of this session's control socket.
    pub unix_sockets: Vec<String>,
}

impl CodexPerms {
    /// Is every key absent? Such a block changes nothing and is not disclosed.
    pub fn is_empty(&self) -> bool {
        *self == CodexPerms::default()
    }

    /// Parse the value of a `codex` key. `whose` names the owner in errors
    /// (`fleet "f"` or `fleet "f" agent "dev"`).
    pub fn parse(whose: &str, v: &json::Value) -> Result<CodexPerms, String> {
        let obj = v
            .as_object()
            .ok_or_else(|| format!("{whose}: \"codex\" must be an object"))?;
        let mut out = CodexPerms::default();
        for (key, val) in obj {
            match key.as_str() {
                "approval" => {
                    let word = val.as_str().ok_or_else(|| {
                        format!(
                            "{whose}: \"codex.approval\" must be one of {}",
                            APPROVALS.join(", ")
                        )
                    })?;
                    if !APPROVALS.contains(&word) {
                        return Err(format!(
                            "{whose}: \"codex.approval\" must be one of {} (got {word:?})",
                            APPROVALS.join(", ")
                        ));
                    }
                    out.approval = Some(word.to_string());
                }
                "git" => {
                    out.git = Some(match val.as_str() {
                        Some("write") => GitAccess::Write,
                        Some("read") => GitAccess::Read,
                        _ => {
                            return Err(format!(
                                "{whose}: \"codex.git\" must be \"write\" or \"read\""
                            ))
                        }
                    });
                }
                "network" => {
                    out.network = Some(val.as_bool().ok_or_else(|| {
                        format!("{whose}: \"codex.network\" must be true or false")
                    })?);
                }
                "read_only" => out.read_only = str_list(whose, "read_only", val)?,
                "domains" => out.domains = str_list(whose, "domains", val)?,
                "unix_sockets" => out.unix_sockets = str_list(whose, "unix_sockets", val)?,
                other => {
                    return Err(format!(
                        "{whose}: \"codex\" has no key {other:?} (it takes approval, git, \
                         read_only, network, domains, unix_sockets)"
                    ))
                }
            }
        }
        Ok(out)
    }

    /// The block a pane runs under: the agent's own keys over the fleet's,
    /// scalar by scalar, with the lists joined (an agent adds to the fleet's
    /// carve-outs, hosts and sockets; it cannot take them away).
    pub fn merged(fleet: Option<&CodexPerms>, agent: Option<&CodexPerms>) -> CodexPerms {
        let base = fleet.cloned().unwrap_or_default();
        let Some(agent) = agent else {
            return base;
        };
        let mut out = CodexPerms {
            approval: agent.approval.clone().or(base.approval),
            git: agent.git.or(base.git),
            network: agent.network.or(base.network),
            read_only: base.read_only,
            domains: base.domains,
            unix_sockets: base.unix_sockets,
        };
        for (into, from) in [
            (&mut out.read_only, &agent.read_only),
            (&mut out.domains, &agent.domains),
            (&mut out.unix_sockets, &agent.unix_sockets),
        ] {
            for s in from {
                if !into.contains(s) {
                    into.push(s.clone());
                }
            }
        }
        out
    }

    /// The block with every relative `read_only` path made absolute against
    /// `base`, the fleet file's directory, as `cwd` and `add_dirs` are. codex
    /// takes absolute paths only; a relative one would name nothing.
    pub fn resolved(&self, base: &std::path::Path) -> CodexPerms {
        let mut out = self.clone();
        for p in &mut out.read_only {
            let path = std::path::Path::new(p.as_str());
            if path.is_relative() {
                *p = crate::resolve::normalize_path(&base.join(path))
                    .to_string_lossy()
                    .into_owned();
            }
        }
        out
    }

    /// The approval policy the pane launches with.
    pub fn approval(&self) -> &str {
        self.approval.as_deref().unwrap_or("never")
    }

    /// Does the pane get its git dirs opened? `git` unset means yes.
    pub fn git_write(&self) -> bool {
        self.git != Some(GitAccess::Read)
    }

    /// Is the network opened (through codex's proxy)?
    pub fn network_open(&self) -> bool {
        self.network == Some(true)
    }

    /// The carve-outs as filesystem rules, after the git rules so a carve-out
    /// inside an opened git dir still wins (codex applies the deeper rule).
    pub fn read_only_rules(&self) -> Vec<CodexFsRule> {
        self.read_only
            .iter()
            .map(|p| CodexFsRule {
                path: p.clone(),
                write: false,
            })
            .collect()
    }

    /// One line for the launch banner: what this block changes from atrium's
    /// defaults, or `None` when nothing. `names` are the agents it applies to.
    pub fn disclosure(&self, names: &[String]) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(a) = &self.approval {
            if a != "never" {
                parts.push(format!("approval {a}"));
            }
        }
        if self.git == Some(GitAccess::Read) {
            parts.push("git read-only".to_string());
        }
        if self.network_open() {
            if self.domains.is_empty() {
                parts.push("network on (every host)".to_string());
            } else {
                parts.push(format!("network on ({})", self.domains.join(", ")));
            }
        }
        if !self.read_only.is_empty() {
            parts.push(format!("{} read-only path(s)", self.read_only.len()));
        }
        if !self.unix_sockets.is_empty() {
            parts.push(format!("{} extra socket(s)", self.unix_sockets.len()));
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!("codex {} — {}", names.join(", "), parts.join(", ")))
    }
}

fn str_list(whose: &str, key: &str, v: &json::Value) -> Result<Vec<String>, String> {
    let err = || format!("{whose}: \"codex.{key}\" must be an array of strings");
    v.as_array()
        .ok_or_else(err)?
        .iter()
        .map(|e| e.as_str().map(str::to_string).ok_or_else(err))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<CodexPerms, String> {
        CodexPerms::parse("fleet \"f\"", &json::parse(text).unwrap())
    }

    #[test]
    fn every_key_parses_and_an_empty_block_changes_nothing() {
        let p = parse(
            r#"{ "approval": "on-request", "git": "read", "read_only": ["../secrets"],
                 "network": true, "domains": ["api.github.com"], "unix_sockets": ["/x.sock"] }"#,
        )
        .unwrap();
        assert_eq!(p.approval(), "on-request");
        assert!(!p.git_write());
        assert_eq!(p.read_only, vec!["../secrets".to_string()]);
        assert!(p.network_open());
        assert_eq!(p.domains, vec!["api.github.com".to_string()]);
        assert_eq!(p.unix_sockets, vec!["/x.sock".to_string()]);
        let empty = parse("{}").unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.approval(), "never");
        assert!(empty.git_write());
        assert!(!empty.network_open());
        assert_eq!(empty.disclosure(&["a".into()]), None);
    }

    /// A key the block does not have, or a value of the wrong shape, is an
    /// error naming the owner and the key: a typo must not read as enforced.
    #[test]
    fn a_wrong_key_or_value_is_a_clear_error() {
        for (bad, word) in [
            (r#"{ "read_onyl": [] }"#, "read_onyl"),
            (r#"{ "approval": "always" }"#, "codex.approval"),
            (r#"{ "git": "rw" }"#, "codex.git"),
            (r#"{ "network": "yes" }"#, "codex.network"),
            (r#"{ "domains": "api.github.com" }"#, "codex.domains"),
            (r#"{ "unix_sockets": [1] }"#, "codex.unix_sockets"),
            (r#"[]"#, "must be an object"),
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains(word), "{bad}: {err}");
            assert!(err.starts_with("fleet \"f\""), "{err}");
        }
    }

    /// An agent's block overrides the fleet's scalars and adds to its lists.
    #[test]
    fn an_agent_block_overrides_scalars_and_adds_to_lists() {
        let fleet = parse(
            r#"{ "approval": "on-request", "network": true, "domains": ["a.example"],
                 "read_only": ["x"] }"#,
        )
        .unwrap();
        let agent = parse(
            r#"{ "approval": "never", "git": "read", "domains": ["b.example", "a.example"] }"#,
        )
        .unwrap();
        let m = CodexPerms::merged(Some(&fleet), Some(&agent));
        assert_eq!(m.approval(), "never");
        assert!(!m.git_write());
        assert!(m.network_open());
        assert_eq!(
            m.domains,
            vec!["a.example".to_string(), "b.example".to_string()]
        );
        assert_eq!(m.read_only, vec!["x".to_string()]);
        assert_eq!(CodexPerms::merged(Some(&fleet), None), fleet);
        assert_eq!(CodexPerms::merged(None, Some(&agent)), agent);
        assert!(CodexPerms::merged(None, None).is_empty());
    }

    /// The banner names what differs from the defaults, and nothing else.
    #[test]
    fn disclosure_names_only_what_differs_from_the_defaults() {
        let p = parse(r#"{ "approval": "never", "git": "write", "network": false }"#).unwrap();
        assert_eq!(p.disclosure(&["a".into()]), None);
        let p = parse(
            r#"{ "approval": "on-failure", "git": "read", "network": true,
                 "domains": ["api.github.com", "*.crates.io"], "read_only": ["s"],
                 "unix_sockets": ["/a", "/b"] }"#,
        )
        .unwrap();
        assert_eq!(
            p.disclosure(&["dev".into(), "qa".into()]).unwrap(),
            "codex dev, qa — approval on-failure, git read-only, \
             network on (api.github.com, *.crates.io), 1 read-only path(s), 2 extra socket(s)"
        );
        let p = parse(r#"{ "network": true }"#).unwrap();
        assert_eq!(
            p.disclosure(&["dev".into()]).unwrap(),
            "codex dev — network on (every host)"
        );
    }
}
