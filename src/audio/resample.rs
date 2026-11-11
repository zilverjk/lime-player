use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

use super::rate_repair::scaled_frames;

const CHUNK_FRAMES: usize = 1_024;
const CHANNELS: usize = 2;

pub struct StreamResampler {
    inner: Option<FftState>,
}

struct FftState {
    resampler: Fft<f64>,
    from_rate: u32,
    to_rate: u32,
    input: Vec<f64>,
    chunk: Vec<f64>,
    output: Vec<f64>,
    frames_to_trim: usize,
    frames_in: u64,
    frames_out: u64,
    expected_out: Option<u64>,
}

impl StreamResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Result<Self, String> {
        if from_rate == 0 || to_rate == 0 {
            return Err(format!("invalid resampling rates {from_rate} -> {to_rate} Hz"));
        }
        if from_rate == to_rate {
            return Ok(Self { inner: None });
        }
        let resampler = Fft::<f64>::new(from_rate as usize, to_rate as usize, CHUNK_FRAMES, CHANNELS, FixedSync::Input)
            .map_err(|error| format!("could not create the resampler: {error}"))?;
        let frames_to_trim = resampler.output_delay();
        let output = vec![0.0; resampler.output_frames_max() * CHANNELS];
        Ok(Self {
            inner: Some(FftState {
                resampler,
                from_rate,
                to_rate,
                input: Vec::with_capacity(CHUNK_FRAMES * 2 * CHANNELS),
                chunk: Vec::with_capacity(CHUNK_FRAMES * CHANNELS),
                output,
                frames_to_trim,
                frames_in: 0,
                frames_out: 0,
                expected_out: None,
            }),
        })
    }

    #[cfg(test)]
    pub fn is_bypass(&self) -> bool {
        self.inner.is_none()
    }

    pub fn push(&mut self, sample: f32, emit: &mut impl FnMut(f32) -> Result<(), String>) -> Result<(), String> {
        let Some(state) = self.inner.as_mut() else {
            return emit(sample);
        };
        state.input.push(f64::from(sample));
        let needed = state.resampler.input_frames_next();
        if state.input.len() >= needed * CHANNELS {
            state.process(needed, emit)?;
        }
        Ok(())
    }

    pub fn finish(&mut self, emit: &mut impl FnMut(f32) -> Result<(), String>) -> Result<(), String> {
        let Some(state) = self.inner.as_mut() else {
            return Ok(());
        };
        state.input.truncate(state.input.len() / CHANNELS * CHANNELS);
        let total_in = state.frames_in + (state.input.len() / CHANNELS) as u64;
        let expected = scaled_frames(total_in, state.to_rate, state.from_rate);
        state.expected_out = Some(expected);
        while state.frames_out < expected {
            let needed = state.resampler.input_frames_next();
            let valid = (state.input.len() / CHANNELS).min(needed);
            let before = state.frames_out;
            state.process(valid, emit)?;
            if state.frames_out == before && valid == 0 && state.frames_to_trim == 0 {
                break;
            }
        }
        Ok(())
    }
}

impl FftState {
    fn process(&mut self, valid_frames: usize, emit: &mut impl FnMut(f32) -> Result<(), String>) -> Result<(), String> {
        let needed = self.resampler.input_frames_next();
        self.chunk.clear();
        self.chunk.extend(self.input.drain(..valid_frames * CHANNELS));
        self.chunk.resize(needed * CHANNELS, 0.0);
        self.frames_in += valid_frames as u64;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: None,
            partial_len: (valid_frames < needed).then_some(valid_frames),
        };
        let input_adapter = InterleavedSlice::new(&self.chunk, CHANNELS, needed).map_err(|error| error.to_string())?;
        let capacity = self.output.len() / CHANNELS;
        let mut output_adapter =
            InterleavedSlice::new_mut(&mut self.output, CHANNELS, capacity).map_err(|error| error.to_string())?;
        let (_, produced) = self
            .resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .map_err(|error| error.to_string())?;

        let skip = self.frames_to_trim.min(produced);
        self.frames_to_trim -= skip;
        let mut keep = produced - skip;
        if let Some(expected) = self.expected_out {
            keep = keep.min(expected.saturating_sub(self.frames_out) as usize);
        }
        let start = skip * CHANNELS;
        for value in &self.output[start..start + keep * CHANNELS] {
            emit(*value as f32)?;
        }
        self.frames_out += keep as u64;
        Ok(())
    }
}
