use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use ebur128::{EbuR128, Mode};
use rodio::Source;

use super::{AudioMetadata, decode_audio};

const WAVEFORM_PEAK_COUNT: usize = 1_536;
const SILENCE_WINDOW: Duration = Duration::from_millis(20);
const SILENCE_THRESHOLD_DBFS: f64 = -50.0;
const NEAR_CLIP_THRESHOLD_DBFS: f64 = -0.1;
const MAXIMUM_CLICK_TIMESTAMPS: usize = 20;
const CLICK_ABSOLUTE_FLOOR: f32 = 0.25;
const CLICK_CONTEXT_RATIO: f32 = 6.0;
const BOUNDARY_IMMEDIATE_WINDOW: Duration = Duration::from_millis(4);
const BOUNDARY_CONTEXT_WINDOW: Duration = Duration::from_millis(16);
const BOUNDARY_MINIMUM_SIGNAL_DBFS: f64 = -60.0;
const BOUNDARY_MINIMUM_CONTRAST_DB: f64 = 15.0;
const BOUNDARY_MINIMUM_CONTEXT_RATIO: f64 = 0.5;
const DISCONTINUITY_MERGE_TOLERANCE: Duration = Duration::from_millis(2);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct WaveformBucket {
    pub(super) minimum: f32,
    pub(super) maximum: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SilenceRange {
    pub(super) start: Duration,
    pub(super) end: Duration,
}

impl SilenceRange {
    fn duration(self) -> Duration {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Clone, Debug)]
pub(super) struct AudioMetrics {
    pub(super) active_speech_rms_dbfs: Option<f64>,
    pub(super) integrated_lufs: Option<f64>,
    pub(super) sample_peak_dbfs: Option<f64>,
    pub(super) true_peak_dbtp: Option<f64>,
    pub(super) dc_offsets: Arc<Vec<f64>>,
    pub(super) near_clipped_samples: usize,
    pub(super) total_samples: usize,
    pub(super) leading_silence: Duration,
    pub(super) trailing_silence: Duration,
    pub(super) internal_pause_count: usize,
    pub(super) internal_pause_total: Duration,
    pub(super) internal_pause_longest: Duration,
    pub(super) possible_click_count: usize,
    pub(super) possible_click_timestamps: Arc<Vec<Duration>>,
}

#[derive(Debug)]
pub(super) struct AudioAnalysis {
    pub(super) metadata: AudioMetadata,
    pub(super) waveform: Arc<Vec<WaveformBucket>>,
    pub(super) silence_ranges: Arc<Vec<SilenceRange>>,
    pub(super) metrics: Arc<AudioMetrics>,
}

pub(super) fn analyze_audio(
    bytes: Arc<Vec<u8>>,
    format_hint: Option<String>,
) -> Result<AudioAnalysis> {
    let file_size = bytes.len() as u64;
    let decoder = decode_audio(bytes.clone(), format_hint.as_deref())?;
    let channels = decoder.channels().get();
    let sample_rate = decoder.sample_rate().get();
    let reported_duration =
        wav_duration_from_bytes(bytes.as_slice()).or_else(|| decoder.total_duration());
    let samples = decoder.collect::<Vec<_>>();
    let channel_count = usize::from(channels.max(1));
    let frame_count = samples.len() / channel_count;
    let decoded_duration = Duration::from_secs_f64(frame_count as f64 / f64::from(sample_rate));
    let duration = reported_duration.or(Some(decoded_duration));
    let waveform = extract_waveform(&samples, channel_count, frame_count);
    let (silence_ranges, active_windows) = segment_silence(&samples, sample_rate, channel_count);

    let metrics = analyze_metrics(
        &samples,
        sample_rate,
        channel_count,
        &silence_ranges,
        &active_windows,
    )?;

    Ok(AudioAnalysis {
        metadata: AudioMetadata {
            file_size,
            duration,
            channels: Some(channels),
            sample_rate: Some(sample_rate),
        },
        waveform: Arc::new(waveform),
        silence_ranges: Arc::new(silence_ranges),
        metrics: Arc::new(metrics),
    })
}

fn extract_waveform(
    samples: &[f32],
    channel_count: usize,
    frame_count: usize,
) -> Vec<WaveformBucket> {
    if frame_count == 0 {
        return Vec::new();
    }
    let bucket_count = WAVEFORM_PEAK_COUNT.min(frame_count);
    let mut buckets = vec![WaveformBucket::default(); bucket_count];
    for (sample_index, sample) in samples.iter().copied().enumerate() {
        let frame_index = sample_index / channel_count;
        let bucket_index = frame_index.saturating_mul(bucket_count) / frame_count;
        if let Some(bucket) = buckets.get_mut(bucket_index) {
            bucket.minimum = bucket.minimum.min(sample);
            bucket.maximum = bucket.maximum.max(sample);
        }
    }
    buckets
}

fn segment_silence(
    samples: &[f32],
    sample_rate: u32,
    channel_count: usize,
) -> (Vec<SilenceRange>, Vec<bool>) {
    let frames_per_window =
        ((u64::from(sample_rate) * SILENCE_WINDOW.as_millis() as u64) / 1_000).max(1) as usize;
    let frame_count = samples.len() / channel_count;
    let window_count = frame_count.div_ceil(frames_per_window);
    let threshold = 10_f64.powf(SILENCE_THRESHOLD_DBFS / 20.0);
    let mut silent_windows = Vec::with_capacity(window_count);
    for window_index in 0..window_count {
        let start_frame = window_index * frames_per_window;
        let end_frame = ((window_index + 1) * frames_per_window).min(frame_count);
        let start_sample = start_frame * channel_count;
        let end_sample = end_frame * channel_count;
        let window_samples = samples.get(start_sample..end_sample).unwrap_or_default();
        let rms = linear_rms(window_samples).unwrap_or_default();
        silent_windows.push(rms < threshold);
    }

    let mut ranges = Vec::new();
    let mut window_index = 0;
    while window_index < silent_windows.len() {
        if !silent_windows[window_index] {
            window_index += 1;
            continue;
        }
        let start = window_index;
        while window_index < silent_windows.len() && silent_windows[window_index] {
            window_index += 1;
        }
        let end_frame = (window_index * frames_per_window).min(frame_count);
        ranges.push(SilenceRange {
            start: Duration::from_secs_f64(
                (start * frames_per_window) as f64 / f64::from(sample_rate),
            ),
            end: Duration::from_secs_f64(end_frame as f64 / f64::from(sample_rate)),
        });
    }
    let active_windows = silent_windows.into_iter().map(|silent| !silent).collect();
    (ranges, active_windows)
}

// Speech can contain large legitimate slopes, so candidates must clear both a fixed floor and
// the surrounding channel's slope context before adjacent/channel detections are consolidated.
fn detect_possible_clicks(
    samples: &[f32],
    sample_rate: u32,
    channel_count: usize,
    silence_ranges: &[SilenceRange],
) -> (usize, Vec<Duration>) {
    let frame_count = samples.len() / channel_count;
    let boundary_candidates = hard_silence_boundary_candidates(
        samples,
        sample_rate,
        channel_count,
        silence_ranges,
        frame_count,
    );
    let merge_tolerance_frames = duration_frames(DISCONTINUITY_MERGE_TOLERANCE, sample_rate);
    let mut total = 0usize;
    let mut timestamps = Vec::with_capacity(MAXIMUM_CLICK_TIMESTAMPS);
    let mut previous_event_frame = None;
    for frame in 0..=frame_count {
        let mut frame_has_candidate = boundary_candidates.binary_search(&frame).is_ok();
        if frame > 0 && frame < frame_count {
            for channel in 0..channel_count {
                let Some(current) = samples.get(frame * channel_count + channel).copied() else {
                    continue;
                };
                let Some(previous) = samples.get((frame - 1) * channel_count + channel).copied()
                else {
                    continue;
                };
                let slope = (current - previous).abs();
                if slope < CLICK_ABSOLUTE_FLOOR {
                    continue;
                }
                let context_start = frame.saturating_sub(9).max(1);
                let context_end = (frame + 9).min(frame_count);
                let mut context_sum = 0.0_f32;
                let mut context_count = 0usize;
                for context_frame in context_start..context_end {
                    if context_frame.abs_diff(frame) <= 1 {
                        continue;
                    }
                    let Some(context_current) = samples
                        .get(context_frame * channel_count + channel)
                        .copied()
                    else {
                        continue;
                    };
                    let Some(context_previous) = samples
                        .get((context_frame - 1) * channel_count + channel)
                        .copied()
                    else {
                        continue;
                    };
                    context_sum += (context_current - context_previous).abs();
                    context_count += 1;
                }
                let context = context_sum / context_count.max(1) as f32;
                if slope > context.max(0.01) * CLICK_CONTEXT_RATIO {
                    frame_has_candidate = true;
                    break;
                }
            }
        }
        if !frame_has_candidate
            || previous_event_frame.is_some_and(|previous: usize| {
                frame <= previous.saturating_add(merge_tolerance_frames)
            })
        {
            continue;
        }
        total += 1;
        previous_event_frame = Some(frame);
        if timestamps.len() < MAXIMUM_CLICK_TIMESTAMPS {
            timestamps.push(Duration::from_secs_f64(
                frame as f64 / f64::from(sample_rate),
            ));
        }
    }
    (total, timestamps)
}

fn hard_silence_boundary_candidates(
    samples: &[f32],
    sample_rate: u32,
    channel_count: usize,
    silence_ranges: &[SilenceRange],
    frame_count: usize,
) -> Vec<usize> {
    let immediate_frames = duration_frames(BOUNDARY_IMMEDIATE_WINDOW, sample_rate);
    let context_frames = duration_frames(BOUNDARY_CONTEXT_WINDOW, sample_rate);
    let minimum_signal = 10_f64.powf(BOUNDARY_MINIMUM_SIGNAL_DBFS / 20.0);
    let minimum_contrast = 10_f64.powf(BOUNDARY_MINIMUM_CONTRAST_DB / 20.0);
    let mut candidates = Vec::new();
    let has_leading_silence = silence_ranges
        .first()
        .is_some_and(|range| range.start.is_zero());
    let total_duration = Duration::from_secs_f64(frame_count as f64 / f64::from(sample_rate));
    let has_trailing_silence = silence_ranges
        .last()
        .is_some_and(|range| range.end >= total_duration);

    if !has_leading_silence {
        let immediate = frame_rms(samples, channel_count, 0, immediate_frames.min(frame_count));
        let context = frame_rms(
            samples,
            channel_count,
            immediate_frames.min(frame_count),
            immediate_frames
                .saturating_add(context_frames)
                .min(frame_count),
        );
        if is_unfaded_file_edge(immediate, context, minimum_signal) {
            candidates.push(0);
        }
    }
    if !has_trailing_silence {
        let immediate_start = frame_count.saturating_sub(immediate_frames);
        let context_end = immediate_start;
        let context_start = context_end.saturating_sub(context_frames);
        let immediate = frame_rms(samples, channel_count, immediate_start, frame_count);
        let context = frame_rms(samples, channel_count, context_start, context_end);
        if is_unfaded_file_edge(immediate, context, minimum_signal) {
            candidates.push(frame_count);
        }
    }

    for range in silence_ranges {
        if range.duration() < SILENCE_WINDOW {
            continue;
        }
        let start_frame = duration_frame(range.start, sample_rate).min(frame_count);
        let end_frame = duration_frame(range.end, sample_rate).min(frame_count);

        let entry_immediate = frame_rms(
            samples,
            channel_count,
            start_frame.saturating_sub(immediate_frames),
            start_frame,
        );
        let entry_context = frame_rms(
            samples,
            channel_count,
            start_frame
                .saturating_sub(immediate_frames)
                .saturating_sub(context_frames),
            start_frame.saturating_sub(immediate_frames),
        );
        let entry_silence = frame_rms(
            samples,
            channel_count,
            start_frame,
            start_frame.saturating_add(immediate_frames).min(end_frame),
        );
        if start_frame > 0
            && is_hard_boundary(
                entry_immediate,
                entry_context,
                entry_silence,
                minimum_signal,
                minimum_contrast,
            )
        {
            candidates.push(start_frame);
        }

        let exit_immediate = frame_rms(
            samples,
            channel_count,
            end_frame,
            end_frame.saturating_add(immediate_frames).min(frame_count),
        );
        let exit_context = frame_rms(
            samples,
            channel_count,
            end_frame.saturating_add(immediate_frames).min(frame_count),
            end_frame
                .saturating_add(immediate_frames)
                .saturating_add(context_frames)
                .min(frame_count),
        );
        let exit_silence = frame_rms(
            samples,
            channel_count,
            end_frame.saturating_sub(immediate_frames).max(start_frame),
            end_frame,
        );
        if end_frame < frame_count
            && is_hard_boundary(
                exit_immediate,
                exit_context,
                exit_silence,
                minimum_signal,
                minimum_contrast,
            )
        {
            candidates.push(end_frame);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

fn is_unfaded_file_edge(immediate_rms: f64, context_rms: f64, minimum_signal: f64) -> bool {
    immediate_rms >= minimum_signal && immediate_rms >= context_rms * BOUNDARY_MINIMUM_CONTEXT_RATIO
}

fn is_hard_boundary(
    immediate_rms: f64,
    context_rms: f64,
    silence_rms: f64,
    minimum_signal: f64,
    minimum_contrast: f64,
) -> bool {
    immediate_rms >= minimum_signal
        && immediate_rms >= context_rms * BOUNDARY_MINIMUM_CONTEXT_RATIO
        && immediate_rms >= silence_rms.max(f64::EPSILON) * minimum_contrast
}

fn duration_frames(duration: Duration, sample_rate: u32) -> usize {
    ((duration.as_secs_f64() * f64::from(sample_rate)).round() as usize).max(1)
}

fn duration_frame(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
}

fn frame_rms(samples: &[f32], channel_count: usize, start_frame: usize, end_frame: usize) -> f64 {
    if start_frame >= end_frame {
        return 0.0;
    }
    let start_sample = start_frame.saturating_mul(channel_count);
    let end_sample = end_frame.saturating_mul(channel_count).min(samples.len());
    linear_rms(samples.get(start_sample..end_sample).unwrap_or_default()).unwrap_or_default()
}

fn analyze_metrics(
    samples: &[f32],
    sample_rate: u32,
    channel_count: usize,
    silence_ranges: &[SilenceRange],
    active_windows: &[bool],
) -> Result<AudioMetrics> {
    let frames_per_window =
        ((u64::from(sample_rate) * SILENCE_WINDOW.as_millis() as u64) / 1_000).max(1) as usize;
    let active_samples = samples
        .chunks(channel_count)
        .enumerate()
        .filter(|(frame_index, _)| {
            active_windows
                .get(frame_index / frames_per_window)
                .copied()
                .unwrap_or(false)
        })
        .flat_map(|(_, frame)| frame.iter().copied())
        .collect::<Vec<_>>();
    let active_speech_rms_dbfs = linear_rms(&active_samples).and_then(linear_to_dbfs);

    let mut loudness = EbuR128::new(
        channel_count as u32,
        sample_rate,
        Mode::I | Mode::SAMPLE_PEAK | Mode::TRUE_PEAK,
    )
    .context("Could not initialize EBU R128 analysis")?;
    loudness
        .add_frames_f32(samples)
        .context("Could not process samples for EBU R128 analysis")?;
    let integrated_lufs = match loudness.loudness_global() {
        Ok(value) if value.is_finite() => Some(value),
        Ok(_) => None,
        Err(error) => return Err(error).context("Could not obtain EBU R128 integrated loudness"),
    };
    let mut sample_peak = None::<f64>;
    let mut true_peak = None::<f64>;
    for channel_index in 0..channel_count {
        let channel_peak = loudness
            .sample_peak(channel_index as u32)
            .context("Could not obtain EBU R128 sample peak")?;
        sample_peak = Some(sample_peak.unwrap_or_default().max(channel_peak));
        let channel_true_peak = loudness
            .true_peak(channel_index as u32)
            .context("Could not obtain EBU R128 true peak")?;
        true_peak = Some(true_peak.unwrap_or_default().max(channel_true_peak));
    }
    let sample_peak_dbfs = sample_peak.and_then(linear_to_dbfs);
    let true_peak_dbtp = true_peak.and_then(linear_to_dbfs);

    let mut channel_sums = vec![0.0_f64; channel_count];
    for (sample_index, sample) in samples.iter().copied().enumerate() {
        channel_sums[sample_index % channel_count] += f64::from(sample);
    }
    let frame_count = samples.len() / channel_count;
    let dc_offsets = if frame_count == 0 {
        vec![0.0; channel_count]
    } else {
        channel_sums
            .into_iter()
            .map(|sum| sum / frame_count as f64)
            .collect()
    };
    let near_clip_threshold = 10_f32.powf(NEAR_CLIP_THRESHOLD_DBFS as f32 / 20.0);
    let near_clipped_samples = samples
        .iter()
        .filter(|sample| sample.abs() >= near_clip_threshold)
        .count();
    let (possible_click_count, possible_click_timestamps) =
        detect_possible_clicks(samples, sample_rate, channel_count, silence_ranges);
    let total_duration = Duration::from_secs_f64(frame_count as f64 / f64::from(sample_rate));
    let leading_silence = silence_ranges
        .first()
        .filter(|range| range.start.is_zero())
        .map(|range| range.duration())
        .unwrap_or_default();
    let trailing_silence = silence_ranges
        .last()
        .filter(|range| range.end >= total_duration)
        .map(|range| range.duration())
        .unwrap_or_default();
    let internal_ranges = silence_ranges
        .iter()
        .filter(|range| !range.start.is_zero() && range.end < total_duration);
    let mut internal_pause_count = 0;
    let mut internal_pause_total = Duration::ZERO;
    let mut internal_pause_longest = Duration::ZERO;
    for range in internal_ranges {
        let duration = range.duration();
        internal_pause_count += 1;
        internal_pause_total = internal_pause_total.saturating_add(duration);
        internal_pause_longest = internal_pause_longest.max(duration);
    }

    Ok(AudioMetrics {
        active_speech_rms_dbfs,
        integrated_lufs,
        sample_peak_dbfs,
        true_peak_dbtp,
        dc_offsets: Arc::new(dc_offsets),
        near_clipped_samples,
        total_samples: samples.len(),
        leading_silence,
        trailing_silence,
        internal_pause_count,
        internal_pause_total,
        internal_pause_longest,
        possible_click_count,
        possible_click_timestamps: Arc::new(possible_click_timestamps),
    })
}

fn linear_rms(samples: &[f32]) -> Option<f64> {
    (!samples.is_empty()).then(|| {
        (samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    })
}

fn linear_to_dbfs(value: f64) -> Option<f64> {
    (value > 0.0 && value.is_finite()).then(|| 20.0 * value.log10())
}

fn wav_duration_from_bytes(bytes: &[u8]) -> Option<Duration> {
    if bytes.len() < 12 || bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }

    let mut byte_rate = None;
    let mut block_align = None;
    let mut data_size = None;
    let mut offset = 12usize;

    while offset.checked_add(8)? <= bytes.len() {
        let chunk_id = bytes.get(offset..offset + 4)?;
        let declared_chunk_size =
            u32::from_le_bytes(bytes.get(offset + 4..offset + 8)?.try_into().ok()?) as usize;
        let chunk_data_offset = offset.checked_add(8)?;
        let available_size = bytes.len().saturating_sub(chunk_data_offset);
        let chunk_size = declared_chunk_size.min(available_size);

        match chunk_id {
            b"fmt " if chunk_size >= 16 => {
                let format = u16::from_le_bytes(
                    bytes
                        .get(chunk_data_offset..chunk_data_offset + 2)?
                        .try_into()
                        .ok()?,
                );
                let current_byte_rate = u32::from_le_bytes(
                    bytes
                        .get(chunk_data_offset + 8..chunk_data_offset + 12)?
                        .try_into()
                        .ok()?,
                );
                let current_block_align = u16::from_le_bytes(
                    bytes
                        .get(chunk_data_offset + 12..chunk_data_offset + 14)?
                        .try_into()
                        .ok()?,
                );

                if format == 1 && current_byte_rate > 0 && current_block_align > 0 {
                    byte_rate = Some(current_byte_rate as u64);
                    block_align = Some(current_block_align as u64);
                }
            }
            b"data" => {
                data_size = Some(chunk_size as u64);
                break;
            }
            _ => {}
        }

        offset = chunk_data_offset
            .checked_add(declared_chunk_size)?
            .checked_add(declared_chunk_size % 2)?;
    }

    let byte_rate = byte_rate?;
    let block_align = block_align?;
    let data_size = data_size?;
    let data_size = data_size - data_size % block_align;
    Some(Duration::from_secs_f64(data_size as f64 / byte_rate as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bits_per_sample = 16u16;
        let block_align = channels * bits_per_sample / 8;
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = std::mem::size_of_val(samples) as u32;
        let riff_size = 36u32 + data_size;
        let mut wav = Vec::with_capacity(44 + data_size as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    #[test]
    fn computes_duration_from_available_wav_data_when_sizes_are_maxed() {
        let sample_rate = 24_000u32;
        let byte_rate = sample_rate * 2;
        let block_align = 2u16;
        let data_bytes = byte_rate as usize;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&u32::MAX.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&u32::MAX.to_le_bytes());
        wav.resize(wav.len() + data_bytes, 0);

        assert_eq!(wav_duration_from_bytes(&wav), Some(Duration::from_secs(1)));
    }

    fn analyzed_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> AudioAnalysis {
        let analysis = analyze_audio(
            Arc::new(pcm_wav(sample_rate, channels, samples)),
            Some("wav".to_string()),
        );
        assert!(analysis.is_ok(), "test WAV should decode: {analysis:?}");
        match analysis {
            Ok(analysis) => analysis,
            Err(error) => panic!("test WAV analysis failed: {error}"),
        }
    }

    #[test]
    fn waveform_is_signed_and_uses_absolute_full_scale() {
        let sample_rate = 8_000u32;
        let samples = (0..sample_rate)
            .map(|index| if index % 2 == 0 { 8_192 } else { -4_096 })
            .collect::<Vec<_>>();
        let analysis = analyzed_wav(sample_rate, 1, &samples);
        let minimum = analysis
            .waveform
            .iter()
            .map(|bucket| bucket.minimum)
            .fold(0.0_f32, f32::min);
        let maximum = analysis
            .waveform
            .iter()
            .map(|bucket| bucket.maximum)
            .fold(0.0_f32, f32::max);

        assert!(minimum < -0.12 && minimum > -0.13, "minimum was {minimum}");
        assert!(maximum > 0.24 && maximum < 0.26, "maximum was {maximum}");
        assert!(maximum < 0.5, "waveform must not be normalized");
    }

    #[test]
    fn computes_active_rms_dc_and_near_clip_metrics() {
        let sample_rate = 8_000u32;
        let mut samples = vec![8_192_i16; sample_rate as usize];
        samples[0] = i16::MAX;
        let analysis = analyzed_wav(sample_rate, 1, &samples);
        let metrics = analysis.metrics.as_ref();

        assert!(
            metrics
                .active_speech_rms_dbfs
                .is_some_and(|value| value > -13.0 && value < -11.0)
        );
        assert!(metrics.dc_offsets[0] > 0.24 && metrics.dc_offsets[0] < 0.26);
        assert_eq!(metrics.near_clipped_samples, 1);
        assert_eq!(metrics.total_samples, sample_rate as usize);
    }

    #[test]
    fn separates_leading_trailing_and_internal_silence() {
        let sample_rate = 1_000u32;
        let mut samples = Vec::new();
        samples.extend(vec![0_i16; 100]);
        samples.extend(vec![8_192_i16; 200]);
        samples.extend(vec![0_i16; 100]);
        samples.extend(vec![-8_192_i16; 200]);
        samples.extend(vec![0_i16; 100]);
        let analysis = analyzed_wav(sample_rate, 1, &samples);
        let metrics = analysis.metrics.as_ref();

        assert_eq!(metrics.leading_silence, Duration::from_millis(100));
        assert_eq!(metrics.trailing_silence, Duration::from_millis(100));
        assert_eq!(metrics.internal_pause_count, 1);
        assert_eq!(metrics.internal_pause_total, Duration::from_millis(100));
        assert_eq!(metrics.internal_pause_longest, Duration::from_millis(100));
        assert_eq!(analysis.silence_ranges.len(), 3);
    }

    #[test]
    fn preserves_single_window_internal_silence() {
        let sample_rate = 1_000u32;
        let mut samples = Vec::new();
        samples.extend(vec![8_192_i16; 100]);
        samples.extend(vec![0_i16; 20]);
        samples.extend(vec![-8_192_i16; 100]);
        let analysis = analyzed_wav(sample_rate, 1, &samples);
        let metrics = analysis.metrics.as_ref();

        assert_eq!(metrics.leading_silence, Duration::ZERO);
        assert_eq!(metrics.trailing_silence, Duration::ZERO);
        assert_eq!(metrics.internal_pause_count, 1);
        assert_eq!(metrics.internal_pause_total, Duration::from_millis(20));
        assert_eq!(metrics.internal_pause_longest, Duration::from_millis(20));
        assert_eq!(analysis.silence_ranges.len(), 1);
    }

    #[test]
    fn detects_isolated_discontinuity_but_not_smooth_sine() {
        let sample_rate = 48_000;
        let mut discontinuity = vec![0.0_f32; 2_000];
        discontinuity[500..1_000].fill(0.8);
        for (index, sample) in discontinuity[1_000..].iter_mut().enumerate() {
            *sample = 0.8 * (1.0 - index as f32 / 1_000.0);
        }
        let (click_count, timestamps) = detect_possible_clicks(&discontinuity, sample_rate, 1, &[]);
        assert_eq!(click_count, 1);
        assert_eq!(timestamps.len(), 1);
        assert!((timestamps[0].as_secs_f64() - 500.0 / sample_rate as f64).abs() < 0.000_1);

        let edge_fade_frames = sample_rate / 50;
        let sine = (0..sample_rate)
            .map(|index| {
                let edge_gain =
                    (index.min(sample_rate - 1 - index) as f32 / edge_fade_frames as f32).min(1.0);
                (index as f32 * 8_000.0 * std::f32::consts::TAU / sample_rate as f32).sin()
                    * 0.8
                    * edge_gain
            })
            .collect::<Vec<_>>();
        let (click_count, timestamps) = detect_possible_clicks(&sine, sample_rate, 1, &[]);
        assert_eq!(click_count, 0);
        assert!(timestamps.is_empty());
    }

    fn detect_with_segmented_silence(samples: &[f32], sample_rate: u32) -> (usize, Vec<Duration>) {
        let (silence_ranges, _) = segment_silence(samples, sample_rate, 1);
        detect_possible_clicks(samples, sample_rate, 1, &silence_ranges)
    }

    #[test]
    fn detects_low_level_abrupt_entry_into_exact_silence() {
        let sample_rate = 48_000;
        let signal_frames = sample_rate / 10;
        let fade_frames = sample_rate / 50;
        let mut samples = (0..fade_frames)
            .map(|frame| 0.005 * frame as f32 / fade_frames as f32)
            .collect::<Vec<_>>();
        samples.extend(vec![0.005; signal_frames as usize]);
        samples.extend(vec![0.0; fade_frames as usize]);
        samples.extend((0..fade_frames).map(|frame| 0.005 * frame as f32 / fade_frames as f32));
        samples.extend(vec![0.005; signal_frames as usize]);
        samples.extend(
            (0..fade_frames).map(|frame| 0.005 * (1.0 - frame as f32 / fade_frames as f32)),
        );

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 1);
        assert_eq!(timestamps, vec![Duration::from_millis(120)]);
    }

    #[test]
    fn detects_low_level_abrupt_entry_into_residual_silence() {
        let sample_rate = 48_000;
        let signal_frames = sample_rate / 10;
        let fade_frames = sample_rate / 50;
        let mut samples = (0..fade_frames)
            .map(|frame| 0.005 * frame as f32 / fade_frames as f32)
            .collect::<Vec<_>>();
        samples.extend(vec![0.005; signal_frames as usize]);
        samples.extend(vec![0.000_5; fade_frames as usize]);
        samples.extend((0..fade_frames).map(|frame| 0.005 * frame as f32 / fade_frames as f32));
        samples.extend(vec![0.005; signal_frames as usize]);
        samples.extend(
            (0..fade_frames).map(|frame| 0.005 * (1.0 - frame as f32 / fade_frames as f32)),
        );

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 1);
        assert_eq!(timestamps, vec![Duration::from_millis(120)]);
    }

    #[test]
    fn does_not_detect_faded_silence_boundary() {
        let sample_rate = 48_000;
        let fade_frames = sample_rate / 50;
        let mut samples = (0..fade_frames)
            .map(|frame| 0.005 * frame as f32 / fade_frames as f32)
            .collect::<Vec<_>>();
        samples.extend(vec![0.005_f32; (sample_rate / 10) as usize]);
        samples.extend(
            (0..fade_frames).map(|frame| 0.005 * (1.0 - frame as f32 / fade_frames as f32)),
        );
        samples.extend(vec![0.0; fade_frames as usize]);
        samples.extend((0..fade_frames).map(|frame| 0.005 * frame as f32 / fade_frames as f32));
        samples.extend(vec![0.005_f32; (sample_rate / 10) as usize]);
        samples.extend(
            (0..fade_frames).map(|frame| 0.005 * (1.0 - frame as f32 / fade_frames as f32)),
        );

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 0);
        assert!(timestamps.is_empty());
    }

    #[test]
    fn detects_both_abrupt_boundaries_of_internal_pause() {
        let sample_rate = 48_000;
        let signal_frames = sample_rate / 10;
        let fade_frames = sample_rate / 50;
        let mut samples = (0..fade_frames)
            .map(|frame| 0.005 * frame as f32 / fade_frames as f32)
            .collect::<Vec<_>>();
        samples.extend(vec![0.005; signal_frames as usize]);
        samples.extend(vec![0.0; fade_frames as usize]);
        samples.extend(vec![-0.005; signal_frames as usize]);
        samples.extend(
            (0..fade_frames).map(|frame| -0.005 * (1.0 - frame as f32 / fade_frames as f32)),
        );

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 2);
        assert_eq!(
            timestamps,
            vec![Duration::from_millis(120), Duration::from_millis(140)]
        );
    }

    #[test]
    fn detects_abrupt_non_silent_file_start_and_end() {
        let sample_rate = 48_000;
        let samples = vec![0.005_f32; (sample_rate / 10) as usize];

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 2);
        assert_eq!(timestamps, vec![Duration::ZERO, Duration::from_millis(100)]);
    }

    #[test]
    fn does_not_detect_faded_file_start_and_end() {
        let sample_rate = 48_000;
        let fade_frames = sample_rate / 50;
        let mut samples = (0..fade_frames)
            .map(|frame| 0.005 * frame as f32 / fade_frames as f32)
            .collect::<Vec<_>>();
        samples.extend(vec![0.005; (sample_rate * 3 / 50) as usize]);
        samples.extend(
            (0..fade_frames).map(|frame| 0.005 * (1.0 - frame as f32 / fade_frames as f32)),
        );

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 0);
        assert!(timestamps.is_empty());
    }

    #[test]
    fn detects_signal_boundaries_of_leading_and_trailing_silence() {
        let sample_rate = 48_000;
        let window_frames = sample_rate / 50;

        let mut leading = vec![0.0_f32; window_frames as usize];
        leading.extend(vec![0.005; (sample_rate / 10) as usize]);
        leading.extend(
            (0..window_frames).map(|frame| 0.005 * (1.0 - frame as f32 / window_frames as f32)),
        );
        let (leading_count, leading_timestamps) =
            detect_with_segmented_silence(&leading, sample_rate);
        assert_eq!(leading_count, 1);
        assert_eq!(leading_timestamps, vec![Duration::from_millis(20)]);

        let mut trailing = (0..window_frames)
            .map(|frame| 0.005 * frame as f32 / window_frames as f32)
            .collect::<Vec<_>>();
        trailing.extend(vec![0.005; (sample_rate / 10) as usize]);
        trailing.extend(vec![0.0; window_frames as usize]);
        let (trailing_count, trailing_timestamps) =
            detect_with_segmented_silence(&trailing, sample_rate);
        assert_eq!(trailing_count, 1);
        assert_eq!(trailing_timestamps, vec![Duration::from_millis(120)]);
    }

    #[test]
    fn does_not_detect_fades_around_edge_silence() {
        let sample_rate = 48_000;
        let window_frames = sample_rate / 50;
        let mut samples = vec![0.0_f32; window_frames as usize];
        samples.extend((0..window_frames).map(|frame| 0.005 * frame as f32 / window_frames as f32));
        samples.extend(vec![0.005; (sample_rate / 10) as usize]);
        samples.extend(
            (0..window_frames).map(|frame| 0.005 * (1.0 - frame as f32 / window_frames as f32)),
        );
        samples.extend(vec![0.0; window_frames as usize]);

        let (count, timestamps) = detect_with_segmented_silence(&samples, sample_rate);
        assert_eq!(count, 0);
        assert!(timestamps.is_empty());
    }

    #[test]
    fn wires_plausible_ebur128_results() {
        let sample_rate = 48_000u32;
        let samples = (0..sample_rate * 2)
            .map(|index| {
                let phase = index as f32 * 440.0 * std::f32::consts::TAU / sample_rate as f32;
                (phase.sin() * 0.25 * f32::from(i16::MAX)) as i16
            })
            .collect::<Vec<_>>();
        let analysis = analyzed_wav(sample_rate, 1, &samples);
        let metrics = analysis.metrics.as_ref();

        assert!(
            metrics
                .integrated_lufs
                .is_some_and(|value| value > -20.0 && value < -10.0)
        );
        assert!(
            metrics
                .sample_peak_dbfs
                .is_some_and(|value| value > -13.0 && value < -11.0)
        );
        assert!(
            metrics
                .true_peak_dbtp
                .is_some_and(|value| value > -13.0 && value < -10.0)
        );
        if let (Some(sample_peak), Some(true_peak)) =
            (metrics.sample_peak_dbfs, metrics.true_peak_dbtp)
        {
            assert!(true_peak + 0.1 >= sample_peak);
        }
    }
}
