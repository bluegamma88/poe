//! Debounced scheduler for source-backed scrollback reflow on terminal resize.
//!
//! A terminal width change invalidates the wrapping of every row already
//! emitted into scrollback. Rebuilding that scrollback is expensive, so we do
//! not react to each intermediate size reported while a user drags a terminal
//! edge. Instead this state machine tracks the observed width and only declares
//! a reflow "due" once the width has been quiet for [`DEBOUNCE`].
//!
//! This module owns only the *timing* decision. The actual clear-and-replay of
//! scrollback lives in the terminal layer; see `InlineTerminal::reflow_scrollback`.

use std::time::{Duration, Instant};

/// Quiet period after the last width change before a reflow becomes due.
/// Matches the reference TUI so dragging a terminal edge does not rebuild
/// scrollback at every intermediate width.
const DEBOUNCE: Duration = Duration::from_millis(75);

#[derive(Debug, Default)]
pub(crate) struct ResizeReflowState {
    /// Width scrollback was last rebuilt at. `None` until seeded.
    last_reflow_width: Option<u16>,
    /// Most recently observed terminal width.
    observed_width: Option<u16>,
    /// Deadline after which a pending reflow becomes due. `None` when no
    /// reflow is scheduled.
    pending_until: Option<Instant>,
}

impl ResizeReflowState {
    /// Seed the baseline width without scheduling a reflow. Called once with
    /// the terminal's initial size so the first observed width is treated as
    /// already-rendered rather than a change.
    pub(crate) fn init_width(&mut self, width: u16) {
        self.last_reflow_width.get_or_insert(width);
        self.observed_width.get_or_insert(width);
    }

    /// Observe the current terminal width at `now`. A no-op while the width is
    /// unchanged; when it changes, (re)arms the debounce so a stream of resize
    /// events collapses into a single reflow once the size settles. Returning
    /// to the last-rebuilt width cancels any pending reflow.
    pub(crate) fn observe(&mut self, width: u16, now: Instant) {
        if self.observed_width == Some(width) {
            return;
        }
        self.observed_width = Some(width);
        if self.last_reflow_width == Some(width) {
            self.pending_until = None;
        } else {
            self.pending_until = Some(now + DEBOUNCE);
        }
    }

    /// If a debounced reflow is now due, consume it and return the width to
    /// rebuild scrollback at. Returns `None` while still inside the quiet
    /// period or when nothing is pending.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<u16> {
        let deadline = self.pending_until?;
        if now < deadline {
            return None;
        }
        self.pending_until = None;
        let width = self.observed_width?;
        self.last_reflow_width = Some(width);
        Some(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn seeding_baseline_does_not_schedule() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_width(80);
        state.observe(80, now);
        assert_eq!(state.take_due(at(now, 1000)), None);
    }

    #[test]
    fn width_change_fires_after_debounce() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_width(80);

        state.observe(100, now);
        // Still inside the quiet period.
        assert_eq!(state.take_due(at(now, 50)), None);
        // Past the deadline — reflow is due at the new width.
        assert_eq!(state.take_due(at(now, 80)), Some(100));
        // Only fires once.
        assert_eq!(state.take_due(at(now, 200)), None);
    }

    #[test]
    fn repeated_changes_collapse_into_one_reflow() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_width(80);

        // A drag reports a stream of intermediate widths; each pushes the
        // deadline out from its own observation time.
        state.observe(90, at(now, 0));
        state.observe(100, at(now, 30));
        state.observe(110, at(now, 60));

        // 70ms after the *first* change but only 10ms after the last — not yet.
        assert_eq!(state.take_due(at(now, 70)), None);
        // 75ms after the last change — fires once at the settled width.
        assert_eq!(state.take_due(at(now, 140)), Some(110));
        assert_eq!(state.take_due(at(now, 300)), None);
    }

    #[test]
    fn returning_to_rebuilt_width_cancels_pending() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_width(80);

        state.observe(120, at(now, 0));
        // Width snaps back to the baseline before the debounce elapses.
        state.observe(80, at(now, 20));
        assert_eq!(state.take_due(at(now, 200)), None);
    }

    #[test]
    fn unchanged_width_does_not_rearm_deadline() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_width(80);

        state.observe(100, at(now, 0));
        // Polling repeatedly with the same width must not keep pushing the
        // deadline out, or the reflow would never become due.
        state.observe(100, at(now, 40));
        state.observe(100, at(now, 80));
        assert_eq!(state.take_due(at(now, 90)), Some(100));
    }
}
