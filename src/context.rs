//! The **context seam**: how a fleet declares that its agents should share a
//! context store, and the one pure function that turns that declaration into the
//! environment variables a spawned agent is handed.
//!
//! # Why a seam
//!
//! atrium hosts programs; it never inspects or credentials them. Context sharing
//! is the same discipline turned outward: atrium does not *implement* a context
//! store, it only tells each agent *where* one lives and *which slice* of it is
//! theirs, through environment variables the child process already understands.
//! Everything the store needs is expressed as `(name, value)` pairs, so the
//! coupling between atrium and the backend is exactly one function —
//! [`context_env`] — plus a fleet-file block ([`ContextCfg`]).
//!
//! # Provider-neutral by construction
//!
//! The backend is a [`Provider`] enum, not a hard-wired string. Today the only
//! real variant is [`Provider::ContextMode`] (the `context-mode` store, keyed by
//! `CONTEXT_MODE_DIR` + `CONTEXT_MODE_SESSION_SUFFIX`); a future native store is a
//! *new variant* here plus a new arm in [`context_env`], and nothing else in the
//! tree moves. [`Provider::None`] is the safe sink: an unknown provider in a fleet
//! file degrades to it (with a warning) rather than aborting the launch, so a
//! forward-dated fleet file never bricks an older atrium.
//!
//! # Sharing model
//!
//! A store is one directory ([`Provider::ContextMode`] → `CONTEXT_MODE_DIR`) plus
//! a *session suffix* that partitions it. What [`Share`] chooses is how the
//! suffix is assigned:
//!
//! * [`Share::Knowledge`] — the whole fleet points at one `CONTEXT_MODE_DIR`, but
//!   each agent gets its **own** suffix (its `agent_name`): shared knowledge base,
//!   private sessions.
//! * [`Share::Full`] — one `CONTEXT_MODE_DIR` **and** one shared suffix
//!   ([`FULL_SESSION_SUFFIX`]), so every agent reads and writes the same session.
//! * [`Share::None`] — inject nothing; agents fall back to their own ambient
//!   configuration exactly as if the block were absent.
//!
//! # Purity
//!
//! [`context_env`] is a pure function of its arguments: same `(cfg, fleet_root,
//! agent_name)` in, same vector out — no filesystem, no clock, no environment
//! reads. That is what lets the whole seam be tested by asserting the exact
//! variable set for each `share` × agent, mirroring the pure arg-builder idiom in
//! `spawn.rs`. Creating `fleet_root` on disk and merging the result into the
//! child environment are the *caller's* jobs (`fleet_cli.rs`), kept out of here on
//! purpose.

use std::path::Path;

use json::Value;

/// The context backend a fleet asks for. Provider-neutral: a future native store
/// is a new variant here and a new arm in [`context_env`] — nothing else changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// The `context-mode` store, addressed by `CONTEXT_MODE_DIR` +
    /// `CONTEXT_MODE_SESSION_SUFFIX`.
    ContextMode,
    /// No backend — inject nothing. Also the landing spot for an unknown provider
    /// string, so an unrecognised fleet file is a no-op, not a failure.
    None,
}

impl Default for Provider {
    /// Absent-block / unknown-provider default: inject nothing.
    fn default() -> Self {
        Provider::None
    }
}

/// How much of the store the fleet's agents share. See the module docs for the
/// three postures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    /// Shared store directory, per-agent session suffix.
    Knowledge,
    /// Shared store directory *and* shared session suffix.
    Full,
    /// Inject nothing (as if no block were present).
    None,
}

impl Default for Share {
    /// The default sharing posture when a `context` block is present but does not
    /// name `share`: a shared knowledge base with private sessions.
    fn default() -> Self {
        Share::Knowledge
    }
}

/// A fleet's parsed `context` block. `None` on the [`Fleet`] means the block was
/// absent and nothing is injected; `Some(ContextCfg)` carries the resolved
/// provider + share. The store directory and per-agent identity are supplied at
/// spawn time — they are inputs to [`context_env`], not stored here.
///
/// [`Fleet`]: crate::fleet::Fleet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCfg {
    pub provider: Provider,
    pub share: Share,
}

impl ContextCfg {
    /// The defaults for a `context` block that is *present but empty* (`{}`) or
    /// otherwise malformed: `provider = context-mode`, `share = knowledge`. This
    /// is deliberately **not** [`Default::default`] on the fields — a present
    /// block means the operator asked for context, so the provider defaults to the
    /// real backend, whereas the *type* default ([`Provider::None`]) is the
    /// absent-block / opt-out value.
    fn present_default() -> Self {
        ContextCfg {
            provider: Provider::ContextMode,
            share: Share::Knowledge,
        }
    }
}

/// The store-directory variable for [`Provider::ContextMode`].
pub const ENV_DIR: &str = "CONTEXT_MODE_DIR";
/// The session-partition variable for [`Provider::ContextMode`].
pub const ENV_SESSION_SUFFIX: &str = "CONTEXT_MODE_SESSION_SUFFIX";
/// The shared session suffix used for [`Share::Full`] — every agent lands in the
/// same session named this.
pub const FULL_SESSION_SUFFIX: &str = "fleet";

/// Parse a fleet's `context` block into a [`ContextCfg`], **tolerantly**.
///
/// This is the parse boundary the fleet parser (`fleet.rs`) delegates to when —
/// and only when — a `context` key is present, storing the result as `Some(cfg)`.
/// Unlike the rest of the fleet parser it never returns an error: forward
/// compatibility is a design requirement, so an unknown `provider`, an unknown
/// `share`, a wrong JSON type, or unknown inner keys each degrade to a safe value
/// with a one-line warning to stderr rather than aborting the launch.
///
/// Resolution:
/// * `provider`: `"context-mode"` → [`Provider::ContextMode`]; omitted →
///   [`Provider::ContextMode`] (a present block opts in); anything else → warn +
///   [`Provider::None`] (no-op).
/// * `share`: `"knowledge"`/`"full"`/`"none"` → the matching [`Share`]; omitted →
///   [`Share::Knowledge`]; anything else → warn + [`Share::Knowledge`].
pub fn parse_block(fleet_name: &str, v: &Value) -> ContextCfg {
    let obj = match v.as_object() {
        Some(o) => o,
        None => {
            eprintln!(
                "atrium fleet {fleet_name:?}: \"context\" must be an object; \
                 using defaults (provider=context-mode, share=knowledge)"
            );
            return ContextCfg::present_default();
        }
    };
    // Same last-wins lookup idiom as fleet.rs's parse_fleet / parse_agent.
    let get = |key: &str| obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);

    let provider = match get("provider") {
        // Block present, provider omitted → the operator opted in; use the store.
        None => Provider::ContextMode,
        Some(pv) => match pv.as_str() {
            Some("context-mode") => Provider::ContextMode,
            Some(other) => {
                eprintln!(
                    "atrium fleet {fleet_name:?}: unknown context provider {other:?}; \
                     no context injected"
                );
                Provider::None
            }
            None => {
                eprintln!(
                    "atrium fleet {fleet_name:?}: \"context.provider\" must be a string; \
                     no context injected"
                );
                Provider::None
            }
        },
    };

    let share = match get("share") {
        None => Share::Knowledge,
        Some(sv) => match sv.as_str() {
            Some("knowledge") => Share::Knowledge,
            Some("full") => Share::Full,
            Some("none") => Share::None,
            Some(other) => {
                eprintln!(
                    "atrium fleet {fleet_name:?}: unknown context share {other:?}; \
                     falling back to \"knowledge\""
                );
                Share::Knowledge
            }
            None => {
                eprintln!(
                    "atrium fleet {fleet_name:?}: \"context.share\" must be a string; \
                     falling back to \"knowledge\""
                );
                Share::Knowledge
            }
        },
    };

    ContextCfg { provider, share }
}

/// The **pure env-seam**: the environment variables one agent is handed for the
/// context store, given the fleet's [`ContextCfg`], the resolved store directory
/// `fleet_root`, and the agent's own `agent_name`.
///
/// Pure and total: no I/O, no environment access, and every input combination
/// yields a well-defined vector (possibly empty). The caller merges these pairs
/// into the child environment alongside the `ATRIUM_*` variables; it does not
/// override any variable the seam does not name.
///
/// Mapping for [`Provider::ContextMode`]:
///
/// | `share`               | pairs                                                                        |
/// |-----------------------|------------------------------------------------------------------------------|
/// | [`Share::Knowledge`]  | `CONTEXT_MODE_DIR=<fleet_root>`, `CONTEXT_MODE_SESSION_SUFFIX=<agent_name>`   |
/// | [`Share::Full`]       | `CONTEXT_MODE_DIR=<fleet_root>`, `CONTEXT_MODE_SESSION_SUFFIX=fleet`          |
/// | [`Share::None`]       | *(empty)*                                                                     |
///
/// [`Provider::None`] yields an empty vector regardless of `share`.
///
/// `fleet_root` is rendered with [`Path::display`], which uses the platform's
/// native separator — so on Windows the value carries backslashes
/// (`C:\proj\.atrium\ctx\demo`) exactly as the store expects, with no manual
/// slash rewriting.
pub fn context_env(cfg: &ContextCfg, fleet_root: &Path, agent_name: &str) -> Vec<(String, String)> {
    match cfg.provider {
        Provider::None => Vec::new(),
        Provider::ContextMode => match cfg.share {
            Share::None => Vec::new(),
            Share::Knowledge => vec![
                (ENV_DIR.to_string(), fleet_root.display().to_string()),
                (ENV_SESSION_SUFFIX.to_string(), agent_name.to_string()),
            ],
            Share::Full => vec![
                (ENV_DIR.to_string(), fleet_root.display().to_string()),
                (
                    ENV_SESSION_SUFFIX.to_string(),
                    FULL_SESSION_SUFFIX.to_string(),
                ),
            ],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A `ContextMode` config at the given share level.
    fn cm(share: Share) -> ContextCfg {
        ContextCfg {
            provider: Provider::ContextMode,
            share,
        }
    }

    // -- share=none: nothing injected -------------------------------------

    #[test]
    fn none_share_injects_nothing_for_lead() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        assert_eq!(
            context_env(&cm(Share::None), root, "lead"),
            Vec::<(String, String)>::new(),
            "share=none (lead): zero vars"
        );
    }

    #[test]
    fn none_share_injects_nothing_for_ada() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        assert_eq!(
            context_env(&cm(Share::None), root, "ada"),
            Vec::<(String, String)>::new(),
            "share=none (ada): zero vars"
        );
    }

    // -- provider=none: nothing injected regardless of share --------------

    #[test]
    fn provider_none_injects_nothing_even_for_full_share() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        let cfg = ContextCfg {
            provider: Provider::None,
            share: Share::Full,
        };
        assert_eq!(
            context_env(&cfg, root, "lead"),
            Vec::<(String, String)>::new(),
            "provider=none: zero vars regardless of share"
        );
    }

    // -- share=knowledge: shared dir, per-agent suffix --------------------

    #[test]
    fn knowledge_shares_dir_and_uses_agent_suffix_for_lead() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        assert_eq!(
            context_env(&cm(Share::Knowledge), root, "lead"),
            vec![
                (ENV_DIR.to_string(), root.display().to_string()),
                (ENV_SESSION_SUFFIX.to_string(), "lead".to_string()),
            ],
            "knowledge (lead): CONTEXT_MODE_DIR=<root> + CONTEXT_MODE_SESSION_SUFFIX=lead"
        );
    }

    #[test]
    fn knowledge_gives_distinct_agents_distinct_suffixes() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        let lead = context_env(&cm(Share::Knowledge), root, "lead");
        let ada = context_env(&cm(Share::Knowledge), root, "ada");
        // Same store dir…
        assert_eq!(
            lead[0], ada[0],
            "knowledge: both agents share CONTEXT_MODE_DIR"
        );
        // …different session suffix (the agent name).
        assert_eq!(
            lead[1],
            (ENV_SESSION_SUFFIX.to_string(), "lead".to_string())
        );
        assert_eq!(ada[1], (ENV_SESSION_SUFFIX.to_string(), "ada".to_string()));
    }

    // -- share=full: shared dir AND shared suffix -------------------------

    #[test]
    fn full_shares_dir_and_the_fleet_suffix_for_lead() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        assert_eq!(
            context_env(&cm(Share::Full), root, "lead"),
            vec![
                (ENV_DIR.to_string(), root.display().to_string()),
                (
                    ENV_SESSION_SUFFIX.to_string(),
                    FULL_SESSION_SUFFIX.to_string()
                ),
            ],
            "full (lead): CONTEXT_MODE_DIR=<root> + CONTEXT_MODE_SESSION_SUFFIX=fleet"
        );
    }

    #[test]
    fn full_gives_every_agent_the_same_shared_suffix() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        let lead = context_env(&cm(Share::Full), root, "lead");
        let ada = context_env(&cm(Share::Full), root, "ada");
        assert_eq!(lead, ada, "full: the env is identical for every agent");
        assert_eq!(
            lead[1].1, FULL_SESSION_SUFFIX,
            "full: shared suffix is \"fleet\""
        );
    }

    // -- fleet_root passes through verbatim, incl. Windows backslashes ----

    #[test]
    fn fleet_root_is_rendered_verbatim() {
        let root = Path::new("/tmp/ctx-17686");
        let env = context_env(&cm(Share::Knowledge), root, "lead");
        assert_eq!(env[0], (ENV_DIR.to_string(), root.display().to_string()));
    }

    #[test]
    fn windows_backslash_root_is_preserved() {
        // A Windows-shaped store dir must reach CONTEXT_MODE_DIR with its
        // backslashes intact — no slash rewriting in the seam.
        let raw = r"C:\proj\.atrium\ctx\demo";
        let root = Path::new(raw);
        let env = context_env(&cm(Share::Full), root, "lead");
        assert_eq!(env[0].0, ENV_DIR);
        assert_eq!(env[0].1, root.display().to_string());
    }

    // -- key order is part of the contract: DIR before SUFFIX -------------

    #[test]
    fn dir_precedes_suffix_for_knowledge_and_full() {
        let root = Path::new("/srv/.atrium/ctx/demo");
        for share in [Share::Knowledge, Share::Full] {
            let env = context_env(&cm(share), root, "lead");
            assert_eq!(env.len(), 2, "{share:?}: exactly two vars");
            assert_eq!(env[0].0, ENV_DIR, "{share:?}: CONTEXT_MODE_DIR first");
            assert_eq!(env[1].0, ENV_SESSION_SUFFIX, "{share:?}: suffix second");
        }
    }

    // -- parse_block tolerance --------------------------------------------

    #[test]
    fn parse_block_defaults_when_block_empty() {
        let v = json::parse("{}").unwrap();
        assert_eq!(
            parse_block("demo", &v),
            ContextCfg {
                provider: Provider::ContextMode,
                share: Share::Knowledge
            },
            "empty block opts in: provider=context-mode, share=knowledge"
        );
    }

    #[test]
    fn parse_block_reads_provider_and_share() {
        let v = json::parse(r#"{"provider":"context-mode","share":"full"}"#).unwrap();
        assert_eq!(
            parse_block("demo", &v),
            ContextCfg {
                provider: Provider::ContextMode,
                share: Share::Full
            }
        );
    }

    #[test]
    fn parse_block_unknown_provider_degrades_to_none() {
        let v = json::parse(r#"{"provider":"native","share":"full"}"#).unwrap();
        assert_eq!(parse_block("demo", &v).provider, Provider::None);
    }

    #[test]
    fn parse_block_unknown_share_degrades_to_knowledge() {
        let v = json::parse(r#"{"share":"everything"}"#).unwrap();
        assert_eq!(parse_block("demo", &v).share, Share::Knowledge);
    }

    #[test]
    fn parse_block_ignores_unknown_keys() {
        let v = json::parse(r#"{"share":"full","future":true}"#).unwrap();
        assert_eq!(parse_block("demo", &v).share, Share::Full);
    }
}
