//! The control-plane server: turn a `ctl` request line into its typed
//! [`atrium::ctl::Reply`] against the live windows, and flush queued `ctl send`
//! deliveries. Moved out of `main.rs` verbatim (r10 audit B11) — the run loop
//! calls in here; nothing here owns the terminal.

mod dispatch;
mod panes;
mod reply;
mod sends;
mod spawn;
mod wake;

#[cfg(test)]
mod testutil;

pub(crate) use dispatch::{apply_ctl, decision_for_agent};
pub(crate) use panes::{effective_mode, status_label};
pub(crate) use reply::truncate;
pub(crate) use sends::{flush_sends, PendingSend};

// Reached through the crate root only by `main.rs`'s tests.
#[cfg(test)]
pub(crate) use panes::privilege_for;
#[cfg(test)]
pub(crate) use reply::audit_outcome;
#[cfg(test)]
pub(crate) use spawn::worktree_spawn_params;
