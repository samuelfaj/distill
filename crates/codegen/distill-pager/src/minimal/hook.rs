// Modified for Distill by Samuel Fajreldines, 2026.
//! Function-pointer hooks for the optional minimal (scrollback-native) render mode.
//!
//! A dependency on `distill-pager-minimal` would be a cargo cycle: that crate reads this crate's `AppView`, `views::*` widgets, and `scrollback`.
//! The minimal crate instead registers its entry points here via [`install`], and this crate calls them through the stored function pointers.
//!
//! The `distill-pager-bin` binary calls `distill_pager_minimal::install()` once at startup.
//! With no hooks installed, the pager's `ScreenMode::Minimal` branches are inert: `draw` is a no-op and `/transcript` falls back to the empty case.
//! The default full-screen and inline render paths never touch this module.

use std::sync::OnceLock;

use crate::app::PagerTerminal;
use crate::app::app_view::AppView;

/// Renders one minimal-mode frame; the installed implementation is `distill_pager_minimal::draw`.
pub type MinimalDrawFn = fn(&mut AppView, &mut PagerTerminal);

/// Minimal's `/transcript` used to install a second hook here; it no longer needs one.
/// `minimal_api::request_minimal_transcript` now sets state, and the draw loop builds as much of the transcript as fits in each frame's time budget.
#[derive(Clone, Copy)]
pub struct MinimalHooks {
    /// Renders one frame of minimal mode. Called from `AppView::draw`.
    pub draw: MinimalDrawFn,
}

static HOOKS: OnceLock<MinimalHooks> = OnceLock::new();

/// The first call wins; later calls are ignored.
pub fn install(hooks: MinimalHooks) {
    let _ = HOOKS.set(hooks);
}

/// The installed hooks, if [`install`] has run.
pub fn hooks() -> Option<&'static MinimalHooks> {
    HOOKS.get()
}
