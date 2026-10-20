//! What a codex command may carry.
//!
//! claude's spawn argv is vetted against a short allowlist; codex's is the
//! other way round. A codex `cmd` may carry any flag or `-c` override, so a
//! fleet can use codex the way its author wants (`--enable <feature>`,
//! `-c model_verbosity=…`, `--search`, …), except for the ones that decide
//! what codex is permitted to do. Those are atrium's: the pane's posture comes
//! from `trust` / `--mode` and nowhere else.
//!
//! Three classes, measured against codex 0.163.0:
//!
//! - **Posture** — the sandbox, the approvals and every other way to change
//!   them (a config profile, a remote server, the hidden `--yolo` alias, the
//!   same settings as `-c` keys). Stripped from every codex command and
//!   reported, as claude's permission flags are.
//! - **Placement** — `-C/--cd` and `--add-dir` widen what the sandbox lets the
//!   agent write (`-C /` makes the whole disk the workspace). Refused, pointing
//!   at the fleet's `cwd` and `add_dirs` keys, which the banner discloses. Not
//!   stripped: a pane's recorded command carries the `--add-dir` atrium wrote
//!   from `add_dirs`, and a respawn must keep it.
//! - **Out of the sandbox** — config that makes codex itself start a program
//!   (an MCP server, `notify`, hooks), send the session or a credential to an
//!   endpoint, or loosen the user's own restrictions. Measured: an MCP server
//!   and a `notify` program both write outside a `read-only` sandbox. A human's
//!   fleet file may set these (it is their machine and their review); an
//!   agent's `ctl spawn` may not, or a sandboxed agent could start a teammate
//!   that escapes the posture the human chose.

/// Who wrote the command being vetted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    /// A fleet file's `cmd`: written and reviewed by a person.
    Fleet,
    /// A `ctl spawn` request: written by an agent.
    Agent,
}

/// Long flags (without `--`) that set the posture. `true` = takes a value.
const POSTURE_LONG: [(&str, bool); 9] = [
    ("sandbox", true),
    ("ask-for-approval", true),
    ("dangerously-bypass-approvals-and-sandbox", false),
    // A hidden alias of the one above: not in `--help`, accepted all the same.
    ("yolo", false),
    ("approve-for-me", false),
    ("dangerously-bypass-hook-trust", false),
    ("profile", true),
    ("remote", true),
    ("remote-auth-token-env", true),
];

/// Short flags that set the posture; all take a value.
const POSTURE_SHORT: [char; 3] = ['s', 'a', 'p'];

/// `-c` keys (and the tables under them) that set the posture.
/// `developer_instructions` is atrium's own channel for `prompt` and the ctl
/// directive; one passed in `cmd` would be overwritten, so it goes too.
const POSTURE_KEYS: [&str; 9] = [
    "sandbox_mode",
    "approval_policy",
    "approvals_reviewer",
    "default_permissions",
    "permissions",
    "sandbox_workspace_write",
    "profile",
    "profiles",
    "developer_instructions",
];

/// `-c` keys (and the tables under them) that reach out of the sandbox: codex
/// starts a program, sends data to an endpoint, or loosens a restriction the
/// user set. An agent may not set them.
const OUTSIDE_KEYS: [&str; 13] = [
    "mcp_servers",
    "notify",
    "hooks",
    "model_providers",
    "openai_base_url",
    "chatgpt_base_url",
    "mcp_oauth_callback_url",
    "otel",
    "desktop",
    "allow_login_shell",
    "apps",
    "computer_use",
    "browser_use",
];

/// `-c` tables whose *sub*-keys widen the sandbox while the key itself is a
/// plain switch: `features.network_proxy=true` turns the proxy on, which an
/// agent may do; `features.network_proxy.domains` lets the network out.
const OUTSIDE_SUBKEYS: [&str; 1] = ["features.network_proxy"];

/// Long flags that drop the user's own config or rules. An agent may not.
const OUTSIDE_LONG: [&str; 2] = ["ignore-user-config", "ignore-rules"];

/// One option in a codex argv: the args it spans, its name as written
/// (`--sandbox`, `-s`) and its value, inline (`--sandbox=x`, `-sx`) or the
/// next arg.
struct Opt {
    start: usize,
    span: usize,
    name: String,
    value: Option<String>,
}

/// Does this option take a value? Only the ones the rules act on, `-c`, and
/// atrium's own `--append-system-prompt` marker (a recorded command carries it,
/// and a prompt that starts `-s…` must not read as a flag) need to be known: a
/// value of any other flag is a positional to this scan, and positionals are
/// never acted on.
fn takes_value(name: &str) -> bool {
    match name.strip_prefix("--") {
        Some("config") | Some("append-system-prompt") => true,
        Some(long) => POSTURE_LONG.iter().any(|&(f, v)| v && f == long),
        None => name
            .strip_prefix('-')
            .and_then(|s| s.chars().next())
            .is_some_and(|c| c == 'c' || POSTURE_SHORT.contains(&c)),
    }
}

/// The options in `argv` (program first), up to a `--`. clap spells a value
/// four ways and codex takes all of them: `--flag v`, `--flag=v`, `-fv`, `-f=v`
/// (and `-f v`).
fn options(argv: &[String]) -> Vec<Opt> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            break;
        }
        let (name, inline) = if let Some(long) = a.strip_prefix("--") {
            match long.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_string())),
                None => (a.clone(), None),
            }
        } else if let Some(short) = a.strip_prefix('-').filter(|s| !s.is_empty()) {
            let mut chars = short.chars();
            let c = chars.next().unwrap_or_default();
            let rest = chars.as_str();
            let rest = rest.strip_prefix('=').unwrap_or(rest);
            (
                format!("-{c}"),
                (!rest.is_empty()).then(|| rest.to_string()),
            )
        } else {
            i += 1;
            continue;
        };
        let (value, span) = match inline {
            Some(v) => (Some(v), 1),
            None if takes_value(&name) && i + 1 < argv.len() => (Some(argv[i + 1].clone()), 2),
            None => (None, 1),
        };
        out.push(Opt {
            start: i,
            span,
            name,
            value,
        });
        i += span;
    }
    out
}

/// The key of a `-c key=value`, its dotted segments unquoted
/// (`mcp_servers."x".command` → `mcp_servers.x.command`).
fn config_key(value: &str) -> String {
    let key = value.split_once('=').map_or(value, |(k, _)| k).trim();
    key.split('.')
        .map(|s| s.trim().trim_matches('"').trim_matches('\''))
        .collect::<Vec<_>>()
        .join(".")
}

/// `key` is `table` or a key under it.
fn under(key: &str, table: &str) -> bool {
    key == table
        || key
            .strip_prefix(table)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// How the rules treat one option.
enum Verdict {
    Keep,
    Strip(String),
    Refuse(String),
}

fn judge(opt: &Opt, author: Author) -> Verdict {
    let name = opt.name.as_str();
    let long = name.strip_prefix("--");
    let short = name
        .strip_prefix('-')
        .filter(|_| long.is_none())
        .and_then(|s| s.chars().next());
    if matches!(long, Some("cd")) || short == Some('C') {
        return Verdict::Refuse(format!(
            "{name} moves codex's workspace, which widens what its sandbox may write: \
             set the fleet agent's \"cwd\" instead"
        ));
    }
    if long == Some("add-dir") {
        return Verdict::Refuse(
            "--add-dir widens what codex's sandbox may write: set the fleet agent's \
             \"add_dirs\" instead"
                .to_string(),
        );
    }
    if long.is_some_and(|l| POSTURE_LONG.iter().any(|&(f, _)| f == l))
        || short.is_some_and(|c| POSTURE_SHORT.contains(&c))
    {
        return Verdict::Strip(name.to_string());
    }
    if author == Author::Agent && long.is_some_and(|l| OUTSIDE_LONG.contains(&l)) {
        return Verdict::Refuse(format!(
            "{name} drops the user's own codex config or rules; a teammate may not"
        ));
    }
    if matches!(long, Some("config")) || short == Some('c') {
        let key = config_key(opt.value.as_deref().unwrap_or_default());
        if POSTURE_KEYS.iter().any(|t| under(&key, t)) {
            return Verdict::Strip(format!("-c {key}"));
        }
        let outside = OUTSIDE_KEYS.iter().any(|t| under(&key, t))
            || OUTSIDE_SUBKEYS
                .iter()
                .any(|t| key.strip_prefix(t).is_some_and(|r| r.starts_with('.')));
        if author == Author::Agent && outside {
            return Verdict::Refuse(format!(
                "-c {key} makes codex start a program, send the session somewhere or \
                 widen its sandbox, outside the posture atrium sets; a teammate may not \
                 (a fleet file may)"
            ));
        }
    }
    Verdict::Keep
}

/// Vet a codex argv: refuse what `author` may not pass, then strip the
/// posture. `Ok((argv, stripped))`, with `stripped` naming what went.
pub(super) fn vet(argv: &[String], author: Author) -> Result<(Vec<String>, Vec<String>), String> {
    for opt in options(argv) {
        if let Verdict::Refuse(why) = judge(&opt, author) {
            return Err(why);
        }
    }
    Ok(strip(argv))
}

/// `argv` without its posture options (and their values), and their names.
/// Placement and out-of-sandbox options are the vetting's to refuse, not this
/// function's to drop: a recorded command carries atrium's own `--add-dir`.
pub(super) fn strip(argv: &[String]) -> (Vec<String>, Vec<String>) {
    let mut drop = vec![false; argv.len()];
    let mut stripped: Vec<String> = Vec::new();
    for opt in options(argv) {
        if let Verdict::Strip(name) = judge(&opt, Author::Fleet) {
            drop[opt.start..opt.start + opt.span].fill(true);
            if !stripped.contains(&name) {
                stripped.push(name);
            }
        }
    }
    let kept = argv
        .iter()
        .zip(drop)
        .filter(|(_, d)| !d)
        .map(|(a, _)| a.clone())
        .collect();
    (kept, stripped)
}
