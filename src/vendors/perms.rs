//! What a fleet file may set about a codex pane beyond its posture: the
//! `codex` block, at fleet level (every codex pane of the session, the ones a
//! lead spawns later included) and per agent (that pane only, merged over the
//! fleet's). The posture (`trust`) still decides the sandbox and whether the
//! pane may write at all; this block refines what happens inside it, in the
//! terms codex itself has — its approval policy, its filesystem rules and its
//! network rules — and becomes the permission profile the pane launches under
//! ([`super::codex_profile`]). atrium strips every posture key from a codex
//! command on purpose, so this block is the one place those are set, and it
//! reaches the whole of codex's permission profile: the keys named here, and
//! `raw` for any other key of the profile, passed through as written.
//!
//! ```json
//! "codex": {
//!   "approval": "never",
//!   "git": "write",
//!   "read_only": ["../secrets"],
//!   "network": true,
//!   "domains": ["api.github.com", "*.crates.io"],
//!   "deny_domains": ["telemetry.example"],
//!   "local": true,
//!   "unix_sockets": ["/run/user/1000/podman.sock"],
//!   "raw": { "network.proxy_url": "http://127.0.0.1:3128" }
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

/// How far the network is opened, through codex's proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// Not opened: the sandbox's own no-network, the control socket aside.
    Off,
    /// Every method, every host or the `domains` listed (`network: true` or
    /// `"full"`).
    Full,
    /// codex's `limited` mode: GET/HEAD/OPTIONS over plain HTTP only, HTTPS
    /// refused (`"limited"`).
    Limited,
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
    /// `network`: `true`/`"full"`, `"limited"`, or `false` (the default).
    pub network: Option<Network>,
    /// `domains`: codex's `network.domains` allow list (`"api.github.com"`,
    /// `"*.crates.io"`). Implies nothing on its own: `network` must be open.
    pub domains: Vec<String>,
    /// `deny_domains`: hosts refused even with the network open to every host.
    pub deny_domains: Vec<String>,
    /// `local`: `true` lets the pane's commands bind local servers and reach
    /// the host's loopback directly (codex's `network.allow_local_binding`).
    pub local: Option<bool>,
    /// `unix_sockets`: unix sockets the pane's commands may connect to, on top
    /// of this session's control socket.
    pub unix_sockets: Vec<String>,
    /// `raw`: any other key of codex's permission profile, as `(key, value)`
    /// with the value already TOML, passed through as
    /// `-c permissions.<profile>.<key>=<value>` — the proxy and SOCKS settings,
    /// MITM hooks, `filesystem.glob_scan_max_depth`, whatever a codex gains.
    /// The keys atrium sets itself are refused ([`RAW_OWNED`]).
    pub raw: Vec<(String, String)>,
}

/// Profile keys atrium sets from the named fields; a `raw` entry naming one
/// (or a child of the two maps) is refused so the two cannot disagree.
pub const RAW_OWNED: [&str; 7] = [
    "extends",
    "filesystem",
    "network.enabled",
    "network.mode",
    "network.domains",
    "network.unix_sockets",
    "network.allow_local_binding",
];

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
                    out.network = Some(match (val.as_bool(), val.as_str()) {
                        (Some(true), _) | (_, Some("full")) => Network::Full,
                        (Some(false), _) | (_, Some("off")) => Network::Off,
                        (_, Some("limited")) => Network::Limited,
                        _ => {
                            return Err(format!(
                                "{whose}: \"codex.network\" must be true, false, \"full\" or \"limited\""
                            ))
                        }
                    });
                }
                "local" => {
                    out.local = Some(val.as_bool().ok_or_else(|| {
                        format!("{whose}: \"codex.local\" must be true or false")
                    })?);
                }
                "read_only" => out.read_only = str_list(whose, "read_only", val)?,
                "domains" => out.domains = str_list(whose, "domains", val)?,
                "deny_domains" => out.deny_domains = str_list(whose, "deny_domains", val)?,
                "unix_sockets" => out.unix_sockets = str_list(whose, "unix_sockets", val)?,
                "raw" => out.raw = raw_entries(whose, val)?,
                other => {
                    return Err(format!(
                        "{whose}: \"codex\" has no key {other:?} (it takes approval, git, \
                         read_only, network, domains, deny_domains, local, unix_sockets, raw)"
                    ))
                }
            }
        }
        Ok(out)
    }

    /// The block a pane runs under: the agent's own keys over the fleet's,
    /// scalar by scalar, with the lists joined (an agent adds to the fleet's
    /// carve-outs, hosts and sockets; it cannot take them away) and a `raw`
    /// key of the agent's replacing the fleet's of the same name.
    pub fn merged(fleet: Option<&CodexPerms>, agent: Option<&CodexPerms>) -> CodexPerms {
        let base = fleet.cloned().unwrap_or_default();
        let Some(agent) = agent else {
            return base;
        };
        let mut out = CodexPerms {
            approval: agent.approval.clone().or(base.approval),
            git: agent.git.or(base.git),
            network: agent.network.or(base.network),
            local: agent.local.or(base.local),
            read_only: base.read_only,
            domains: base.domains,
            deny_domains: base.deny_domains,
            unix_sockets: base.unix_sockets,
            raw: base.raw,
        };
        for (into, from) in [
            (&mut out.read_only, &agent.read_only),
            (&mut out.domains, &agent.domains),
            (&mut out.deny_domains, &agent.deny_domains),
            (&mut out.unix_sockets, &agent.unix_sockets),
        ] {
            for s in from {
                if !into.contains(s) {
                    into.push(s.clone());
                }
            }
        }
        for (k, v) in &agent.raw {
            match out.raw.iter_mut().find(|(key, _)| key == k) {
                Some(slot) => slot.1 = v.clone(),
                None => out.raw.push((k.clone(), v.clone())),
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

    /// The network mode the block opens, as codex spells it, or `None` when
    /// the network stays shut.
    pub fn network_mode(&self) -> Option<&'static str> {
        match self.network {
            Some(Network::Full) => Some("full"),
            Some(Network::Limited) => Some("limited"),
            Some(Network::Off) | None => None,
        }
    }

    /// Is the network opened (through codex's proxy)?
    pub fn network_open(&self) -> bool {
        self.network_mode().is_some()
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
        if let Some(mode) = self.network_mode() {
            let scope = if self.domains.is_empty() {
                "every host".to_string()
            } else {
                self.domains.join(", ")
            };
            let how = if mode == "limited" {
                "network limited"
            } else {
                "network on"
            };
            parts.push(format!("{how} ({scope})"));
        }
        if !self.deny_domains.is_empty() {
            parts.push(format!("{} host(s) denied", self.deny_domains.len()));
        }
        if self.local == Some(true) {
            parts.push("local binding".to_string());
        }
        if !self.read_only.is_empty() {
            parts.push(format!("{} read-only path(s)", self.read_only.len()));
        }
        if !self.unix_sockets.is_empty() {
            parts.push(format!("{} extra socket(s)", self.unix_sockets.len()));
        }
        if !self.raw.is_empty() {
            let keys: Vec<&str> = self.raw.iter().map(|(k, _)| k.as_str()).collect();
            parts.push(format!("raw {}", keys.join(", ")));
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

/// The `raw` object as `(key, toml)` pairs: each key a dotted path under the
/// profile, not one atrium owns; each value turned into TOML ([`to_toml`]).
fn raw_entries(whose: &str, v: &json::Value) -> Result<Vec<(String, String)>, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{whose}: \"codex.raw\" must be an object of profile keys"))?;
    let mut out = Vec::with_capacity(obj.len());
    for (key, val) in obj {
        let well_formed = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !key.starts_with('.')
            && !key.ends_with('.')
            && !key.contains("..");
        if !well_formed {
            return Err(format!(
                "{whose}: \"codex.raw\" key {key:?} is not a dotted profile key"
            ));
        }
        let owned = RAW_OWNED.contains(&key.as_str())
            || key.starts_with("network.domains.")
            || key.starts_with("network.unix_sockets.");
        if owned {
            return Err(format!(
                "{whose}: \"codex.raw\" key {key:?} is set by the block's own keys \
                 (approval, git, read_only, network, domains, deny_domains, local, \
                 unix_sockets); use those"
            ));
        }
        let toml = to_toml(val)
            .ok_or_else(|| format!("{whose}: \"codex.raw\" value of {key:?} must not be null"))?;
        out.push((key.clone(), toml));
    }
    Ok(out)
}

/// A JSON value as the TOML text codex's `-c key=value` parses: strings
/// quoted, numbers as they are, arrays and objects inline. `None` for null,
/// which TOML has no spelling for.
pub fn to_toml(v: &json::Value) -> Option<String> {
    Some(match v {
        json::Value::Null => return None,
        json::Value::Bool(b) => b.to_string(),
        json::Value::Number(json::Number::Int(i)) => i.to_string(),
        json::Value::Number(json::Number::Float(f)) => format!("{f:?}"),
        json::Value::String(s) => super::toml_string(s),
        json::Value::Array(items) => {
            let parts: Option<Vec<String>> = items.iter().map(to_toml).collect();
            format!("[{}]", parts?.join(","))
        }
        json::Value::Object(fields) => {
            let mut parts = Vec::with_capacity(fields.len());
            for (k, val) in fields {
                parts.push(format!("{}={}", super::toml_string(k), to_toml(val)?));
            }
            format!("{{{}}}", parts.join(","))
        }
    })
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
                 "network": true, "domains": ["api.github.com"], "deny_domains": ["t.example"],
                 "local": true, "unix_sockets": ["/x.sock"],
                 "raw": { "network.proxy_url": "http://127.0.0.1:3128", "filesystem.glob_scan_max_depth": 3 } }"#,
        )
        .unwrap();
        assert_eq!(p.approval(), "on-request");
        assert!(!p.git_write());
        assert_eq!(p.read_only, vec!["../secrets".to_string()]);
        assert_eq!(p.network_mode(), Some("full"));
        assert_eq!(p.domains, vec!["api.github.com".to_string()]);
        assert_eq!(p.deny_domains, vec!["t.example".to_string()]);
        assert_eq!(p.local, Some(true));
        assert_eq!(p.unix_sockets, vec!["/x.sock".to_string()]);
        assert_eq!(
            p.raw,
            vec![
                (
                    "network.proxy_url".to_string(),
                    "\"http://127.0.0.1:3128\"".to_string()
                ),
                (
                    "filesystem.glob_scan_max_depth".to_string(),
                    "3".to_string()
                ),
            ]
        );
        assert_eq!(
            parse(r#"{ "network": "limited" }"#).unwrap().network_mode(),
            Some("limited")
        );
        assert_eq!(
            parse(r#"{ "network": false }"#).unwrap().network_mode(),
            None
        );
        let empty = parse("{}").unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.approval(), "never");
        assert!(empty.git_write());
        assert!(!empty.network_open());
        assert_eq!(empty.disclosure(&["a".into()]), None);
    }

    /// A key the block does not have, or a value of the wrong shape, is an
    /// error naming the owner and the key: a typo must not read as enforced.
    /// A `raw` key atrium sets itself is refused too, so the two cannot differ.
    #[test]
    fn a_wrong_key_or_value_is_a_clear_error() {
        for (bad, word) in [
            (r#"{ "read_onyl": [] }"#, "read_onyl"),
            (r#"{ "approval": "always" }"#, "codex.approval"),
            (r#"{ "git": "rw" }"#, "codex.git"),
            (r#"{ "network": "yes" }"#, "codex.network"),
            (r#"{ "local": "yes" }"#, "codex.local"),
            (r#"{ "domains": "api.github.com" }"#, "codex.domains"),
            (r#"{ "deny_domains": [1] }"#, "codex.deny_domains"),
            (r#"{ "unix_sockets": [1] }"#, "codex.unix_sockets"),
            (r#"{ "raw": ["x"] }"#, "codex.raw"),
            (
                r#"{ "raw": { "network.mode": "full" } }"#,
                "set by the block's own keys",
            ),
            (
                r#"{ "raw": { "network.domains.x": "allow" } }"#,
                "set by the block's own keys",
            ),
            (
                r#"{ "raw": { "filesystem": {} } }"#,
                "set by the block's own keys",
            ),
            (r#"{ "raw": { "bad key": 1 } }"#, "not a dotted profile key"),
            (
                r#"{ "raw": { "network..x": 1 } }"#,
                "not a dotted profile key",
            ),
            (
                r#"{ "raw": { "network.proxy_url": null } }"#,
                "must not be null",
            ),
            (r#"[]"#, "must be an object"),
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains(word), "{bad}: {err}");
            assert!(err.starts_with("fleet \"f\""), "{err}");
        }
    }

    /// Raw values become the TOML codex's `-c` parses: every JSON shape but
    /// null, nested as inline arrays and tables.
    #[test]
    fn raw_values_are_spelled_as_toml() {
        let v = json::parse(
            r#"{ "s": "a\"b", "t": true, "i": 7, "f": 1.5, "a": ["x", 2],
                 "o": { "k": "v", "n": { "d": false } } }"#,
        )
        .unwrap();
        let t = |k: &str| to_toml(v.get(k).unwrap()).unwrap();
        assert_eq!(t("s"), r#""a\"b""#);
        assert_eq!(t("t"), "true");
        assert_eq!(t("i"), "7");
        assert_eq!(t("f"), "1.5");
        assert_eq!(t("a"), r#"["x",2]"#);
        assert_eq!(t("o"), r#"{"k"="v","n"={"d"=false}}"#);
        assert_eq!(to_toml(&json::Value::Null), None);
    }

    /// An agent's block overrides the fleet's scalars, adds to its lists, and
    /// replaces a raw key of the same name.
    #[test]
    fn an_agent_block_overrides_scalars_and_adds_to_lists() {
        let fleet = parse(
            r#"{ "approval": "on-request", "network": true, "domains": ["a.example"],
                 "read_only": ["x"], "raw": { "network.proxy_url": "http://a", "network.enable_socks5": true } }"#,
        )
        .unwrap();
        let agent = parse(
            r#"{ "approval": "never", "git": "read", "domains": ["b.example", "a.example"],
                 "local": true, "raw": { "network.proxy_url": "http://b" } }"#,
        )
        .unwrap();
        let m = CodexPerms::merged(Some(&fleet), Some(&agent));
        assert_eq!(m.approval(), "never");
        assert!(!m.git_write());
        assert!(m.network_open());
        assert_eq!(m.local, Some(true));
        assert_eq!(
            m.domains,
            vec!["a.example".to_string(), "b.example".to_string()]
        );
        assert_eq!(m.read_only, vec!["x".to_string()]);
        assert_eq!(
            m.raw,
            vec![
                ("network.proxy_url".to_string(), "\"http://b\"".to_string()),
                ("network.enable_socks5".to_string(), "true".to_string()),
            ]
        );
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
                 "domains": ["api.github.com", "*.crates.io"], "deny_domains": ["t.example"],
                 "local": true, "read_only": ["s"], "unix_sockets": ["/a", "/b"],
                 "raw": { "network.proxy_url": "http://a" } }"#,
        )
        .unwrap();
        assert_eq!(
            p.disclosure(&["dev".into(), "qa".into()]).unwrap(),
            "codex dev, qa — approval on-failure, git read-only, \
             network on (api.github.com, *.crates.io), 1 host(s) denied, local binding, \
             1 read-only path(s), 2 extra socket(s), raw network.proxy_url"
        );
        let p = parse(r#"{ "network": "limited" }"#).unwrap();
        assert_eq!(
            p.disclosure(&["dev".into()]).unwrap(),
            "codex dev — network limited (every host)"
        );
    }
}
