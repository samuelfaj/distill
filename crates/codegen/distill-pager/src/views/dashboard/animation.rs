//! Cadences painted from `spinner_tick`. A projected row that was not painted does not count.

use super::state::{DashboardState, RowState};

pub(crate) const SPINNER_DIVISOR: u64 = 4;

pub(crate) const NEEDS_INPUT_BLINK_DIVISOR: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Animation {
    Spinner,
    Blink,
}

impl Animation {
    fn on_boundary(self, tick: u64) -> bool {
        let divisor = match self {
            Self::Spinner => SPINNER_DIVISOR,
            Self::Blink => NEEDS_INPUT_BLINK_DIVISOR,
        };
        tick.is_multiple_of(divisor)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PaintedAnimations {
    pub(crate) spinner: bool,
    pub(crate) blink: bool,
}

impl PaintedAnimations {
    pub(crate) fn mark(&mut self, animation: Animation) {
        match animation {
            Animation::Spinner => self.spinner = true,
            Animation::Blink => self.blink = true,
        }
    }

    pub(crate) fn any(self) -> bool {
        self.spinner || self.blink
    }

    fn changes_at(self, tick: u64) -> bool {
        (self.spinner && Animation::Spinner.on_boundary(tick))
            || (self.blink && Animation::Blink.on_boundary(tick))
    }
}

impl RowState {
    /// Wide-layout icon cadence. Narrow NeedsInput is a static diamond.
    pub(crate) fn animation(self) -> Option<Animation> {
        match self {
            Self::Working => Some(Animation::Spinner),
            Self::NeedsInput => Some(Animation::Blink),
            Self::Idle | Self::Inactive | Self::Completed | Self::Failed => None,
        }
    }
}

impl DashboardState {
    pub(crate) fn tick(&mut self) -> bool {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        self.painted_animations.changes_at(self.spinner_tick)
    }
}
