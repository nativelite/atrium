//! Per-pane agent **identity** — the credential a pane's agent runs under
//! (path B of the atrium ⨯ akey design). atrium resolves the identity's env via
//! `akey` at spawn and injects it into that one child's pty; the pane is
//! tagged with the identity **name** only — never the secret.
//!
//! This module owns the two *pure* seams so the wiring is testable without a
//! vault or a pty:
//!
//! * [`parse`] pulls `--identity <name>` / `-I <name>` out of atrium's own
//!   argument vector, leaving the hosted command intact.
//! * [`wants_env`] decides — for a given command and the active identity —
//!   whether this spawn should inject a resolved credential env (i.e. it is an
//!   agent pane *and* an identity is set). The actual `akey::resolve` call and
//!   the resolved env live only for the duration of the spawn in
//!   `spawn_pane`; nothing here ever touches secret material.
//!
//! ## The name-only / no-secret contract
//!
//! Only the identity **name** is stored (on the pane, in the chrome). The
//! resolved env is re-fetched on every spawn and dropped immediately after —
//! never cached on the pane, never logged, never persisted. This module holds
//! no secret at all: it decides *whether* to resolve; the caller resolves,
//! injects, and drops.

/// A small, clearly-marked palette for the identity name-tag color.
///
// TUNABLE: first-pass identity colors — the founder will tune these. They are
// deliberately chosen to AVOID the status hues so the two signals never clash:
// status uses yellow(11), red(1), grey(8), bright-cyan(14) and default. These
// are a distinct hue family (blue / magenta). Accessibility rule: the tag's
// `·<name>` TEXT is ALWAYS rendered (see `tile`/`bar`); color is a redundant
// second channel, never the only one.
pub const IDENTITY_PALETTE: &[u8] = &[12, 13]; // blue, magenta

/// Deterministically map an identity name to one of the [`IDENTITY_PALETTE`]
/// colors, so a given identity always reads in the same hue across panes and
/// windows without any per-identity state. Pure and stable: same name in →
/// same index out.
pub fn palette_index(name: &str) -> u8 {
    // A tiny FNV-1a-style rolling hash over the bytes — enough to spread names
    // across the palette deterministically; not security-sensitive.
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    IDENTITY_PALETTE[(h as usize) % IDENTITY_PALETTE.len()]
}

/// Extract atrium's own `--identity <name>` (or short `-I <name>`) flag from the
/// front-loaded argument vector, returning the chosen identity (if any) and the
/// remaining vector — the hosted command and its args, untouched.
///
/// The flag is atrium's, so it is only recognized as a *leading* option, before
/// the hosted command begins: `atrium [--identity <name>] [command args…]`. The
/// first non-flag token starts the command; everything from there on is the
/// child's, so a later `-I` that belongs to the hosted program is never eaten.
/// A trailing `--identity` with no value (or the glued `--identity=` with an
/// empty value) is treated as no identity — the value is required — and the
/// token is dropped so it cannot leak into the command. If the flag is given
/// more than once, the last occurrence wins.
pub fn parse(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut identity: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--identity" | "-I" => {
                if i + 1 < args.len() {
                    identity = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    // Value missing: consume the bare flag, no identity set.
                    i += 1;
                }
            }
            // The glued forms `--identity=<name>` / `-I=<name>`. An empty value
            // (`--identity=`) is treated as no identity — same as a bare flag —
            // so a meaningless empty name never reaches `akey::resolve`.
            s if s.starts_with("--identity=") => {
                let val = &s["--identity=".len()..];
                if !val.is_empty() {
                    identity = Some(val.to_string());
                }
                i += 1;
            }
            s if s.starts_with("-I=") => {
                let val = &s["-I=".len()..];
                if !val.is_empty() {
                    identity = Some(val.to_string());
                }
                i += 1;
            }
            // First non-flag token: the hosted command starts here. Stop
            // parsing atrium options so the child owns the rest verbatim.
            _ => {
                return (identity, args[i..].to_vec());
            }
        }
    }
    (identity, Vec::new())
}

/// Should this spawn inject a resolved credential env? True iff an identity is
/// set **and** the command is an agent pane atrium would bind (the same
/// agent-stem test [`crate::bind`] uses for `--session-id`). Non-agent panes
/// (shells, editors) and no-identity spawns keep plain `spawn`.
///
/// Pure: it inspects only the command's program stem and whether an identity is
/// present — it neither resolves nor touches any secret. The caller uses it to
/// choose between `pty::Pty::spawn` and `spawn_with_env`.
pub fn wants_env(command: &[String], identity: Option<&str>) -> bool {
    if identity.is_none() {
        return false;
    }
    let Some(prog) = command.first() else {
        return false; // no program to run — nothing to credential
    };
    // Cross-platform stem (splits on `/` and `\` on every OS) so a Windows-authored
    // fleet path is still recognized on macOS/Linux — see `bind::command_stem`.
    let stem = crate::bind::command_stem(prog);
    crate::bind::is_agent_stem(&stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn absent_identity_yields_none_and_full_command() {
        let (id, cmd) = parse(&v(&["claude", "--model", "opus"]));
        assert_eq!(id, None);
        assert_eq!(cmd, v(&["claude", "--model", "opus"]));
    }

    #[test]
    fn long_flag_is_consumed_and_command_preserved() {
        let (id, cmd) = parse(&v(&["--identity", "work", "claude", "--continue"]));
        assert_eq!(id.as_deref(), Some("work"));
        assert_eq!(cmd, v(&["claude", "--continue"]));
    }

    #[test]
    fn short_flag_is_consumed_and_command_preserved() {
        let (id, cmd) = parse(&v(&["-I", "wif:prod", "claude"]));
        assert_eq!(id.as_deref(), Some("wif:prod"));
        assert_eq!(cmd, v(&["claude"]));
    }

    #[test]
    fn glued_forms_are_accepted() {
        let (id, cmd) = parse(&v(&["--identity=work", "claude"]));
        assert_eq!(id.as_deref(), Some("work"));
        assert_eq!(cmd, v(&["claude"]));
        let (id, cmd) = parse(&v(&["-I=work", "claude"]));
        assert_eq!(id.as_deref(), Some("work"));
        assert_eq!(cmd, v(&["claude"]));
    }

    #[test]
    fn identity_with_no_command_yields_empty_command() {
        let (id, cmd) = parse(&v(&["--identity", "work"]));
        assert_eq!(id.as_deref(), Some("work"));
        assert!(cmd.is_empty());
    }

    #[test]
    fn a_flag_that_belongs_to_the_hosted_command_is_not_eaten() {
        // The command starts at the first non-flag token (`claude`); a `-I` the
        // hosted program takes lives *after* that and must pass through.
        let (id, cmd) = parse(&v(&["claude", "-I", "somearg"]));
        assert_eq!(id, None);
        assert_eq!(cmd, v(&["claude", "-I", "somearg"]));
    }

    #[test]
    fn trailing_bare_flag_is_dropped_not_leaked() {
        // `--identity` with no value: no identity, and the bare flag does not
        // survive into the command.
        let (id, cmd) = parse(&v(&["--identity"]));
        assert_eq!(id, None);
        assert!(cmd.is_empty());
    }

    #[test]
    fn empty_glued_value_is_treated_as_no_identity() {
        // `--identity=` must not send an empty name to akey::resolve.
        let (id, cmd) = parse(&v(&["--identity=", "claude"]));
        assert_eq!(id, None);
        assert_eq!(cmd, v(&["claude"]));
        let (id, _) = parse(&v(&["-I=", "claude"]));
        assert_eq!(id, None);
    }

    #[test]
    fn repeated_identity_flag_last_wins() {
        let (id, cmd) = parse(&v(&["--identity", "work", "-I", "personal", "claude"]));
        assert_eq!(id.as_deref(), Some("personal"));
        assert_eq!(cmd, v(&["claude"]));
    }

    #[test]
    fn wants_env_is_safe_on_an_empty_command() {
        // A pub function with no documented precondition must not panic on `&[]`.
        assert!(!wants_env(&[], Some("work")));
    }

    #[test]
    fn wants_env_only_for_agent_with_identity() {
        // Agent + identity -> inject.
        assert!(wants_env(&v(&["claude"]), Some("work")));
        assert!(wants_env(&v(&["C:\\tools\\claude.cmd"]), Some("work")));
        // Agent, no identity -> plain spawn.
        assert!(!wants_env(&v(&["claude"]), None));
        // Non-agent, even with identity -> plain spawn (a shell is not credentialed).
        assert!(!wants_env(&v(&["cmd"]), Some("work")));
        assert!(!wants_env(&v(&["sh", "-i"]), Some("work")));
    }

    #[test]
    fn palette_is_deterministic_and_within_the_tunable_set() {
        for name in ["work", "wif:prod", "personal", "scratch"] {
            let a = palette_index(name);
            let b = palette_index(name);
            assert_eq!(a, b, "stable for {name}");
            assert!(IDENTITY_PALETTE.contains(&a), "in palette for {name}");
        }
    }
}
