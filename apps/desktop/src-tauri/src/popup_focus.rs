// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Windows popup intent and cancellable focus checks; no native window calls here.
use std::time::{Duration, Instant};

const OPEN_SETTLE: Duration = Duration::from_millis(350);
const BLUR_SETTLE: Duration = Duration::from_millis(120);

#[derive(Default)]
pub struct PopupFocus {
    pub open: bool,
    revision: u64,
    opening_until: Option<Instant>,
    deadline: Option<Instant>,
    retry_available: bool,
    tray_press_open: Option<bool>,
}

impl PopupFocus {
    fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn shown(&mut self, now: Instant) {
        self.invalidate();
        self.open = true;
        self.opening_until = Some(now + OPEN_SETTLE);
        self.deadline = self.opening_until;
        self.retry_available = true;
    }

    pub fn hidden(&mut self) {
        self.invalidate();
        self.open = false;
        self.deadline = None;
        self.opening_until = None;
        self.retry_available = false;
        // Keep a mouse-down snapshot until mouse-up, even if blur hides us first.
    }

    pub fn blurred(&mut self, now: Instant) {
        if self.open {
            self.invalidate();
            self.deadline = Some((now + BLUR_SETTLE).max(self.opening_until.unwrap_or(now)));
        }
    }

    pub fn focused(&mut self) {
        self.invalidate();
        // Still verify initial activation: Explorer can take focus back after GotFocus.
        self.deadline = self.opening_until;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn take_due(&mut self, now: Instant) -> Option<u64> {
        if self.deadline.is_some_and(|at| at <= now) {
            self.deadline = None;
            Some(self.revision)
        } else {
            None
        }
    }

    pub fn is_current(&self, revision: u64) -> bool {
        self.open && self.revision == revision
    }

    pub fn confirmed(&mut self) {
        self.invalidate();
        self.opening_until = None;
        self.deadline = None;
        self.retry_available = false;
    }

    /// Retry activation once only if Explorer still owns focus during opening.
    /// Never steal focus back from an application the user switched to.
    pub fn retry_shell_activation(&mut self, shell_foreground: bool, now: Instant) -> bool {
        if self.open && self.retry_available && shell_foreground {
            self.invalidate();
            self.retry_available = false;
            self.opening_until = None;
            self.deadline = Some(now + BLUR_SETTLE);
            true
        } else {
            false
        }
    }

    pub fn tray_down(&mut self) {
        self.tray_press_open = Some(self.open);
    }

    pub fn tray_up_should_show(&mut self) -> bool {
        !self.tray_press_open.take().unwrap_or(self.open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_blur_waits_for_activation_to_settle() {
        let now = Instant::now();
        let mut state = PopupFocus::default();
        state.shown(now);
        state.blurred(now + Duration::from_millis(10));
        assert!(state.take_due(now + BLUR_SETTLE).is_none());
        assert!(state.take_due(now + OPEN_SETTLE).is_some());
    }

    #[test]
    fn recovered_focus_cancels_old_blur_check() {
        let now = Instant::now();
        let mut state = PopupFocus::default();
        state.shown(now);
        let old = state.take_due(now + OPEN_SETTLE).unwrap();
        state.confirmed();
        assert!(!state.is_current(old));
        state.blurred(now + OPEN_SETTLE);
        let old = state.take_due(now + OPEN_SETTLE + BLUR_SETTLE).unwrap();
        state.focused();
        assert!(!state.is_current(old));
        assert!(state.deadline().is_none());
    }

    #[test]
    fn reopen_invalidates_queued_hide_and_does_not_swallow_click() {
        let now = Instant::now();
        let mut state = PopupFocus::default();
        state.shown(now);
        let old = state.take_due(now + OPEN_SETTLE).unwrap();
        state.hidden();
        state.tray_down();
        assert!(state.tray_up_should_show());
        state.shown(now + OPEN_SETTLE);
        assert!(!state.is_current(old));
    }

    #[test]
    fn tray_click_closes_even_if_blur_hides_between_down_and_up() {
        let mut state = PopupFocus::default();
        state.shown(Instant::now());
        state.tray_down();
        state.hidden();
        assert!(!state.tray_up_should_show());
        state.tray_down();
        assert!(state.tray_up_should_show());
    }

    #[test]
    fn activation_retry_is_bounded_and_never_targets_another_app() {
        let now = Instant::now();
        let mut state = PopupFocus::default();
        state.shown(now);
        assert!(!state.retry_shell_activation(false, now));
        assert!(state.retry_shell_activation(true, now));
        assert!(!state.retry_shell_activation(true, now));
        assert_eq!(state.deadline(), Some(now + BLUR_SETTLE));
        state.confirmed();
        assert!(!state.retry_shell_activation(true, now));
    }

    #[test]
    fn normal_blur_is_debounced_and_hide_cancels_deadline() {
        let now = Instant::now();
        let mut state = PopupFocus::default();
        state.shown(now);
        state.confirmed();
        state.blurred(now);
        assert_eq!(state.deadline(), Some(now + BLUR_SETTLE));
        state.hidden();
        assert!(state.take_due(now + BLUR_SETTLE).is_none());
    }
}
