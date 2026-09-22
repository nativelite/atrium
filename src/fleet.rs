//! The fleet loader — `atrium fleet up <name>`, the saved-roster "swarm with
//! presets" feature (design doc §6b).
//!
//! A fleet is a named set of agents that come up **in-role** — identity, working
//! directory, context dirs, and instructions — in one command. Its definition
//! lives in a project-local `atrium.fleet.json` (checked into the repo so a team
//! shares the fleet), with a user-global fallback. The file is read **read-only**;
//! atrium never writes it.
//!
//! This module owns the *pure* seams so the wiring is testable without a pty, a
//! vault, or a layout tree:
//!
//! * [`parse`] turns the `atrium.fleet.json` text into the typed [`Fleets`] map
//!   (the §6b schema), rejecting a malformed file / missing `fleets` / a fleet
//!   with zero agents with a clear message — the loader spawns nothing on error.
//! * [`Agent::args`] is the pure arg-builder: an agent def → the argv it
//!   produces (`--add-dir` / `--append-system-prompt` / `--model` / `--effort`
//!   present exactly when the corresponding field is set). The `--session-id`
//!   inject stays in the shared spawn path, so this builder is only the
//!   fleet-specific extras on top of `cmd`.
//! * [`discover`] locates the fleet file (cwd first, then the user-global
//!   fallback) and returns its path + directory, or a clear not-found error
//!   naming both locations.
//! * [`resolve_dir`] resolves an agent's `cwd` / `add_dirs` relative to the
//!   fleet file's directory (absolute paths used as-is).
//! * [`Plan`] resolves that reach **through symlinks**, classifies each grant
//!   against an [`anchor_for`] directory, and renders the banner the operator
//!   acknowledges — the same resolved paths the spawn then uses, so disclosure
//!   and launch cannot drift. It refuses nothing; read its doc comment for what
//!   it does not cover.
//!
//! The run loop reads a [`Fleet`] out of the parsed map, builds one pane per
//! agent (each `cmd` + [`Agent::args`], under its resolved identity and cwd), and
//! lays them out in one tiled window. Identity resolution, `--session-id`
//! injection, and the name-only chrome are the existing per-pane machinery —
//! this module adds no secret handling.

mod banner;
mod locate;
mod paths;
mod plan;
mod schema;

#[cfg(test)]
mod testutil;

pub use banner::{preflight_block, sanitize, shorten, show_path, BANNER_MAX, MAX_LOUD_LINES};
pub use locate::{
    discover, fleet_object_text, global_dir, global_file, global_path, resolve_dir, Located,
    ENV_FLEET, FILE_NAME,
};
pub use paths::{home_dir, real_path, Stores};
pub use plan::{anchor_for, AgentPlan, Anchor, AnchorKind, Field, Grant, Plan, Reach};
pub use schema::{parse, Agent, Fleet, Fleets};
