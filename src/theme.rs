//! The amux color theme — one palette shared by the status bar ([`crate::bar`])
//! and the tile borders ([`crate::tile`]), so the chrome reads as a single
//! designed system rather than borrowed terminal defaults.
//!
//! Deliberately **vivid, high-contrast truecolor**: the states must be
//! distinguishable at a glance (focused vs idle vs waiting vs dead), which the
//! old muted 8/16-color indices made hard. The tile-border language and the bar
//! entry language are the *same* colors, so a bright-cyan border and a
//! bright-cyan bar entry mean the same thing (focused). Truecolor renders on any
//! modern terminal; a 16-color fallback would only ever *reduce* contrast, which
//! is the opposite of the goal, so we commit to RGB.

use ansi::Color;

// --- statusline base -------------------------------------------------------
/// The bar background — a dark slate the colored text sits on.
pub const BAR_BG: Color = Color::Rgb(28, 28, 36);
/// The bar's default (non-state) text.
pub const BAR_FG: Color = Color::Rgb(205, 205, 215);

// --- the amux signature chip ----------------------------------------------
/// Background of the ` amux ` wordmark chip (teal-cyan).
pub const BRAND_BG: Color = Color::Rgb(0, 184, 209);
/// Text on the wordmark chip (near-black, for max legibility on the chip).
pub const BRAND_FG: Color = Color::Rgb(10, 14, 18);

// --- shared STATE colors (bar entries AND tile borders) --------------------
/// Focused / active pane — bright cyan. The single most important "you are here".
pub const FOCUSED: Color = Color::Rgb(70, 235, 255);
/// A bound agent is blocked on you — amber.
pub const WAITING: Color = Color::Rgb(255, 200, 70);
/// The pane's process exited — red.
pub const EXITED: Color = Color::Rgb(255, 95, 95);
/// Background output arrived while unfocused — green.
pub const ACTIVITY: Color = Color::Rgb(95, 240, 140);
/// Quiet / idle — a clearly-dimmer grey, still legible against the slate.
pub const IDLE: Color = Color::Rgb(150, 152, 165);
/// The keys hint — dimmer still, so it recedes behind live state.
pub const KEYS: Color = Color::Rgb(120, 122, 135);

// --- splash wordmark gradient (cool: cyan → azure → indigo → violet) -------
/// One RGB stop per letter of `a m u x`, a cool left-to-right sweep.
pub const SPLASH_GRADIENT: [(u8, u8, u8); 4] = [
    (60, 230, 255),  // a — cyan
    (60, 160, 255),  // m — azure
    (120, 120, 255), // u — indigo
    (185, 100, 255), // x — violet
];
