//! Frame and capture counters.
//!
//! Everything here answers "is it actually slow?", which is the question every
//! performance change should start from. The numbers are cheap to keep (a few
//! adds per frame) and are exposed over IPC so a change can be measured on a
//! running compositor instead of argued about.
//!
//! Nothing is sampled over time: these are running totals plus the most recent
//! frame, so a reader computes rates itself from two reads. That keeps the
//! compositor free of timers whose phase would compete with the frame loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::output::Output;

use crate::backend::RenderResult;

/// How many frame intervals of history the frame-time percentiles keep.
///
/// A power of two so the ring is a mask instead of a modulo. At 60 Hz this is
/// about two seconds, long enough to include a slow animation and short enough
/// that an old hitch stops skewing the numbers.
const FRAME_HISTORY: usize = 128;

/// A gap longer than this is an idle period, not a dropped frame.
///
/// The compositor renders on demand, so a desktop that nobody touches produces
/// no frames at all; the first frame after that has a gap measured in seconds.
/// Counting it as late would make every interaction look like a hitch. Half a
/// second is long enough to still catch a genuine stall (thirty frames at 60 Hz)
/// and short enough that a real pause is never mistaken for one.
const IDLE_GAP: Duration = Duration::from_millis(500);

/// Per-output frame statistics.
#[derive(Debug, Clone)]
pub struct OutputMetrics {
    /// Frames handed to the display.
    pub frames_submitted: u64,
    /// Rendered frames that had no damage and were not presented.
    pub frames_no_damage: u64,
    /// Renders that did not produce a frame (a paused session, a lost device).
    pub frames_skipped: u64,
    /// Frames that arrived later than one and a half refresh intervals after
    /// the previous one.
    ///
    /// This is the number a user perceives as stutter: a frame that misses its
    /// slot is a visible hitch even when the average rate is fine.
    pub frames_late: u64,
    /// When the last frame was submitted, for a live "how long ago" reading.
    ///
    /// Stored as the instant rather than a running total: a total only advances
    /// when something happens to update it, so an idle output would report a
    /// gap that stopped growing. A reader subtracting from `now` always gets the
    /// truth.
    pub last_frame_at: Option<Instant>,
    /// Duration of the most recent render call, from the event loop's point of
    /// view: element building, drawing and queueing the frame.
    pub last_render: Duration,
    /// Running total of [`Self::last_render`], for an average.
    pub total_render: Duration,
    /// Most recent frame durations, for percentiles.
    recent: [Duration; FRAME_HISTORY],
    /// Write index into [`Self::recent`].
    recent_len: usize,
}

impl Default for OutputMetrics {
    fn default() -> Self {
        return Self {
            frames_submitted: 0,
            frames_no_damage: 0,
            frames_skipped: 0,
            frames_late: 0,
            last_frame_at: None,
            last_render: Duration::ZERO,
            total_render: Duration::ZERO,
            // `[Duration; N]` has no `Default` (Duration is not `Copy`), so the
            // ring is filled by hand.
            recent: [Duration::ZERO; FRAME_HISTORY],
            recent_len: 0,
        };
    }
}

impl OutputMetrics {
    /// Record one render call.
    pub fn record(
        &mut self,
        result: RenderResult,
        render_time: Duration,
        now: Instant,
        frame_interval: Duration,
    ) {
        self.last_render = render_time;
        self.total_render += render_time;
        match result {
            RenderResult::Submitted => {
                // A frame that took longer than a slot and a half since the last
                // one missed its VBlank, which is what stutter looks like.
                //
                // An idle period is not stutter. This compositor only renders
                // when something changed, so coming back from two seconds of
                // nothing is a gap of two seconds and would be counted here on
                // every single interaction — which is how a healthy compositor
                // ends up reporting a quarter of its frames as late. A gap longer
                // than [`IDLE_GAP`] is a period with no frames to drop, so it is
                // not counted at all.
                let gap = self
                    .last_frame_at
                    .map(|last| return now.saturating_duration_since(last))
                    .filter(|gap| return *gap <= IDLE_GAP);
                if gap.is_some_and(|gap| return gap > frame_interval.mul_f64(1.5)) {
                    self.frames_late += 1;
                }
                self.frames_submitted += 1;
                self.last_frame_at = Some(now);
                self.recent[self.recent_len % FRAME_HISTORY] = render_time;
                self.recent_len = self.recent_len.wrapping_add(1);
            }
            RenderResult::NoDamage => self.frames_no_damage += 1,
            RenderResult::Skipped => self.frames_skipped += 1,
        }
    }

    /// How long ago the last frame was submitted.
    pub fn since_last_frame(&self, now: Instant) -> Duration {
        return match self.last_frame_at {
            Some(last) => now.saturating_duration_since(last),
            None => Duration::ZERO,
        };
    }

    /// The mean render time of the recorded frames.
    pub fn mean_render(&self) -> Duration {
        let frames = self.recent_len.min(FRAME_HISTORY);
        if frames == 0 {
            return Duration::ZERO;
        }
        let total: Duration = self.recent[..frames].iter().sum();
        return total / u32::try_from(frames).unwrap_or(1);
    }

    /// A frame-time percentile, `0.0..=1.0`, from the recent history.
    ///
    /// The 95th percentile is the useful one: it is where a hitch shows up even
    /// when the mean looks healthy.
    pub fn percentile(&self, fraction: f64) -> Duration {
        let frames = self.recent_len.min(FRAME_HISTORY);
        if frames == 0 {
            return Duration::ZERO;
        }
        let mut sorted: Vec<Duration> = self.recent[..frames].to_vec();
        sorted.sort_unstable();
        let index =
            ((f64::from(u32::try_from(frames).unwrap_or(1) - 1)) * fraction).round() as usize;
        return sorted[index.min(frames - 1)];
    }
}

/// Capture statistics, accumulated across every session.
#[derive(Debug, Default, Clone)]
pub struct CaptureMetrics {
    /// Frames rendered into a client's buffer.
    pub frames: u64,
    /// Frames that found nothing to change and copied nothing.
    pub unchanged: u64,
    /// Frames whose readback was deferred to a later iteration because the GPU
    /// had not finished.
    pub deferred: u64,
    /// Frames drawn straight into a client's DMA-BUF.
    pub dmabuf_frames: u64,
    /// Frames that could not be produced at all (an error, not a client issue
    /// the protocol reports).
    pub failed: u64,
    /// Bytes copied out of the framebuffer, over the process lifetime.
    pub bytes_read_back: u64,
    /// Running total of the capture cost, for an average.
    pub total_time: Duration,
    /// Duration of the most recent capture.
    pub last_time: Duration,
}

impl CaptureMetrics {
    /// Record one capture frame.
    ///
    /// `None` counts a frame that produced no outcome at all.
    pub fn record(
        &mut self,
        outcome: Option<&crate::core::capture::CaptureOutcome>,
        elapsed: Duration,
        bytes: u64,
    ) {
        use crate::core::capture::CaptureOutcome;
        self.frames += 1;
        self.total_time += elapsed;
        self.last_time = elapsed;
        self.bytes_read_back += bytes;
        match outcome {
            None => self.failed += 1,
            Some(CaptureOutcome::Unchanged) => self.unchanged += 1,
            Some(CaptureOutcome::Deferred) => self.deferred += 1,
            Some(CaptureOutcome::Dmabuf(_)) => self.dmabuf_frames += 1,
            Some(CaptureOutcome::Full(_) | CaptureOutcome::Damage(_)) => (),
        }
    }
}

/// Every output's frame counters.
#[derive(Debug, Default)]
pub struct Metrics {
    outputs: HashMap<Output, OutputMetrics>,
    /// Capture counters, across all sessions.
    pub capture: CaptureMetrics,
}

impl Metrics {
    /// The counters of `output`, created on first use.
    pub fn output(&mut self, output: &Output) -> &mut OutputMetrics {
        return self.outputs.entry(output.clone()).or_default();
    }

    /// Forget an output that went away.
    ///
    /// Its counters are gone with it: a hotplug loop would otherwise grow the
    /// map without bound, and the totals of an output that no longer exists are
    /// not something anyone asked for.
    pub fn remove_output(&mut self, output: &Output) {
        self.outputs.remove(output);
        return;
    }

    /// Every output's counters, for reporting.
    ///
    /// An iterator rather than the map itself: `Output` has interior mutability,
    /// so handing out `&HashMap<Output, _>` would expose a mutable key type to
    /// the caller, and callers only ever want to read the pairs anyway.
    pub fn outputs(&self) -> impl Iterator<Item = (&Output, &OutputMetrics)> {
        return self.outputs.iter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval() -> Duration {
        return Duration::from_micros(16_667);
    }

    #[test]
    fn coming_back_from_idle_is_not_a_dropped_frame() {
        // The compositor renders on demand, so an untouched desktop produces no
        // frames. The first frame after a pause has a gap of seconds, and
        // counting it as late made a healthy session report a quarter of its
        // frames as dropped.
        let start = Instant::now();
        let mut metrics = OutputMetrics::default();
        metrics.record(
            RenderResult::Submitted,
            Duration::from_micros(500),
            start,
            interval(),
        );
        metrics.record(
            RenderResult::Submitted,
            Duration::from_micros(500),
            start + Duration::from_secs(3),
            interval(),
        );
        assert_eq!(metrics.frames_submitted, 2);
        assert_eq!(
            metrics.frames_late, 0,
            "three seconds of nothing is an idle period, not a missed VBlank"
        );
    }

    #[test]
    fn a_stall_inside_the_idle_window_is_still_late() {
        // The other side of the same rule: a genuine stall short enough to be a
        // stall rather than a pause must still be counted.
        let start = Instant::now();
        let mut metrics = OutputMetrics::default();
        metrics.record(
            RenderResult::Submitted,
            Duration::from_micros(500),
            start,
            interval(),
        );
        metrics.record(
            RenderResult::Submitted,
            Duration::from_micros(500),
            start + IDLE_GAP - Duration::from_millis(1),
            interval(),
        );
        assert_eq!(metrics.frames_late, 1);
    }

    #[test]
    fn a_frame_arriving_after_more_than_a_slot_is_late() {
        let start = Instant::now();
        let mut metrics = OutputMetrics::default();
        metrics.record(
            RenderResult::Submitted,
            Duration::from_millis(2),
            start,
            interval(),
        );
        assert_eq!(metrics.frames_submitted, 1);
        assert_eq!(metrics.frames_late, 0, "the first frame has no predecessor");

        // One slot later: on time.
        metrics.record(
            RenderResult::Submitted,
            Duration::from_millis(2),
            start + Duration::from_millis(16),
            interval(),
        );
        assert_eq!(metrics.frames_late, 0);

        // Nearly two slots: the frame missed its VBlank.
        metrics.record(
            RenderResult::Submitted,
            Duration::from_millis(2),
            start + Duration::from_millis(46),
            interval(),
        );
        assert_eq!(metrics.frames_late, 1);
        // The gap is measured from the last submitted frame, whenever asked.
        assert_eq!(
            metrics.since_last_frame(start + Duration::from_millis(100)),
            Duration::from_millis(54)
        );
    }

    #[test]
    fn outcomes_are_counted_separately() {
        let mut metrics = OutputMetrics::default();
        metrics.record(
            RenderResult::Submitted,
            Duration::ZERO,
            Instant::now(),
            interval(),
        );
        metrics.record(
            RenderResult::NoDamage,
            Duration::ZERO,
            Instant::now(),
            interval(),
        );
        metrics.record(
            RenderResult::Skipped,
            Duration::ZERO,
            Instant::now(),
            interval(),
        );
        assert_eq!(metrics.frames_submitted, 1);
        assert_eq!(metrics.frames_no_damage, 1);
        assert_eq!(metrics.frames_skipped, 1);
    }

    #[test]
    fn percentiles_need_no_frames_to_report_zero() {
        let metrics = OutputMetrics::default();
        assert_eq!(metrics.mean_render(), Duration::ZERO);
        assert_eq!(metrics.percentile(0.95), Duration::ZERO);
    }

    #[test]
    fn the_p95_is_the_slow_frames_not_the_mean() {
        let mut metrics = OutputMetrics::default();
        // Ninety cheap frames and ten slow ones. The mean stays low, the 95th
        // percentile does not: that is the whole reason to look at it.
        for _ in 0..90 {
            metrics.record(
                RenderResult::Submitted,
                Duration::from_micros(500),
                Instant::now(),
                interval(),
            );
        }
        for _ in 0..10 {
            metrics.record(
                RenderResult::Submitted,
                Duration::from_millis(40),
                Instant::now(),
                interval(),
            );
        }
        assert!(metrics.mean_render() < Duration::from_millis(5));
        assert!(metrics.percentile(0.95) >= Duration::from_millis(40));
    }

    #[test]
    fn capture_counters_follow_the_outcome() {
        use crate::core::capture::CaptureOutcome;
        use smithay::utils::{Buffer, Size};

        let mut metrics = CaptureMetrics::default();
        metrics.record(
            Some(&CaptureOutcome::Full(Size::<i32, Buffer>::from((
                1920, 1080,
            )))),
            Duration::from_millis(3),
            1920 * 1080 * 4,
        );
        metrics.record(Some(&CaptureOutcome::Unchanged), Duration::ZERO, 0);
        metrics.record(Some(&CaptureOutcome::Deferred), Duration::ZERO, 0);
        metrics.record(
            Some(&CaptureOutcome::Dmabuf(Size::<i32, Buffer>::from((
                1920, 1080,
            )))),
            Duration::ZERO,
            0,
        );
        // A frame that produced no outcome at all is not an unchanged frame:
        // counting it as one would make a broken capture look healthy.
        metrics.record(None, Duration::ZERO, 0);
        assert_eq!(metrics.frames, 5);
        assert_eq!(metrics.unchanged, 1);
        assert_eq!(metrics.deferred, 1);
        assert_eq!(metrics.dmabuf_frames, 1);
        assert_eq!(metrics.failed, 1);
        assert_eq!(metrics.bytes_read_back, 1920 * 1080 * 4);
    }
}
