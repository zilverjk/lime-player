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

#[cfg(test)]
mod tests {
    use super::*;

    fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
        let mut resampler = StreamResampler::new(from, to).unwrap();
        let mut output = Vec::new();
        let mut emit = |sample: f32| {
            output.push(sample);
            Ok(())
        };
        for sample in input {
            resampler.push(*sample, &mut emit).unwrap();
        }
        resampler.finish(&mut emit).unwrap();
        output
    }

    fn stereo_sine(frequency: f64, rate: u32, frames: usize, amplitude: f64) -> Vec<f32> {
        (0..frames)
            .flat_map(|frame| {
                let value = (amplitude * (2.0 * std::f64::consts::PI * frequency * frame as f64 / f64::from(rate)).sin()) as f32;
                [value, value]
            })
            .collect()
    }

    #[test]
    fn output_length_follows_the_rate_ratio() {
        for (from, to, frames) in [(44_100, 48_000, 44_100), (48_000, 44_100, 48_000), (44_100, 96_000, 10_000), (96_000, 48_000, 777)] {
            let output = resample(&vec![0.25; frames * 2], from, to);
            assert_eq!(output.len() % 2, 0, "{from} -> {to}: whole stereo frames only");
            let expected = (frames as u64 * u64::from(to) + u64::from(from) / 2) / u64::from(from);
            let produced = (output.len() / 2) as u64;
            assert!(produced.abs_diff(expected) <= 2, "{from} -> {to}: {produced} frames, expected about {expected}");
        }
    }

    #[test]
    fn equal_rates_are_a_bit_exact_pass_through() {
        let input: Vec<f32> = (0..1_001).map(|index| index as f32 / 1_000.0 - 0.5).collect();
        assert!(StreamResampler::new(48_000, 48_000).unwrap().is_bypass());
        assert_eq!(resample(&input, 48_000, 48_000), input, "no conversion at all when the rates match");
    }

    #[test]
    fn a_stream_shorter_than_one_chunk_is_still_flushed() {
        let output = resample(&vec![0.5; 100 * 2], 44_100, 48_000);
        let frames = output.len() / 2;
        assert!(frames.abs_diff(109) <= 2, "the tail must be flushed, got {frames} frames");
        assert!(output.iter().any(|sample| *sample != 0.0), "the flushed tail holds the input, not just silence");
    }

    #[test]
    fn invalid_rates_are_refused() {
        assert!(StreamResampler::new(0, 48_000).is_err());
        assert!(StreamResampler::new(44_100, 0).is_err());
    }

    #[test]
    fn a_sine_keeps_its_frequency_and_amplitude() {
        let (from, to, frequency, amplitude) = (44_100, 48_000, 1_000.0, 0.5);
        let output = resample(&stereo_sine(frequency, from, from as usize, amplitude), from, to);
        let left: Vec<f64> = output.iter().step_by(2).map(|sample| f64::from(*sample)).collect();
        // The filter rings at the stream edges.
        let middle = &left[4_800..left.len() - 4_800];
        let peak = middle.iter().fold(0.0f64, |peak, value| peak.max(value.abs()));
        assert!((peak - amplitude).abs() < 0.02, "amplitude {peak}, expected {amplitude}");
        let crossings = middle.windows(2).filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0).count();
        let measured = crossings as f64 * f64::from(to) / middle.len() as f64;
        assert!((measured - frequency).abs() < 10.0, "frequency {measured} Hz, expected {frequency} Hz");
        let right: Vec<f32> = output.iter().skip(1).step_by(2).copied().collect();
        let left_f32: Vec<f32> = output.iter().step_by(2).copied().collect();
        assert_eq!(left_f32, right);
    }

    #[test]
    fn an_emit_error_stops_the_stream() {
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let mut emit = |_sample: f32| Err::<(), String>("cancelled".into());
        let mut result = Ok(());
        for _ in 0..CHUNK_FRAMES * 4 {
            result = resampler.push(0.1, &mut emit);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err("cancelled".into()));
    }
}
