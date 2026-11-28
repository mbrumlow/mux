use std::time::Duration;

use tokio::time::Instant;

const QUIET_PERIOD: Duration = Duration::from_millis(75);
const MAX_WAIT: Duration = Duration::from_millis(250);

/// Coalesce SIGWINCH bursts without postponing updates forever during a drag.
/// Query the terminal when the deadline fires, so only the latest size is sent.
pub(crate) struct ResizeDebounce {
    first_signal: Option<Instant>,
    pub(crate) deadline: Option<Instant>,
    last_sent: (u16, u16),
}

impl ResizeDebounce {
    pub(crate) fn new(initial_size: (u16, u16)) -> Self {
        Self {
            first_signal: None,
            deadline: None,
            last_sent: initial_size,
        }
    }

    pub(crate) fn signal(&mut self, now: Instant) {
        let first = *self.first_signal.get_or_insert(now);
        self.deadline = Some((now + QUIET_PERIOD).min(first + MAX_WAIT));
    }

    pub(crate) fn flush_due(&mut self, now: Instant, size: (u16, u16)) -> Option<(u16, u16)> {
        if self.deadline.is_none_or(|deadline| now < deadline) {
            return None;
        }
        self.first_signal = None;
        self.deadline = None;
        if size == self.last_sent {
            return None;
        }
        self.last_sent = size;
        Some(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_sends_only_final_dimensions_after_quiet_period() {
        let start = Instant::now();
        let mut resize = ResizeDebounce::new((80, 24));
        for millis in [0, 20, 40, 60] {
            let now = start + Duration::from_millis(millis);
            resize.signal(now);
            assert_eq!(resize.flush_due(now, (90, 30)), None);
        }
        assert_eq!(
            resize.flush_due(start + Duration::from_millis(134), (100, 40)),
            None
        );
        assert_eq!(
            resize.flush_due(start + Duration::from_millis(135), (120, 50)),
            Some((120, 50))
        );
        assert_eq!(
            resize.flush_due(start + Duration::from_secs(1), (120, 50)),
            None
        );
    }

    #[test]
    fn continuous_drag_has_bounded_latency_and_delivers_trailing_size() {
        let start = Instant::now();
        let mut resize = ResizeDebounce::new((80, 24));
        for millis in (0..=240).step_by(20) {
            resize.signal(start + Duration::from_millis(millis));
        }
        assert_eq!(
            resize.flush_due(start + MAX_WAIT, (100, 30)),
            Some((100, 30))
        );
        resize.signal(start + Duration::from_millis(260));
        assert_eq!(
            resize.flush_due(start + Duration::from_millis(335), (110, 35)),
            Some((110, 35))
        );
    }

    #[test]
    fn duplicate_signals_and_return_to_original_size_send_nothing() {
        let start = Instant::now();
        let mut resize = ResizeDebounce::new((80, 24));
        resize.signal(start);
        assert_eq!(resize.flush_due(start + QUIET_PERIOD, (80, 24)), None);
        resize.signal(start + Duration::from_secs(1));
        assert_eq!(
            resize.flush_due(start + Duration::from_secs(2), (80, 24)),
            None
        );
        resize.signal(start + Duration::from_secs(3));
        assert_eq!(
            resize.flush_due(start + Duration::from_secs(4), (120, 40)),
            Some((120, 40))
        );
    }
}
