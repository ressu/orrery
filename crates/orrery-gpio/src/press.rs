//! What a button's edges mean: press, release, click, and long press. Plain logic over edge
//! times, so it's tested without hardware.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fire {
    Press,
    Release,
    Click,
    LongPress,
}

impl Fire {
    pub fn name(self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Release => "release",
            Self::Click => "click",
            Self::LongPress => "long_press",
        }
    }
}

#[derive(Debug)]
pub struct Press {
    /// Moonraker's `minimum_event_time`: a change sooner than this after the previous one is
    /// ignored.
    minimum_event_time: Duration,
    /// When clicks and long presses are told apart: set when the button has `on_click` or
    /// `on_long_press`.
    long_press_time: Option<Duration>,
    pressed: bool,
    last_change: Option<Duration>,
    /// When the current press started, if it counted.
    press_started: Option<Duration>,
    long_fired: bool,
}

impl Press {
    pub fn new(minimum_event_time: Duration, long_press_time: Option<Duration>, pressed: bool) -> Self {
        Self { minimum_event_time, long_press_time, pressed, last_change: None, press_started: None, long_fired: false }
    }

    /// The line changed at `at`, a monotonic time.
    pub fn edge(&mut self, pressed: bool, at: Duration) -> Vec<Fire> {
        if pressed == self.pressed {
            return Vec::new();
        }
        self.pressed = pressed;
        let elapsed = self.last_change.map(|last| at.saturating_sub(last));
        self.last_change = Some(at);
        if elapsed.is_some_and(|e| e < self.minimum_event_time) {
            if pressed {
                self.press_started = None;
            }
            return Vec::new();
        }
        if pressed {
            self.press_started = Some(at);
            self.long_fired = false;
            return vec![Fire::Press];
        }
        let mut fires = vec![Fire::Release];
        if let (Some(start), Some(long)) = (self.press_started.take(), self.long_press_time)
            && !self.long_fired
        {
            // A timer that ran late still makes a long press of a long hold.
            fires.push(if at.saturating_sub(start) >= long { Fire::LongPress } else { Fire::Click });
        }
        fires
    }

    /// When the current press becomes a long one, if it can.
    pub fn deadline(&self) -> Option<Duration> {
        let (start, long) = (self.press_started?, self.long_press_time?);
        (!self.long_fired).then_some(start + long)
    }

    /// The time from [`Press::deadline`] has come.
    pub fn timer(&mut self, at: Duration) -> Vec<Fire> {
        match self.deadline() {
            Some(deadline) if at >= deadline => {
                self.long_fired = true;
                vec![Fire::LongPress]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    #[test]
    fn press_and_release_without_long_presses() {
        let mut p = Press::new(Duration::ZERO, None, false);
        assert_eq!(p.edge(true, s(1.0)), [Fire::Press]);
        assert_eq!(p.deadline(), None);
        assert_eq!(p.edge(false, s(5.0)), [Fire::Release]);
        // The same state again isn't a change.
        assert_eq!(p.edge(false, s(6.0)), []);
    }

    #[test]
    fn click_or_long_press_never_both() {
        let mut p = Press::new(Duration::ZERO, Some(s(1.0)), false);
        assert_eq!(p.edge(true, s(10.0)), [Fire::Press]);
        assert_eq!(p.edge(false, s(10.4)), [Fire::Release, Fire::Click]);

        assert_eq!(p.edge(true, s(20.0)), [Fire::Press]);
        assert_eq!(p.deadline(), Some(s(21.0)));
        assert_eq!(p.timer(s(20.9)), []);
        assert_eq!(p.timer(s(21.0)), [Fire::LongPress]);
        assert_eq!(p.deadline(), None);
        assert_eq!(p.timer(s(22.0)), []);
        assert_eq!(p.edge(false, s(23.0)), [Fire::Release]);

        // A late timer: the release still makes it a long press.
        assert_eq!(p.edge(true, s(30.0)), [Fire::Press]);
        assert_eq!(p.edge(false, s(31.5)), [Fire::Release, Fire::LongPress]);
    }

    #[test]
    fn minimum_event_time_ignores_quick_changes() {
        let mut p = Press::new(s(0.5), Some(s(1.0)), false);
        // The first change has nothing before it, so it counts.
        assert_eq!(p.edge(true, s(1.0)), [Fire::Press]);
        assert_eq!(p.edge(false, s(1.2)), []);
        // The press after an ignored release is a change from the last one, 0.1 s ago.
        assert_eq!(p.edge(true, s(1.3)), []);
        assert_eq!(p.deadline(), None);
        assert_eq!(p.edge(false, s(2.0)), [Fire::Release]);
    }

    #[test]
    fn held_at_startup() {
        // Released without a press that counted: no click.
        let mut p = Press::new(Duration::ZERO, Some(s(1.0)), true);
        assert_eq!(p.edge(false, s(1.0)), [Fire::Release]);
    }
}
