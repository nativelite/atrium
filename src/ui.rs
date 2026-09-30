//! What a keyboard or mouse handler acts on, bundled: the terminal it draws to,
//! and the window list it may change. Every handler took these as the same run
//! of positional arguments (`rows, cols, out, flash`; `windows, active`), which
//! is how clippy's `too_many_arguments` found them.

use crate::*;

/// The terminal a handler draws on this tick: its size, where its bytes go, and
/// the status-bar flash it reports a failure through.
pub(crate) struct Ui<'a, W> {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) out: &'a mut W,
    pub(crate) flash: &'a mut Option<(String, Instant)>,
}

/// The windows a handler may open, close or switch between, and which one is
/// active.
pub(crate) struct Desk<'a> {
    pub(crate) windows: &'a mut Vec<Window>,
    pub(crate) active: &'a mut usize,
}
