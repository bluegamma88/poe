//! Debounced scheduler for source-backed scrollback reflow on terminal resize.
//!
//! A terminal width change invalidates the wrapping of every row already
//! emitted into scrollback; a height change exposes, hides, or shifts rows
//! around the inline viewport. Either warrants rebuilding scrollback from the
//! retained source. Rebuilding is expensive, so we do not react to each
//! intermediate size reported while a user drags a terminal edge. Instead this
//! state machine tracks the observed size and only declares a reflow "due" once
//! the size has been quiet for [`DEBOUNCE`].
//!
//! This module owns only the *timing* decision. The actual clear-and-replay of
//! scrollback lives in the terminal layer; see `InlineTerminal::reflow_scrollback`.

use std::time::{Duration, Instant};

/// Terminal dimensions tracked for reflow scheduling, as `(width, height)`.
/// A plain tuple keeps this module free of a backend dependency.
type Size = (u16, u16);

/// Quiet period after the last width change before a reflow becomes due.
/// Matches the reference TUI so dragging a terminal edge does not rebuild
/// scrollback at every intermediate width.
const DEBOUNCE: Duration = Duration::from_millis(75);

/// Default cap on rendered rows replayed during reflow. Approximates common
/// terminal scrollback retention so a long session does not reformat unbounded
/// history the terminal would not have kept anyway.
const DEFAULT_MAX_REFLOW_ROWS: usize = 10_000;

/// Resolve the row cap from `POE_TUI_RESIZE_REFLOW_MAX_ROWS`:
/// unset or unparseable -> [`DEFAULT_MAX_REFLOW_ROWS`]; `0` -> no cap (replay
/// every retained row); a positive number -> that many rows.
pub(crate) fn parse_max_rows(raw: Option<&str>) -> Option<usize> {
    match raw {
        None => Some(DEFAULT_MAX_REFLOW_ROWS),
        Some(value) => match value.trim().parse::<usize>() {
            Ok(0) => None,
            Ok(rows) => Some(rows),
            Err(_) => Some(DEFAULT_MAX_REFLOW_ROWS),
        },
    }
}

#[derive(Debug, Default)]
pub(crate) struct ResizeReflowState {
    /// Size scrollback was last rebuilt at. `None` until seeded.
    last_reflow: Option<Size>,
    /// Most recently observed terminal size.
    observed: Option<Size>,
    /// Deadline after which a pending reflow becomes due. `None` when no
    /// reflow is scheduled.
    pending_until: Option<Instant>,
}

impl ResizeReflowState {
    /// Seed the baseline size without scheduling a reflow. Called once with
    /// the terminal's initial size so the first observed size is treated as
    /// already-rendered rather than a change.
    pub(crate) fn init_size(&mut self, width: u16, height: u16) {
        self.last_reflow.get_or_insert((width, height));
        self.observed.get_or_insert((width, height));
    }

    /// Observe the current terminal size at `now`. A no-op while the size is
    /// unchanged; when it changes, (re)arms the debounce so a stream of resize
    /// events collapses into a single reflow once the size settles. Returning
    /// to the last-rebuilt size cancels any pending reflow.
    pub(crate) fn observe(&mut self, width: u16, height: u16, now: Instant) {
        let size = (width, height);
        if self.observed == Some(size) {
            return;
        }
        self.observed = Some(size);
        if self.last_reflow == Some(size) {
            self.pending_until = None;
        } else {
            self.pending_until = Some(now + DEBOUNCE);
        }
    }

    /// If a debounced reflow is now due, consume it and return the size to
    /// rebuild scrollback at. Returns `None` while still inside the quiet
    /// period or when nothing is pending.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<Size> {
        let deadline = self.pending_until?;
        if now < deadline {
            return None;
        }
        self.pending_until = None;
        let size = self.observed?;
        self.last_reflow = Some(size);
        Some(size)
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
        state.init_size(80, 24);
        state.observe(80, 24, now);
        assert_eq!(state.take_due(at(now, 1000)), None);
    }

    #[test]
    fn width_change_fires_after_debounce() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_size(80, 24);

        state.observe(100, 24, now);
        // Still inside the quiet period.
        assert_eq!(state.take_due(at(now, 50)), None);
        // Past the deadline — reflow is due at the new size.
        assert_eq!(state.take_due(at(now, 80)), Some((100, 24)));
        // Only fires once.
        assert_eq!(state.take_due(at(now, 200)), None);
    }

    #[test]
    fn height_change_alone_schedules_reflow() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_size(80, 24);

        // Width unchanged, height grew — rows shift around the viewport, so a
        // reflow is still warranted.
        state.observe(80, 40, now);
        assert_eq!(state.take_due(at(now, 80)), Some((80, 40)));
    }

    #[test]
    fn repeated_changes_collapse_into_one_reflow() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_size(80, 24);

        // A drag reports a stream of intermediate sizes; each pushes the
        // deadline out from its own observation time.
        state.observe(90, 24, at(now, 0));
        state.observe(100, 30, at(now, 30));
        state.observe(110, 32, at(now, 60));

        // 70ms after the *first* change but only 10ms after the last — not yet.
        assert_eq!(state.take_due(at(now, 70)), None);
        // 75ms after the last change — fires once at the settled size.
        assert_eq!(state.take_due(at(now, 140)), Some((110, 32)));
        assert_eq!(state.take_due(at(now, 300)), None);
    }

    #[test]
    fn returning_to_rebuilt_size_cancels_pending() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_size(80, 24);

        state.observe(120, 24, at(now, 0));
        // Size snaps back to the baseline before the debounce elapses.
        state.observe(80, 24, at(now, 20));
        assert_eq!(state.take_due(at(now, 200)), None);
    }

    #[test]
    fn max_rows_parses_env_overrides() {
        assert_eq!(parse_max_rows(None), Some(DEFAULT_MAX_REFLOW_ROWS));
        assert_eq!(parse_max_rows(Some("0")), None);
        assert_eq!(parse_max_rows(Some("500")), Some(500));
        assert_eq!(parse_max_rows(Some("  42 ")), Some(42));
        // Garbage falls back to the default rather than disabling the cap.
        assert_eq!(
            parse_max_rows(Some("nonsense")),
            Some(DEFAULT_MAX_REFLOW_ROWS)
        );
    }

    #[test]
    fn unchanged_size_does_not_rearm_deadline() {
        let now = Instant::now();
        let mut state = ResizeReflowState::default();
        state.init_size(80, 24);

        state.observe(100, 24, at(now, 0));
        // Polling repeatedly with the same size must not keep pushing the
        // deadline out, or the reflow would never become due.
        state.observe(100, 24, at(now, 40));
        state.observe(100, 24, at(now, 80));
        assert_eq!(state.take_due(at(now, 90)), Some((100, 24)));
    }
}
