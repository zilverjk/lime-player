use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use cpal::StreamInstant;
use rtrb::{Consumer, Producer, RingBuffer};

use super::{
    AudioDiagnostics, MISMATCH_FLOAT_IN_I32, MISMATCH_INTEGER_IN_F32, mark_first_output_pcm_playback_time,
    record_output_samples_consumed,
};
use crate::audio::decoder::PcmSample;

const UPCOMING_CAPACITY: usize = 2;
/// Room for every feed the callback can hold, so retiring never waits.
const FINISHED_CAPACITY: usize = UPCOMING_CAPACITY + 2;

const HANDOVER_PENDING: u8 = 0;
const HANDOVER_STARTED: u8 = 1;
const HANDOVER_WITHDRAWN: u8 = 2;

/// Callback starts, controller withdraws: the first compare-exchange wins.
#[derive(Clone)]
pub(super) struct HandoverState(Arc<AtomicU8>);

impl HandoverState {
    pub(super) fn pending() -> Self {
        Self(Arc::new(AtomicU8::new(HANDOVER_PENDING)))
    }

    pub(super) fn started() -> Self {
        Self(Arc::new(AtomicU8::new(HANDOVER_STARTED)))
    }

    fn try_start(&self) -> bool {
        self.0
            .compare_exchange(HANDOVER_PENDING, HANDOVER_STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn try_withdraw(&self) -> bool {
        self.0
            .compare_exchange(HANDOVER_PENDING, HANDOVER_WITHDRAWN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn has_started(&self) -> bool {
        self.0.load(Ordering::Acquire) == HANDOVER_STARTED
    }

    fn is_withdrawn(&self) -> bool {
        self.0.load(Ordering::Acquire) == HANDOVER_WITHDRAWN
    }
}

#[derive(Clone)]
pub(super) struct TrackSignals {
    pub(super) buffered: Arc<AtomicUsize>,
    pub(super) eof: Arc<AtomicBool>,
    pub(super) drained: Arc<AtomicBool>,
    pub(super) diagnostics: Arc<AudioDiagnostics>,
    pub(super) consumed_output_samples: Arc<AtomicU64>,
    pub(super) handover: HandoverState,
}

pub(super) struct TrackFeed {
    consumer: Consumer<PcmSample>,
    signals: TrackSignals,
}

impl TrackFeed {
    pub(super) fn new(consumer: Consumer<PcmSample>, signals: TrackSignals) -> Self {
        Self { consumer, signals }
    }

    pub(super) fn into_consumer(self) -> Consumer<PcmSample> {
        self.consumer
    }

    /// The decoder's last samples can land between a failed pop and `eof`.
    fn is_finished(&self) -> bool {
        self.signals.eof.load(Ordering::Acquire) && self.consumer.is_empty()
    }
}

pub(super) trait OutputSample: Copy + Send + 'static {
    const SILENCE: Self;
    const MISMATCH: u8;
    fn from_pcm(sample: PcmSample) -> Option<Self>;
    fn is_nonzero(self) -> bool;
}

impl OutputSample for i32 {
    const SILENCE: Self = 0;
    const MISMATCH: u8 = MISMATCH_FLOAT_IN_I32;

    fn from_pcm(sample: PcmSample) -> Option<Self> {
        match sample {
            PcmSample::Integer(value) => Some(value),
            PcmSample::Float(_) => None,
        }
    }

    fn is_nonzero(self) -> bool {
        self != 0
    }
}

impl OutputSample for f32 {
    const SILENCE: Self = 0.0;
    const MISMATCH: u8 = MISMATCH_INTEGER_IN_F32;

    fn from_pcm(sample: PcmSample) -> Option<Self> {
        match sample {
            PcmSample::Float(value) => Some(value),
            PcmSample::Integer(_) => None,
        }
    }

    fn is_nonzero(self) -> bool {
        self != 0.0
    }
}

#[derive(Default)]
struct FeedTally {
    delivered: u64,
    underrun: u64,
    nonzero: bool,
}

impl FeedTally {
    fn publish(self, signals: &TrackSignals, playback_time: StreamInstant) {
        let diagnostics = &signals.diagnostics;
        if self.nonzero {
            diagnostics.callback_nonzero_sample.store(true, Ordering::Release);
        }
        mark_first_output_pcm_playback_time(
            &diagnostics.first_output_timestamp_state,
            &diagnostics.first_output_playback_secs,
            &diagnostics.first_output_playback_nanos,
            playback_time,
            self.delivered,
        );
        record_output_samples_consumed(&signals.consumed_output_samples, self.delivered);
        if self.underrun > 0 {
            diagnostics.underrun_samples.fetch_add(self.underrun, Ordering::AcqRel);
        }
    }
}

pub(super) struct TrackChain {
    current: TrackFeed,
    upcoming: Consumer<TrackFeed>,
    finished: Producer<TrackFeed>,
    preroll_remaining_samples: usize,
    output_failed: Arc<AtomicBool>,
    format_mismatch: Arc<AtomicU8>,
}

pub(super) struct ChainHandle {
    upcoming: Producer<TrackFeed>,
    finished: Consumer<TrackFeed>,
}

impl ChainHandle {
    pub(super) fn queue(&mut self, feed: TrackFeed) -> Result<(), TrackFeed> {
        self.upcoming.push(feed).map_err(|rtrb::PushError::Full(feed)| feed)
    }

    pub(super) fn release_finished(&mut self) {
        while self.finished.pop().is_ok() {}
    }
}

impl TrackChain {
    pub(super) fn new(
        first: TrackFeed,
        preroll_samples: usize,
        output_failed: Arc<AtomicBool>,
        format_mismatch: Arc<AtomicU8>,
    ) -> (Self, ChainHandle) {
        let (upcoming_producer, upcoming) = RingBuffer::new(UPCOMING_CAPACITY);
        let (finished, finished_consumer) = RingBuffer::new(FINISHED_CAPACITY);
        let chain = Self {
            current: first,
            upcoming,
            finished,
            preroll_remaining_samples: preroll_samples,
            output_failed,
            format_mismatch,
        };
        (chain, ChainHandle { upcoming: upcoming_producer, finished: finished_consumer })
    }

    /// Real-time safe: no lock, allocation, deallocation or blocking.
    pub(super) fn fill<S: OutputSample>(&mut self, out: &mut [S], playback_time: StreamInstant) {
        self.retire_withdrawn_upcoming();
        let mut tally = FeedTally::default();
        let mut written = 0;
        while written < out.len() {
            if self.preroll_remaining_samples > 0 {
                out[written] = S::SILENCE;
                self.preroll_remaining_samples -= 1;
                written += 1;
                continue;
            }
            if let Ok(sample) = self.current.consumer.pop() {
                self.current.signals.buffered.fetch_sub(1, Ordering::Release);
                out[written] = match S::from_pcm(sample) {
                    Some(value) => {
                        tally.delivered += 1;
                        tally.nonzero |= value.is_nonzero();
                        value
                    }
                    None => {
                        self.report_mismatch(S::MISMATCH);
                        S::SILENCE
                    }
                };
                written += 1;
            } else if !self.current.signals.eof.load(Ordering::Acquire) {
                out[written] = S::SILENCE;
                tally.underrun += 1;
                written += 1;
            } else if self.current.is_finished() {
                self.current.signals.drained.store(true, Ordering::Release);
                if !self.advance(&mut tally, playback_time) {
                    out[written..].fill(S::SILENCE);
                    break;
                }
            }
        }
        tally.publish(&self.current.signals, playback_time);
    }

    fn advance(&mut self, tally: &mut FeedTally, playback_time: StreamInstant) -> bool {
        self.retire_withdrawn_upcoming();
        if self.finished.is_full() {
            return false;
        }
        let claimed = self.upcoming.peek().is_ok_and(|next| next.signals.handover.try_start());
        if !claimed {
            return false;
        }
        let Ok(next) = self.upcoming.pop() else { return false };
        std::mem::take(tally).publish(&self.current.signals, playback_time);
        let done = std::mem::replace(&mut self.current, next);
        self.retire(done);
        true
    }

    fn retire_withdrawn_upcoming(&mut self) {
        while !self.finished.is_full()
            && self.upcoming.peek().is_ok_and(|next| next.signals.handover.is_withdrawn())
        {
            if let Ok(withdrawn) = self.upcoming.pop() {
                self.retire(withdrawn);
            }
        }
    }

    fn retire(&mut self, feed: TrackFeed) {
        if let Err(rtrb::PushError::Full(feed)) = self.finished.push(feed) {
            std::mem::forget(feed);
        }
    }

    fn report_mismatch(&self, code: u8) {
        self.format_mismatch.store(code, Ordering::Relaxed);
        self.output_failed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTrack {
        producer: Producer<PcmSample>,
        signals: TrackSignals,
        feed: Option<TrackFeed>,
    }

    impl TestTrack {
        fn new(handover: HandoverState) -> Self {
            let (producer, consumer) = RingBuffer::new(64);
            let signals = TrackSignals {
                buffered: Arc::new(AtomicUsize::new(0)),
                eof: Arc::new(AtomicBool::new(false)),
                drained: Arc::new(AtomicBool::new(false)),
                diagnostics: Arc::new(AudioDiagnostics::default()),
                consumed_output_samples: Arc::new(AtomicU64::new(0)),
                handover,
            };
            let feed = Some(TrackFeed::new(consumer, signals.clone()));
            Self { producer, signals, feed }
        }

        fn with_samples(handover: HandoverState, samples: impl IntoIterator<Item = i32>, eof: bool) -> Self {
            let mut track = Self::new(handover);
            for sample in samples {
                track.producer.push(PcmSample::Integer(sample)).unwrap();
                track.signals.buffered.fetch_add(1, Ordering::Release);
            }
            track.signals.eof.store(eof, Ordering::Release);
            track
        }

        fn feed(&mut self) -> TrackFeed {
            self.feed.take().unwrap()
        }

        fn consumed(&self) -> u64 {
            self.signals.consumed_output_samples.load(Ordering::Acquire)
        }

        fn underruns(&self) -> u64 {
            self.signals.diagnostics.underrun_samples.load(Ordering::Acquire)
        }

        fn drained(&self) -> bool {
            self.signals.drained.load(Ordering::Acquire)
        }
    }

    fn chain_playing(first: &mut TestTrack) -> (TrackChain, ChainHandle) {
        TrackChain::new(first.feed(), 0, Arc::new(AtomicBool::new(false)), Arc::new(AtomicU8::new(0)))
    }

    fn fill(chain: &mut TrackChain, len: usize) -> Vec<i32> {
        let mut out = vec![-1; len];
        chain.fill(&mut out, StreamInstant::new(0, 0));
        out
    }

    fn finished_count(handle: &mut ChainHandle) -> usize {
        std::iter::from_fn(|| handle.finished.pop().ok()).count()
    }

    #[test]
    fn the_next_track_follows_within_the_same_buffer_with_nothing_lost_or_inserted() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [1, 2, 3], true);
        let mut second = TestTrack::with_samples(HandoverState::pending(), [10, 11, 12, 13], false);
        let (mut chain, mut handle) = chain_playing(&mut first);
        assert!(handle.queue(second.feed()).is_ok());

        assert_eq!(fill(&mut chain, 6), vec![1, 2, 3, 10, 11, 12]);

        assert!(first.drained());
        assert!(second.signals.handover.has_started());
        assert_eq!((first.consumed(), second.consumed()), (3, 3));
        assert_eq!((first.underruns(), second.underruns()), (0, 0), "the boundary is not an underrun");
        assert_eq!(second.signals.buffered.load(Ordering::Acquire), 1);
        assert_eq!(finished_count(&mut handle), 1, "the finished track goes back to the controller");
    }

    #[test]
    fn a_finished_track_with_nothing_queued_drains_into_silence_without_underruns() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [5, 6], true);
        let (mut chain, mut handle) = chain_playing(&mut first);

        assert_eq!(fill(&mut chain, 4), vec![5, 6, 0, 0]);
        assert_eq!(fill(&mut chain, 2), vec![0, 0]);

        assert!(first.drained());
        assert_eq!(first.consumed(), 2);
        assert_eq!(first.underruns(), 0);
        assert_eq!(finished_count(&mut handle), 0, "the playing track stays put");
    }

    #[test]
    fn an_empty_ring_before_eof_is_an_underrun_of_that_track() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [5], false);
        let (mut chain, _handle) = chain_playing(&mut first);

        assert_eq!(fill(&mut chain, 3), vec![5, 0, 0]);

        assert!(!first.drained());
        assert_eq!(first.underruns(), 2);
    }

    #[test]
    fn a_track_queued_after_the_drain_starts_on_the_next_buffer() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [1], true);
        let mut second = TestTrack::with_samples(HandoverState::pending(), [7, 8], true);
        let (mut chain, mut handle) = chain_playing(&mut first);
        assert_eq!(fill(&mut chain, 2), vec![1, 0]);

        assert!(handle.queue(second.feed()).is_ok());

        assert_eq!(fill(&mut chain, 3), vec![7, 8, 0]);
        assert!(second.drained());
        assert_eq!(first.underruns() + second.underruns(), 0);
    }

    #[test]
    fn a_withdrawn_track_is_never_played_and_goes_back_for_dropping() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [1, 2], true);
        let mut second = TestTrack::with_samples(HandoverState::pending(), [9, 9], true);
        let (mut chain, mut handle) = chain_playing(&mut first);
        assert!(handle.queue(second.feed()).is_ok());

        assert!(second.signals.handover.try_withdraw());
        assert_eq!(fill(&mut chain, 4), vec![1, 2, 0, 0]);

        assert_eq!(second.consumed(), 0);
        assert!(!second.signals.handover.has_started());
        assert_eq!(finished_count(&mut handle), 1);
    }

    #[test]
    fn a_withdrawn_track_is_skipped_for_the_one_queued_after_it() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [1], true);
        let mut withdrawn = TestTrack::with_samples(HandoverState::pending(), [9], true);
        let mut replacement = TestTrack::with_samples(HandoverState::pending(), [4, 5], false);
        let (mut chain, mut handle) = chain_playing(&mut first);
        assert!(handle.queue(withdrawn.feed()).is_ok());
        assert!(withdrawn.signals.handover.try_withdraw());
        assert!(handle.queue(replacement.feed()).is_ok());

        assert_eq!(fill(&mut chain, 3), vec![1, 4, 5]);
        assert_eq!(finished_count(&mut handle), 2, "the withdrawn track and the finished one");
    }

    #[test]
    fn a_started_track_can_no_longer_be_withdrawn() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [1], true);
        let mut second = TestTrack::with_samples(HandoverState::pending(), [2, 3], false);
        let (mut chain, mut handle) = chain_playing(&mut first);
        assert!(handle.queue(second.feed()).is_ok());
        fill(&mut chain, 2);

        assert!(!second.signals.handover.try_withdraw());
        assert_eq!(fill(&mut chain, 1), vec![3]);
    }

    #[test]
    fn preroll_silence_comes_first_and_is_not_counted() {
        let mut first = TestTrack::with_samples(HandoverState::started(), [1, 2], false);
        let (mut chain, _handle) =
            TrackChain::new(first.feed(), 2, Arc::new(AtomicBool::new(false)), Arc::new(AtomicU8::new(0)));

        assert_eq!(fill(&mut chain, 4), vec![0, 0, 1, 2]);
        assert_eq!(first.consumed(), 2);
        assert_eq!(first.underruns(), 0);
    }

    #[test]
    fn the_wrong_sample_type_fails_the_output_and_plays_silence() {
        let mut first = TestTrack::new(HandoverState::started());
        first.producer.push(PcmSample::Float(0.5)).unwrap();
        let output_failed = Arc::new(AtomicBool::new(false));
        let format_mismatch = Arc::new(AtomicU8::new(0));
        let (mut chain, _handle) = TrackChain::new(first.feed(), 0, output_failed.clone(), format_mismatch.clone());

        assert_eq!(fill(&mut chain, 1), vec![0]);
        assert!(output_failed.load(Ordering::Acquire));
        assert_eq!(format_mismatch.load(Ordering::Acquire), MISMATCH_FLOAT_IN_I32);
        assert_eq!(first.consumed(), 0);
    }
}
