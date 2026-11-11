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
