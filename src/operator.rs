//! **Operator tokens**: a credential for a control-plane caller that is not a
//! pane — a control endpoint, a bridge, a person at another shell.
//!
//! A pane authenticates with the `ATRIUM_TOKEN` atrium minted for it and is
//! scoped to the panes it spawned. An operator has no pane. It authenticates
//! with a **named token read from a file**, may use the verbs its entry in that
//! file names, across the whole session, and every request it makes — allowed
//! or refused — is audited as `operator:<name>`.
//!
//! What this module holds is the part with no session in it, so it is tested
//! without one: the file format and its refusals ([`Operators::parse`],
//! [`Operators::load`]), the constant-time match ([`Operators::authenticate`]),
//! the verb vocabulary and the allow-list ([`verb`], [`Operator::verdict`]),
//! the vetting of every field an operator sends ([`vet`]), and the two ends'
//! flag handling ([`host_startup`], [`client_token`]). The server applies them
//! in `ctl_server::dispatch`.
//!
//! The token is a secret and is treated as one here: it is read from a file,
//! never from an environment variable or an argument; the type that holds it
//! has no `Debug` that prints it; and no error this module words contains it.

use crate::ctl::{BoardOp, BusOp, Cmd};
use json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The hosting atrium's flag: `--operator-token-file <path>`.
pub const FLAG: &str = "--operator-token-file";
/// The same, from the environment of the hosting atrium. It names the **path**,
/// never the token, and [`host_startup`] removes it from atrium's environment
/// before any pane exists, so no pane inherits it.
pub const ENV_FILE: &str = "ATRIUM_OPERATOR_TOKEN_FILE";
/// The shortest token accepted. 32 characters of hex is 128 bits.
pub const MIN_TOKEN_LEN: usize = 32;
/// The longest token accepted; also the longest one a request may present.
pub const MAX_TOKEN_LEN: usize = 512;
/// The audit label of a request whose operator token matched no entry. `?` is
/// not a character an operator name may hold, so no entry can wear it.
pub const UNKNOWN_LABEL: &str = "operator:?";
/// What every operator's label starts with; a pane role may not ([`role_reserved`]).
pub const LABEL_PREFIX: &str = "operator:";

/// A token file larger than this is not a token file.
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_OPERATORS: usize = 64;
const MAX_NAME_LEN: usize = 32;
/// A token with fewer distinct characters than this is a placeholder, not a
/// secret (`0000…`, `xxxx…`).
const MIN_DISTINCT: usize = 8;

/// Every verb an entry's `verbs` may be checked against: the control plane's
/// commands, with `board` and `bus` split by operation. [`verb`] maps a request
/// onto one of these.
pub const VERBS: &[&str] = &[
    "spawn",
    "list",
    "send",
    "status",
    "kill",
    "respawn",
    "audit",
    "board set",
    "board get",
    "board list",
    "board del",
    "board claim",
    "board release",
    "bus pub",
    "bus sub",
    "bus unsub",
    "bus feed",
    "bus resolve",
    "bus topics",
    "hello",
    "report",
    "whoami",
    "answer",
    "inbox",
    "ask",
    "asked",
    "who",
];

/// The verbs whose reply carries a model's words or the paths it edited. Like
/// every verb they are off unless the entry names them; they are listed apart
/// so the docs, the startup record and the tests can say which grants hand an
/// operator model output.
pub const MODEL_OUTPUT: &[&str] = &["answer", "asked", "who"];

/// The verbs that are about the calling pane's own identity, each with why an
/// operator cannot use it. Naming one in the token file is a load error, and a
/// request for one is refused with the same reason.
const PANE_ONLY: &[(&str, &str)] = &[
    (
        "hello",
        "a mod announces the pane it runs in, and an operator is not a pane",
    ),
    (
        "report",
        "a report is a pane's own status, and an operator has none",
    ),
    (
        "whoami",
        "it describes the calling pane, and an operator is not a pane",
    ),
    (
        "inbox",
        "it hands a pane its queued deliveries, and nothing is queued for an operator",
    ),
    (
        "bus sub",
        "a subscription wakes the subscribing pane, and an operator has none (read with bus feed)",
    ),
    ("bus unsub", "an operator holds no subscriptions"),
];

/// Why `verb` is a pane's own, if it is.
pub fn pane_only(verb: &str) -> Option<&'static str> {
    PANE_ONLY
        .iter()
        .find(|(v, _)| *v == verb)
        .map(|(_, why)| *why)
}

/// The verb a request is, in the vocabulary of [`VERBS`]. `board` and `bus`
/// are split by operation; every other command is its audit action, which is
/// its name on the command line.
pub fn verb(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Board(op) => match op {
            BoardOp::Set { .. } => "board set",
            BoardOp::Get { .. } => "board get",
            BoardOp::List => "board list",
            BoardOp::Del { .. } => "board del",
            BoardOp::Claim { .. } => "board claim",
            BoardOp::Release { .. } => "board release",
        },
        Cmd::Bus(op) => match op {
            BusOp::Pub { .. } => "bus pub",
            BusOp::Sub { .. } => "bus sub",
            BusOp::Unsub { .. } => "bus unsub",
            BusOp::Feed { .. } => "bus feed",
            BusOp::Resolve { .. } => "bus resolve",
            BusOp::Topics => "bus topics",
        },
        other => other.audit_label().0,
    }
}

/// One entry of the token file.
pub struct Operator {
    name: String,
    token: String,
    verbs: Vec<String>,
}

/// Never the token: an `Operator` in a `{:?}` shows its name and verbs.
impl std::fmt::Debug for Operator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operator")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("verbs", &self.verbs)
            .finish()
    }
}

/// What an operator's entry says about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The entry names the verb.
    Listed,
    /// The entry does not name it, but it is one of the reads the endpoint
    /// serves to any caller: served exactly as it is to a caller with no token.
    OpenRead,
    /// Refused, with the reason for the reply and the audit log.
    Refused(String),
}

impl Operator {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `operator:<name>`: what the audit log, the board and the bus call it.
    pub fn label(&self) -> String {
        format!("{LABEL_PREFIX}{}", self.name)
    }

    pub fn verbs(&self) -> &[String] {
        &self.verbs
    }

    /// May this operator make this request? A pane's own verb is refused
    /// whatever the entry says; a named verb is allowed; an unnamed read is
    /// served as it is to anyone; anything else is refused.
    pub fn verdict(&self, cmd: &Cmd) -> Verdict {
        let verb = verb(cmd);
        if let Some(why) = pane_only(verb) {
            return Verdict::Refused(format!(
                "operator {:?} may not use {verb:?}: {why}",
                self.name
            ));
        }
        if self.verbs.iter().any(|v| v == verb) {
            return Verdict::Listed;
        }
        if cmd.is_read_only() {
            return Verdict::OpenRead;
        }
        Verdict::Refused(format!(
            "operator {:?} may not use {verb:?}: its entry in the operator token file does not \
             name it",
            self.name
        ))
    }
}

/// The operators of a session: the parsed token file, or none.
#[derive(Debug, Default)]
pub struct Operators {
    entries: Vec<Operator>,
}

static EMPTY: Operators = Operators {
    entries: Vec::new(),
};
static SESSION: OnceLock<Operators> = OnceLock::new();

/// The operators the hosting atrium loaded at startup; none when it was given
/// no token file.
pub fn session() -> &'static Operators {
    SESSION.get().unwrap_or(&EMPTY)
}

impl Operators {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Operator> {
        self.entries.iter()
    }

    /// The operator whose token is `presented`, compared in constant time and
    /// against every entry, so neither how much of a token matched nor which
    /// entry it was nearest shows in the time taken. A pane's `ATRIUM_TOKEN`
    /// is not looked at here and an operator token is not looked at when panes
    /// are matched: the two are separate namespaces, in separate request fields.
    pub fn authenticate(&self, presented: &str) -> Option<&Operator> {
        if presented.is_empty() || presented.len() > MAX_TOKEN_LEN {
            return None;
        }
        let mut found = None;
        for op in &self.entries {
            if ct_eq(presented.as_bytes(), op.token.as_bytes()) {
                found = Some(op);
            }
        }
        found
    }

    /// Read and check the token file at `path`. Refused, with the reason: a
    /// path that is not a regular file; on unix a file some other user owns or
    /// that its group or anyone else can read or write; a file over 64 KiB or
    /// not UTF-8; and everything [`Operators::parse`] refuses.
    pub fn load(path: &Path) -> Result<Operators, String> {
        let text = read_secret_file(path)?;
        Operators::parse(&text).map_err(|e| file_err(path, &e))
    }

    /// Parse the token file's text:
    ///
    /// ```json
    /// {"operators": [
    ///   {"name": "bridge", "token": "<32+ characters>", "verbs": ["send", "bus pub"]}
    /// ]}
    /// ```
    ///
    /// Strict, because a typo here is a hole or a lockout: an unknown or
    /// repeated key, a missing one, a name outside `[A-Za-z0-9_-]{1,32}`, a
    /// token under [`MIN_TOKEN_LEN`] characters or holding anything but
    /// `A-Z a-z 0-9 . _ ~ + / = -`, an unknown verb, a pane's own verb, a name
    /// or a token used twice, and an empty list are all errors. No error text
    /// contains a token.
    pub fn parse(text: &str) -> Result<Operators, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        // The parser's error is a line, a column and a fixed phrase: nothing
        // of the file's content.
        let v = json::parse(text).map_err(|e| format!("not valid JSON ({e})"))?;
        let shape = "the file is one JSON object: {\"operators\": [{\"name\": ..., \
                     \"token\": ..., \"verbs\": [...]}]}";
        let top = v.as_object().ok_or_else(|| shape.to_string())?;
        if top.len() != 1 || top[0].0 != "operators" {
            return Err(format!(
                "{shape} (found {} top-level key(s); only \"operators\" is allowed, once)",
                top.len()
            ));
        }
        let items = top[0].1.as_array().ok_or_else(|| shape.to_string())?;
        if items.is_empty() {
            return Err("\"operators\" is empty: name at least one operator".to_string());
        }
        if items.len() > MAX_OPERATORS {
            return Err(format!(
                "{} operators; at most {MAX_OPERATORS}",
                items.len()
            ));
        }
        let mut entries: Vec<Operator> = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let op = parse_entry(item).map_err(|e| format!("operator #{}: {e}", i + 1))?;
            if entries.iter().any(|have| have.name == op.name) {
                return Err(format!("operator name {:?} appears twice", op.name));
            }
            if let Some(twin) = entries.iter().find(|have| have.token == op.token) {
                return Err(format!(
                    "operators {:?} and {:?} share one token; each needs its own",
                    twin.name, op.name
                ));
            }
            entries.push(op);
        }
        Ok(Operators { entries })
    }
}

/// One entry: exactly `name`, `token` and `verbs`, each once.
fn parse_entry(item: &Value) -> Result<Operator, String> {
    let keys = "an entry has exactly \"name\", \"token\" and \"verbs\"";
    let members = item
        .as_object()
        .ok_or_else(|| format!("not an object; {keys}"))?;
    let field = |key: &str| -> Result<&Value, String> {
        let mut hits = members.iter().filter(|(k, _)| k == key);
        match (hits.next(), hits.next()) {
            (Some((_, v)), None) => Ok(v),
            (None, _) => Err(format!("\"{key}\" is missing; {keys}")),
            (Some(_), Some(_)) => Err(format!("\"{key}\" is given twice")),
        }
    };
    let name = field("name")?
        .as_str()
        .ok_or_else(|| "\"name\" must be a string".to_string())?;
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "\"name\" must be 1 to {MAX_NAME_LEN} of A-Z a-z 0-9 _ -"
        ));
    }
    let token = field("token")?
        .as_str()
        .ok_or_else(|| format!("{name:?}: \"token\" must be a string"))?;
    check_token(token).map_err(|why| format!("{name:?}: the token {why}"))?;
    let listed = field("verbs")?
        .as_array()
        .ok_or_else(|| format!("{name:?}: \"verbs\" must be an array of strings"))?;
    // After the three known keys are each found once, any member left over is
    // a key that is not one of them. Its name is not echoed: a misplaced
    // token must not be printed back.
    if members.len() != 3 {
        return Err(format!("{name:?}: unknown key; {keys}"));
    }
    let mut verbs: Vec<String> = Vec::with_capacity(listed.len());
    for v in listed {
        let v = v
            .as_str()
            .ok_or_else(|| format!("{name:?}: \"verbs\" must be an array of strings"))?;
        if let Some(why) = pane_only(v) {
            return Err(format!(
                "{name:?}: verb {v:?} cannot be granted to an operator: {why}"
            ));
        }
        if !VERBS.contains(&v) {
            return Err(format!(
                "{name:?}: unknown verb {} (the verbs are: {})",
                quote_short(v),
                grantable().join(", ")
            ));
        }
        if verbs.iter().any(|have| have == v) {
            return Err(format!("{name:?}: verb {v:?} is named twice"));
        }
        verbs.push(v.to_string());
    }
    Ok(Operator {
        name: name.to_string(),
        token: token.to_string(),
        verbs,
    })
}

/// The verbs an entry may name: all of [`VERBS`] but a pane's own.
pub fn grantable() -> Vec<&'static str> {
    VERBS
        .iter()
        .copied()
        .filter(|v| pane_only(v).is_none())
        .collect()
}

/// `s` quoted, unless it is long enough to be a token someone put in the wrong
/// place: then only its length.
fn quote_short(s: &str) -> String {
    if s.chars().count() < MIN_TOKEN_LEN {
        format!("{s:?}")
    } else {
        format!("<{} characters>", s.chars().count())
    }
}

/// Is `token` one a file may hold? The reason reads after "the token".
fn check_token(token: &str) -> Result<(), String> {
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._~+/=-".contains(c))
    {
        return Err(
            "may hold only A-Z a-z 0-9 and . _ ~ + / = - (hex and base64 both fit)".to_string(),
        );
    }
    if token.len() < MIN_TOKEN_LEN {
        return Err(format!(
            "is {} characters; the minimum is {MIN_TOKEN_LEN} (try: openssl rand -hex 32)",
            token.len()
        ));
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err(format!(
            "is {} characters; the maximum is {MAX_TOKEN_LEN}",
            token.len()
        ));
    }
    let mut seen: Vec<char> = Vec::new();
    for c in token.chars() {
        if !seen.contains(&c) {
            seen.push(c);
        }
    }
    if seen.len() < MIN_DISTINCT {
        return Err(format!(
            "repeats fewer than {MIN_DISTINCT} distinct characters: a placeholder, not a secret \
             (try: openssl rand -hex 32)"
        ));
    }
    Ok(())
}

/// Constant-time equality of a presented token with a stored one. The time
/// taken depends on the presented length alone, which the presenter chose: the
/// loop runs over the presented bytes, reading the stored ones cyclically, and
/// a length difference is folded into the same accumulator as a byte
/// difference. `black_box` keeps the compiler from turning the fold back into
/// an early exit.
fn ct_eq(presented: &[u8], stored: &[u8]) -> bool {
    if stored.is_empty() {
        return false;
    }
    let mut diff: usize = presented.len() ^ stored.len();
    for (i, b) in presented.iter().enumerate() {
        diff |= usize::from(b ^ stored[i % stored.len()]);
    }
    std::hint::black_box(diff) == 0
}

fn file_err(path: &Path, why: &str) -> String {
    format!("operator token file {}: {why}", path.display())
}

/// Read a token file whole, after checking it is one this user alone can read.
/// The checks are made on the opened file, so the file checked is the file read.
fn read_secret_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    // Before the open: opening a FIFO would wait for its writer.
    let pre = std::fs::metadata(path).map_err(|e| file_err(path, &e.to_string()))?;
    if !pre.is_file() {
        return Err(file_err(path, "is not a regular file"));
    }
    let file = std::fs::File::open(path).map_err(|e| file_err(path, &e.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|e| file_err(path, &e.to_string()))?;
    if !meta.is_file() {
        return Err(file_err(path, "is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        check_unix_perms(meta.mode(), meta.uid(), euid()).map_err(|why| file_err(path, &why))?;
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(file_err(path, "is larger than 64 KiB"));
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| {
            let why = if e.kind() == std::io::ErrorKind::InvalidData {
                "is not UTF-8 text".to_string()
            } else {
                e.to_string()
            };
            file_err(path, &why)
        })?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(file_err(path, "is larger than 64 KiB"));
    }
    Ok(text)
}

/// The unix rule for a token file, on the numbers `fstat` gave: owned by the
/// user atrium runs as, and no permission bit for the group or anyone else.
/// Pure, so it is tested on every host; only reading the numbers is unix code.
pub fn check_unix_perms(mode: u32, owner: u32, me: u32) -> Result<(), String> {
    if owner != me {
        return Err(format!(
            "is owned by uid {owner}, not by the user atrium runs as (uid {me})"
        ));
    }
    if mode & 0o077 != 0 {
        return Err(format!(
            "is open to its group or to others (mode {:03o}); run: chmod 600 on it",
            mode & 0o777
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn euid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { geteuid() }
}

/// The token an `atrium ctl --operator-token-file <path> [--operator <name>]`
/// sends. The file is either the hosting atrium's own (the JSON above; the
/// entry is picked by `name`, or is the only one) or a single token on one
/// line, which is what a client that should know one token is given. The same
/// ownership and permission rule as the host's file applies.
pub fn client_token(path: &Path, name: Option<&str>) -> Result<String, String> {
    let text = read_secret_file(path)?;
    pick_client_token(&text, name).map_err(|e| file_err(path, &e))
}

/// [`client_token`] on the file's text.
pub fn pick_client_token(text: &str, name: Option<&str>) -> Result<String, String> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text).trim();
    if !body.starts_with('{') {
        // The single-token form. A token cannot start with `{`, so the two
        // forms cannot be mistaken for each other.
        if name.is_some() {
            return Err("holds a single token, which has no name: drop --operator".to_string());
        }
        check_token(body).map_err(|why| format!("the token {why}"))?;
        return Ok(body.to_string());
    }
    let ops = Operators::parse(body)?;
    let names = || {
        ops.entries
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let pick = match name {
        Some(n) => ops.entries.iter().find(|o| o.name == n).ok_or_else(|| {
            format!(
                "names no operator {} (it names: {})",
                quote_short(n),
                names()
            )
        })?,
        None => match ops.entries.as_slice() {
            [one] => one,
            _ => {
                return Err(format!(
                    "names {} operators ({}); pick one with --operator <name>",
                    ops.entries.len(),
                    names()
                ))
            }
        },
    };
    Ok(pick.token.clone())
}

/// A pane role an operator's label could be mistaken for. The board and the
/// bus show who wrote an entry by label, and a pane's label is its role, so a
/// pane spawned as `operator:bridge` would sign its posts as that operator.
/// (The audit log is not fooled either way: it records the authenticated
/// caller, not a label.)
pub fn role_reserved(role: &str) -> bool {
    role.to_ascii_lowercase().starts_with(LABEL_PREFIX)
}

// ---- vetting what an operator sends ---------------------------------------

/// Vet every field of a request an operator sent. A pane's request is written
/// by an agent atrium itself launched, on the same machine; an operator is, in
/// the deployments this exists for, reachable over a network. So each string
/// is bounded, free of control and display-reordering characters, and — where
/// it becomes a path component or part of a system prompt — a plain word.
/// `send` and `ask` text may span lines; nothing else may.
pub fn vet(cmd: &Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Spawn(sp) => {
            if !sp.new_window {
                return Err(
                    "an operator has no pane to tile beside: spawn into a window (no --here)"
                        .to_string(),
                );
            }
            if let Some(role) = &sp.role {
                word("role", role)?;
            }
            if let Some(identity) = &sp.identity {
                plain("identity", identity, 128)?;
            }
            if let Some(worktree) = &sp.worktree {
                word("worktree", worktree)?;
            }
            if sp.argv.len() > 64 {
                return Err("argv has more than 64 arguments".to_string());
            }
            for a in &sp.argv {
                plain("argv", a, 4096)?;
            }
            Ok(())
        }
        Cmd::Send(sr) => {
            plain("target", &sr.target, 128)?;
            lines("text", &sr.text)
        }
        Cmd::Ask(a) => {
            plain("target", &a.target, 128)?;
            lines("text", &a.text)
        }
        Cmd::Status(sr) => match &sr.target {
            Some(t) => plain("target", t, 128),
            None => Ok(()),
        },
        Cmd::Kill(kr) => plain("target", &kr.target, 128),
        Cmd::Answer(a) => plain("target", &a.target, 128),
        Cmd::Asked(a) => plain("target", &a.target, 128),
        Cmd::Respawn(rr) => {
            plain("target", &rr.target, 128)?;
            match &rr.worktree {
                Some(w) => word("worktree", w),
                None => Ok(()),
            }
        }
        Cmd::Who(w) => plain("path", &w.path, 4096),
        Cmd::Board(op) => match op {
            BoardOp::Set { key, fields } => {
                plain("key", key, 128)?;
                pairs(fields)
            }
            BoardOp::Get { key }
            | BoardOp::Del { key }
            | BoardOp::Claim { key, .. }
            | BoardOp::Release { key } => plain("key", key, 128),
            BoardOp::List => Ok(()),
        },
        Cmd::Bus(op) => match op {
            BusOp::Pub { topic, fields, .. } => {
                plain("topic", topic, 128)?;
                pairs(fields)
            }
            BusOp::Sub { topics } | BusOp::Unsub { topics } => {
                if topics.len() > 64 {
                    return Err("more than 64 topics".to_string());
                }
                topics.iter().try_for_each(|t| plain("topic", t, 128))
            }
            BusOp::Feed { .. } | BusOp::Resolve { .. } | BusOp::Topics => Ok(()),
        },
        // A pane's own verb, refused for an operator whatever it holds; what
        // is vetted is what its audit label would copy into the log.
        Cmd::Hello(h) => {
            plain("mod", &h.version, 64)?;
            if h.caps.len() > 32 {
                return Err("more than 32 caps".to_string());
            }
            h.caps.iter().try_for_each(|c| plain("a cap", c, 64))
        }
        // No text of the caller's reaches a handler or the log from these: a
        // `report` is refused before it is applied, and its label holds only
        // a status word, a number and a length.
        Cmd::List | Cmd::Audit(_) | Cmd::Whoami | Cmd::Inbox(_) | Cmd::Report(_) => Ok(()),
        // A verb added after this was written has not been read for what it
        // carries. Refused until it is added above.
        #[allow(
            unreachable_patterns,
            reason = "every verb is listed today; this arm is for the next one"
        )]
        _ => Err("this verb is not open to operators".to_string()),
    }
}

/// Does `s` hold a character that can end a line, drive a terminal, or
/// reorder or hide what a line shows? One definition, the mod reports' own.
fn has_unsafe(s: &str) -> bool {
    crate::modstate::text_safe(s) != s
}

/// A single-line value: non-empty, bounded, nothing [`has_unsafe`].
fn plain(what: &str, s: &str, max: usize) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if s.chars().count() > max {
        return Err(format!("{what} is longer than {max} characters"));
    }
    if has_unsafe(s) {
        return Err(format!("{what} holds a control or direction character"));
    }
    Ok(())
}

/// Text that is typed into a pane (`send`, `ask`): as [`plain`], but it may
/// span lines and hold tabs. Escape, Ctrl+C and the bracketed-paste markers
/// are control characters and are refused.
fn lines(what: &str, s: &str) -> Result<(), String> {
    const MAX: usize = 64 * 1024;
    if s.chars().count() > MAX {
        return Err(format!("{what} is longer than {MAX} characters"));
    }
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    if has_unsafe(&flat) {
        return Err(format!(
            "{what} holds a control or direction character (newlines and tabs are fine)"
        ));
    }
    Ok(())
}

/// A name that becomes a path component, a git ref, or a line of a pane's
/// system prompt (a role, a worktree): `[A-Za-z0-9._-]{1,64}`, starting with
/// a letter, a digit or `_`.
fn word(what: &str, s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !s.starts_with(['.', '-']);
    if !ok {
        return Err(format!(
            "{what} must be 1 to 64 of A-Z a-z 0-9 . _ - and not start with . or -"
        ));
    }
    Ok(())
}

/// A board entry's or a bus event's fields.
fn pairs(fields: &[(String, String)]) -> Result<(), String> {
    if fields.len() > 64 {
        return Err("more than 64 fields".to_string());
    }
    for (k, v) in fields {
        plain("a field name", k, 64)?;
        // An empty value clears a board field; it is the one empty string
        // that means something.
        if !v.is_empty() {
            plain("a field value", v, 4096)?;
        }
    }
    Ok(())
}

// ---- the hosting atrium's startup -----------------------------------------

/// Is `arg` the host flag, in either spelling?
pub fn is_flag(arg: &str) -> bool {
    arg == FLAG || arg.strip_prefix(FLAG).is_some_and(|r| r.starts_with('='))
}

/// What the flag must be told when it is found where it is not parsed.
pub const MISPLACED: &str = "--operator-token-file goes first: \
     atrium --operator-token-file <path> [session flags] [command] \
     (or set ATRIUM_OPERATOR_TOKEN_FILE to the path)";

/// Take [`ENV_FILE`] out of this process's environment and return what it
/// held. Every pane atrium spawns inherits atrium's environment, so the
/// variable is gone before the first one exists. This hides the *path* from a
/// pane's environment; it does not hide the file from a pane that runs as the
/// same user and can guess or find the path. See docs/control-plane.md.
fn take_env() -> Option<PathBuf> {
    let v = std::env::var_os(ENV_FILE);
    std::env::remove_var(ENV_FILE);
    v.filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// Pull `--operator-token-file <path>` (or `=<path>`) out of `args`. For
/// `fleet` and `recover`, whose every argument is atrium's own, it may sit
/// anywhere; for a plain launch only first, because past atrium's own flags
/// the arguments belong to the hosted command.
fn strip_flag(args: Vec<String>) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let anywhere = matches!(
        args.first().map(String::as_str),
        Some("fleet") | Some("recover")
    );
    let mut path: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::with_capacity(args.len());
    let mut it = args.into_iter().enumerate();
    while let Some((i, a)) = it.next() {
        if !is_flag(&a) || !(anywhere || i == 0) {
            rest.push(a);
            continue;
        }
        let value = match a.strip_prefix(FLAG).and_then(|r| r.strip_prefix('=')) {
            Some(v) => v.to_string(),
            None => it
                .next()
                .map(|(_, v)| v)
                .filter(|v| !v.starts_with('-'))
                .unwrap_or_default(),
        };
        if value.is_empty() {
            return Err(format!("{FLAG} needs a path"));
        }
        if path.replace(PathBuf::from(value)).is_some() {
            return Err(format!("{FLAG} may be given only once"));
        }
    }
    Ok((path, rest))
}

/// The hosting atrium's first step, before any dispatch and before any pane:
/// take the token file's path from the flag or the environment (the flag
/// wins), remove the variable from the environment panes inherit, load and
/// check the file, and keep the operators for [`session`]. Returns the
/// arguments without the flag. A file that fails a check is an `Err`: atrium
/// says why and does not start.
///
/// An invocation that hosts no session ([`hosts_no_session`]) does not read
/// the file: `atrium --version` has no business opening a secret, and must
/// not fail on one. `atrium ctl` is among them; its `--operator-token-file`
/// is the client's own, comes after the word `ctl`, and is left in place.
/// The variable is removed in every case.
pub fn host_startup(args: Vec<String>) -> Result<Vec<String>, String> {
    let from_env = take_env();
    let (from_flag, rest) = strip_flag(args)?;
    if hosts_no_session(&rest) {
        return Ok(rest);
    }
    if let Some(path) = from_flag.or(from_env) {
        let ops = Operators::load(&path)?;
        // Set once per process; a second call (there is none) keeps the first.
        let _ = SESSION.set(ops);
    }
    Ok(rest)
}

/// Is this an invocation that can never host a session: the `ctl` client, a
/// maintenance family, a print-and-exit flag, the watchdog re-exec, or a
/// `fleet` subcommand other than `up`? Anything not listed here is treated as
/// hosting, so a new way to start a session loads the operators by default.
fn hosts_no_session(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some(
            "ctl" | "reap" | "config" | "mod" | "--version" | "-V" | "--help" | "-h"
            | "--stdin-probe",
        ) => true,
        Some(a) if a == crate::reap::WATCHDOG_FLAG => true,
        Some("fleet") => args.get(1).map(String::as_str) != Some("up"),
        _ => false,
    }
}

/// One line per operator for the audit log at startup: who exists, what each
/// may do, and which of those verbs return model output. Never a token.
pub fn startup_records(ops: &Operators) -> Vec<String> {
    ops.entries
        .iter()
        .map(|o| {
            let model: Vec<&str> = o
                .verbs
                .iter()
                .map(String::as_str)
                .filter(|v| MODEL_OUTPUT.contains(v))
                .collect();
            format!(
                "name={} verbs={} model_output={}",
                o.name,
                if o.verbs.is_empty() {
                    "-".to_string()
                } else {
                    o.verbs.join(",")
                },
                if model.is_empty() {
                    "-".to_string()
                } else {
                    model.join(",")
                }
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
