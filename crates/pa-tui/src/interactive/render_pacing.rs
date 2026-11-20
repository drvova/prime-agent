//! The streaming frame interval: while a turn streams, the TUI paints at most one frame per
//! interval, and the interval follows how long the terminal takes to take a frame (the PTY
//! write). A fast local terminal keeps 16 ms; a slow link (SSH, a busy multiplexer) whose
//! writes back up stretches it toward 50 ms, so frames stop queueing behind each other.
//!
//! It never applies to the first paint or to a paint an input asked for: those keep the
//! 16 ms floor (`PaintCause::Input`, `PaintCause::Idle`). The pacing is
//! `pa_types::control`'s controller with the controlled rate as frames a minute: a slow write
//! halves it, fast writes add back step by step.

use std::time::{Duration, Instant};

use pa_types::control::{Controller, ControllerConfig, LimitReport, Observation, Outcome};

/// The floor: about 60 frames a second.
pub(super) const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// The ceiling while streaming: 20 frames a second.
const MAX_FRAME_INTERVAL: Duration = Duration::from_millis(50);
/// A frame write slower than this backs up behind the next frame.
const SLOW_WRITE: Duration = Duration::from_millis(8);

const RENDER_CONTROLLER_CONFIG: ControllerConfig = ControllerConfig {
    initial_concurrency: 1,
    min_concurrency: 1,
    max_concurrency: 1,
    initial_rate_per_min: 3_750,
    min_rate_per_min: 1_200,
    max_rate_per_min: 3_750,
    rate_step_per_min: 150,
    decrease_keep_pct: 50,
    latency_keep_pct: 100,
    latency_rise_pct: 300,
    latency_rise_samples: 3,
    headroom_pct: 0,
    decrease_spacing_ms: 250,
    ceiling_ttl_ms: 10_000,
};

/// Why a frame is about to paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaintCause {
    /// An input (key, paste, mouse, resize) changed the view since the last frame.
    Input,
    /// A streaming turn's events changed it.
    Streaming,
    /// Anything else (no turn is streaming): timers, settles, the first frame.
    Idle,
}

/// The learned streaming frame interval.
pub(super) struct RenderPacer {
    controller: Controller,
    started: Instant,
}

impl RenderPacer {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            controller: Controller::new(RENDER_CONTROLLER_CONFIG, 0),
            started: now,
        }
    }

    /// The interval the next frame of `cause` waits out after the previous one.
    pub(super) fn interval(&self, cause: PaintCause) -> Duration {
        match cause {
            PaintCause::Input | PaintCause::Idle => MIN_FRAME_INTERVAL,
            PaintCause::Streaming => {
                let rate = u64::from(self.controller.target().rate_per_min.max(1));
                Duration::from_millis(60_000 / rate).clamp(MIN_FRAME_INTERVAL, MAX_FRAME_INTERVAL)
            }
        }
    }

    /// Fold in one streaming frame's write time, measured at `now`.
    pub(super) fn observe_write(&mut self, write: Duration, now: Instant) {
        let outcome = if write >= SLOW_WRITE {
            Outcome::Overloaded
        } else {
            Outcome::Success
        };
        let now_ms = u64::try_from(now.saturating_duration_since(self.started).as_millis())
            .unwrap_or(u64::MAX);
        let _target = self.controller.observe(
            &Observation {
                outcome,
                latency_ms: None,
                limits: LimitReport::default(),
            },
            now_ms,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_frame_writes_stretch_only_the_streaming_interval() {
        let start = Instant::now();
        let mut pacer = RenderPacer::new(start);
        let mut seen = vec![(
            pacer.interval(PaintCause::Streaming),
            pacer.interval(PaintCause::Input),
        )];
        // Two slow writes (an SSH link backing up), then fast ones as it drains.
        for (at_ms, write_ms) in [
            (100, 2),
            (400, 20),
            (800, 30),
            (900, 2),
            (1_000, 2),
            (1_100, 2),
        ] {
            pacer.observe_write(
                Duration::from_millis(write_ms),
                start + Duration::from_millis(at_ms),
            );
            seen.push((
                pacer.interval(PaintCause::Streaming),
                pacer.interval(PaintCause::Input),
            ));
        }
        let ms = Duration::from_millis;
        assert_eq!(
            seen,
            [
                (ms(16), ms(16)),
                (ms(16), ms(16)),
                (ms(32), ms(16)),
                (ms(50), ms(16)),
                (ms(44), ms(16)),
                (ms(40), ms(16)),
                (ms(36), ms(16)),
            ]
        );
        // The first frame and idle frames never wait on the learned interval.
        assert_eq!(pacer.interval(PaintCause::Idle), MIN_FRAME_INTERVAL);
    }
}
