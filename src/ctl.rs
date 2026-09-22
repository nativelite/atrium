//! `atrium ctl` — the verb layer over the control channel ([`crate::ipc`]).
//!
//! Two halves live here:
//! * The **client** ([`ctl_cmd`]): parse `atrium ctl <cmd> …` argv into one JSON
//!   request, read `ATRIUM_CTL` (the endpoint) and `ATRIUM_PANE` (the caller's agent
//!   id) from the environment atrium injected, send it, and print the JSON reply.
//! * The **protocol + policy** the server (the run loop) applies: [`parse_request`]
//!   turns a request line into a typed [`Request`], and [`evaluate_spawn`] is the
//!   *pure* guard — allowlist + depth cap — so the safety rules are unit-tested
//!   without a pty or a running atrium.
//!
//! Surface: `spawn` (open a visible worker — new window or `--here` split, under
//! an optionally-delegated `--identity`), `list` (the org chart), `send`
//! (queue-until-idle task delivery), `status` (agsess-backed), `kill` (subtree
//! teardown), and `audit` (the ctl request log). `send`/`status`/`kill`/`audit`
//! with a target are subtree-scoped; credential delegation is scoped by
//! [`delegation_allowed`] (C3).

/// Environment variable naming the control endpoint, injected into every pane a
/// `--allow-ctl` atrium spawns. Absent → `atrium ctl` refuses (not in a ctl session).
pub const ENV_ADDRESS: &str = "ATRIUM_CTL";
/// Environment variable carrying the *caller* pane's agent id, so the server can
/// attribute a spawn to its parent (spawn tree + depth). Injected per pane.
pub const ENV_PANE: &str = "ATRIUM_PANE";
/// Environment variable carrying the pane's **capability token** — an unguessable
/// per-pane secret minted at spawn. The server authenticates a request by looking
/// up the pane whose token matches (identity comes from the token, not the
/// self-reported `ATRIUM_PANE`), so a pane can neither claim another pane's id nor
/// claim operator. A request with no valid token is *unauthenticated* and may
/// only run read-only commands. Injected per pane alongside `ATRIUM_PANE`.
pub const ENV_TOKEN: &str = "ATRIUM_TOKEN";

/// The default spawn-depth ceiling: the recursion circuit-breaker (design §5).
/// Generous — a real hierarchy is CEO→lead→IC (depth 2–3); this only stops a
/// runaway self-spawning agent. Overridable/removable via `--max-depth`.
pub const DEFAULT_MAX_DEPTH: usize = 6;

mod client;
mod launch;
mod policy;
mod render;
mod reply;
mod request;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;

pub use client::{build_request, ctl_cmd};
pub use launch::{parse_flags, TrustMode, AGENT_CTL_DIRECTIVE, SKIP_PERMISSIONS_FLAG};
pub use policy::{
    delegation_allowed, evaluate_spawn, extra_allow_from_env, in_subtree, parse_allow_list,
    resolve_target, sanitize_spawn_argv, vet_spawn_argv, ArgvVerdict, SpawnDenied, ENV_ALLOW,
    ENV_AUDIT,
};
pub use render::{hyperlink, is_url, status_glyph, status_sgr};
pub use reply::{
    humanize_idle, reply_audit, reply_board_claim, reply_board_del, reply_board_entry,
    reply_board_list, reply_board_release, reply_bus_feed, reply_bus_published, reply_bus_resolved,
    reply_bus_subscribed, reply_bus_topics, reply_err, reply_killed, reply_list, reply_sent,
    reply_spawned, reply_status_one, zero_sub_warning, ListNode, Reply, TreeNode,
    IDLE_RENDER_MIN_MS,
};
pub use request::{
    parse_request, AgentId, AuditReq, BoardOp, BusOp, Cmd, KillReq, Request, RespawnReq, SendReq,
    SpawnReq, StatusReq,
};
