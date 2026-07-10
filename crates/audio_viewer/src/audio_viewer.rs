use std::{
    cell::RefCell,
    io::Cursor,
    path::Path,
    rc::Rc,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result};
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render, SharedString, Task, Window,
    actions, canvas, div, fill, point, size,
};
use language::File as _;
use project::{AudioItem, AudioItemEvent, Project};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{
    Color, Icon, IconButton, IconButtonShape, IconName, IconSize, Label, LabelSize, Tooltip,
    prelude::*,
};
use util::{ResultExt as _, paths::PathExt};
use workspace::{
    ItemSettings, Pane, ToolbarItemLocation, WorkspaceId,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ProjectItem, TabContentParams},
};

const SEEK_STEP: Duration = Duration::from_secs(5);
const WAVEFORM_PEAK_COUNT: usize = 1_536;
const PLAYBACK_UPDATE_INTERVAL: Duration = Duration::from_millis(33);

actions!(
    audio_viewer,
    [
        /// Play or pause the audio file.
        TogglePlayback,
        /// Seek backward by five seconds.
        SeekBackward,
        /// Seek forward by five seconds.
        SeekForward,
        /// Return to the beginning of the audio file.
        ResetPlayback
    ]
);

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioMetadata {
    file_size: u64,
    duration: Option<Duration>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
}

impl AudioMetadata {
    fn loading(file_size: u64) -> Self {
        Self {
            file_size,
            ..Default::default()
        }
    }
}

#[derive(Clone)]
struct SharedAudioBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedAudioBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

#[derive(Debug)]
struct AudioAnalysis {
    metadata: AudioMetadata,
    waveform_peaks: Arc<Vec<f32>>,
}

fn decode_audio(
    bytes: Arc<Vec<u8>>,
    format_hint: Option<&str>,
) -> Result<Decoder<Cursor<SharedAudioBytes>>> {
    let file_size = bytes.len() as u64;
    let mut builder = Decoder::builder()
        .with_data(Cursor::new(SharedAudioBytes(bytes)))
        .with_byte_len(file_size);
    if let Some(format_hint) = format_hint {
        builder = builder.with_hint(format_hint);
    }
    builder.build().context("Could not decode the audio file")
}

fn analyze_audio(bytes: Arc<Vec<u8>>, format_hint: Option<String>) -> Result<AudioAnalysis> {
    let file_size = bytes.len() as u64;
    let mut decoder = decode_audio(bytes.clone(), format_hint.as_deref())?;
    let channels = decoder.channels().get();
    let sample_rate = decoder.sample_rate().get();
    let duration = wav_duration_from_bytes(bytes.as_slice()).or_else(|| decoder.total_duration());
    let waveform_peaks = extract_waveform_peaks(&mut decoder, duration, sample_rate, channels);

    Ok(AudioAnalysis {
        metadata: AudioMetadata {
            file_size,
            duration,
            channels: Some(channels),
            sample_rate: Some(sample_rate),
        },
        waveform_peaks: Arc::new(waveform_peaks),
    })
}

fn extract_waveform_peaks(
    decoder: &mut Decoder<Cursor<SharedAudioBytes>>,
    duration: Option<Duration>,
    sample_rate: u32,
    channels: u16,
) -> Vec<f32> {
    let expected_frames = duration
        .map(|duration| duration.as_secs_f64() * sample_rate as f64)
        .map(|frames| frames.ceil() as usize)
        .filter(|frames| *frames > 0);
    let channels = usize::from(channels.max(1));
    let mut peaks = vec![0.0_f32; WAVEFORM_PEAK_COUNT];
    let mut decoded_frames = 0usize;

    if let Some(expected_frames) = expected_frames {
        for (sample_index, sample) in decoder.enumerate() {
            let frame_index = sample_index / channels;
            let peak_index = frame_index
                .saturating_mul(WAVEFORM_PEAK_COUNT)
                .checked_div(expected_frames)
                .unwrap_or_default()
                .min(WAVEFORM_PEAK_COUNT - 1);
            peaks[peak_index] = peaks[peak_index].max(sample.abs());
            decoded_frames = frame_index.saturating_add(1);
        }
    } else {
        let mut frames_per_peak = 1_024usize;
        for (sample_index, sample) in decoder.enumerate() {
            let frame_index = sample_index / channels;
            let mut peak_index = frame_index / frames_per_peak;
            if peak_index >= WAVEFORM_PEAK_COUNT {
                for index in 0..WAVEFORM_PEAK_COUNT / 2 {
                    peaks[index] = peaks[index * 2].max(peaks[index * 2 + 1]);
                }
                peaks[WAVEFORM_PEAK_COUNT / 2..].fill(0.0);
                frames_per_peak = frames_per_peak.saturating_mul(2);
                peak_index = frame_index / frames_per_peak;
            }
            peaks[peak_index.min(WAVEFORM_PEAK_COUNT - 1)] =
                peaks[peak_index.min(WAVEFORM_PEAK_COUNT - 1)].max(sample.abs());
            decoded_frames = frame_index.saturating_add(1);
        }
    }

    let populated_peaks = expected_frames
        .map(|_| WAVEFORM_PEAK_COUNT)
        .unwrap_or_else(|| {
            decoded_frames
                .checked_add(1_023)
                .and_then(|frames| frames.checked_div(1_024))
                .unwrap_or_default()
                .clamp(1, WAVEFORM_PEAK_COUNT)
        });
    peaks.truncate(populated_peaks);

    let maximum_peak = peaks.iter().copied().fold(0.0_f32, f32::max);
    if maximum_peak > 0.0 {
        for peak in &mut peaks {
            *peak = (*peak / maximum_peak).clamp(0.0, 1.0);
        }
    }
    peaks
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

pub enum AudioViewEvent {
    TitleChanged,
}

impl EventEmitter<AudioViewEvent> for AudioView {}

pub struct AudioView {
    audio_item: Entity<AudioItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    playback: PlaybackState,
    metadata: AudioMetadata,
    waveform_peaks: Option<Arc<Vec<f32>>>,
    analysis_error: Option<String>,
    analysis_task: Task<()>,
    scrub_position: Option<Duration>,
    hover_position: Option<Duration>,
    seek_bar_hovered: bool,
    progress_updates_running: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PlaybackStatus {
    #[default]
    Idle,
    Starting,
    Playing,
    Paused,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PlaybackSnapshot {
    status: PlaybackStatus,
    position: Duration,
    error: Option<String>,
}

#[derive(Default)]
struct SharedPlaybackSnapshot(Mutex<PlaybackSnapshot>);

impl SharedPlaybackSnapshot {
    fn read(&self) -> PlaybackSnapshot {
        match self.0.lock() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => {
                log::error!("audio playback state lock was poisoned");
                poisoned.into_inner().clone()
            }
        }
    }

    fn update(&self, update: impl FnOnce(&mut PlaybackSnapshot)) {
        match self.0.lock() {
            Ok(mut snapshot) => update(&mut snapshot),
            Err(poisoned) => {
                log::error!("audio playback state lock was poisoned");
                update(&mut poisoned.into_inner());
            }
        }
    }
}

enum PlaybackCommand {
    LoadAndPlay {
        bytes: Arc<Vec<u8>>,
        format_hint: Option<String>,
        offset: Duration,
    },
    Pause,
    Resume,
    Seek(Duration),
    Stop,
    Shutdown,
}

struct PlaybackController {
    command_sender: Sender<PlaybackCommand>,
    snapshot: Arc<SharedPlaybackSnapshot>,
    _thread: thread::JoinHandle<()>,
}

impl PlaybackController {
    fn new() -> Result<Self> {
        let (command_sender, command_receiver) = mpsc::channel();
        let snapshot = Arc::new(SharedPlaybackSnapshot::default());
        let thread_snapshot = snapshot.clone();
        let thread = thread::Builder::new()
            .name("AudioFileViewerPlayback".to_string())
            .spawn(move || playback_thread(command_receiver, thread_snapshot))
            .context("Could not start the audio playback thread")?;

        Ok(Self {
            command_sender,
            snapshot,
            _thread: thread,
        })
    }

    fn send(&self, command: PlaybackCommand) -> Result<()> {
        self.command_sender
            .send(command)
            .context("The audio playback thread stopped unexpectedly")
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        self.snapshot.read()
    }
}

impl Drop for PlaybackController {
    fn drop(&mut self) {
        if self.command_sender.send(PlaybackCommand::Shutdown).is_err() {
            log::debug!("audio playback thread had already stopped");
        }
    }
}

struct PlaybackSession {
    _output: MixerDeviceSink,
    player: Player,
}

fn playback_thread(
    command_receiver: Receiver<PlaybackCommand>,
    snapshot: Arc<SharedPlaybackSnapshot>,
) {
    let mut session: Option<PlaybackSession> = None;

    loop {
        let command = match command_receiver.recv_timeout(PLAYBACK_UPDATE_INTERVAL) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(command) = command
            && handle_playback_command(command, &mut session, &snapshot)
        {
            break;
        }
        while let Ok(command) = command_receiver.try_recv() {
            if handle_playback_command(command, &mut session, &snapshot) {
                return;
            }
        }

        let Some(current_session) = session.as_ref() else {
            continue;
        };
        if snapshot.read().status == PlaybackStatus::Failed {
            session = None;
            continue;
        }

        let position = current_session.player.get_pos();
        if current_session.player.empty() {
            snapshot.update(|snapshot| {
                snapshot.status = PlaybackStatus::Finished;
                snapshot.position = position;
            });
            session = None;
        } else if !current_session.player.is_paused() {
            snapshot.update(|snapshot| {
                snapshot.status = PlaybackStatus::Playing;
                snapshot.position = position;
            });
        }
    }
}

fn handle_playback_command(
    command: PlaybackCommand,
    session: &mut Option<PlaybackSession>,
    snapshot: &Arc<SharedPlaybackSnapshot>,
) -> bool {
    match command {
        PlaybackCommand::LoadAndPlay {
            bytes,
            format_hint,
            offset,
        } => {
            snapshot.update(|snapshot| {
                snapshot.status = PlaybackStatus::Starting;
                snapshot.position = offset;
                snapshot.error = None;
            });
            match start_playback_session(bytes, format_hint.as_deref(), offset, snapshot.clone()) {
                Ok(new_session) => {
                    *session = Some(new_session);
                    snapshot.update(|snapshot| snapshot.status = PlaybackStatus::Playing);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    log::error!("failed to play audio file: {error:?}");
                    snapshot.update(|snapshot| {
                        snapshot.status = PlaybackStatus::Failed;
                        snapshot.error = Some(message);
                    });
                    *session = None;
                }
            }
        }
        PlaybackCommand::Pause => {
            if let Some(session) = session {
                session.player.pause();
                let position = session.player.get_pos();
                snapshot.update(|snapshot| {
                    snapshot.status = PlaybackStatus::Paused;
                    snapshot.position = position;
                });
            }
        }
        PlaybackCommand::Resume => {
            if let Some(session) = session {
                session.player.play();
                snapshot.update(|snapshot| {
                    snapshot.status = PlaybackStatus::Playing;
                    snapshot.error = None;
                });
            }
        }
        PlaybackCommand::Seek(position) => {
            if let Some(session) = session {
                match session.player.try_seek(position) {
                    Ok(()) => snapshot.update(|snapshot| {
                        snapshot.position = position;
                        snapshot.error = None;
                    }),
                    Err(error) => {
                        log::error!("failed to seek audio file: {error:?}");
                        snapshot.update(|snapshot| {
                            snapshot.error =
                                Some(format!("Could not seek in the audio file: {error}"));
                        });
                    }
                }
            }
        }
        PlaybackCommand::Stop => {
            *session = None;
            snapshot.update(|snapshot| *snapshot = PlaybackSnapshot::default());
        }
        PlaybackCommand::Shutdown => return true,
    }
    false
}

fn start_playback_session(
    bytes: Arc<Vec<u8>>,
    format_hint: Option<&str>,
    offset: Duration,
    snapshot: Arc<SharedPlaybackSnapshot>,
) -> Result<PlaybackSession> {
    let decoder = decode_audio(bytes, format_hint)?;
    let error_snapshot = snapshot;
    let mut output = DeviceSinkBuilder::from_default_device()
        .context("No audio output device is available")?
        .with_error_callback(move |error| {
            log::error!("audio output stream failed: {error:?}");
            error_snapshot.update(|snapshot| {
                snapshot.status = PlaybackStatus::Failed;
                snapshot.error = Some(format!("The audio output device failed: {error}"));
            });
        })
        .open_sink_or_fallback()
        .context("Could not open the audio output device")?;
    output.log_on_drop(false);
    let player = Player::connect_new(output.mixer());
    player.append(decoder);
    if !offset.is_zero() {
        player
            .try_seek(offset)
            .context("Could not seek to the playback position")?;
    }

    Ok(PlaybackSession {
        _output: output,
        player,
    })
}

#[derive(Default)]
struct PlaybackState {
    controller: Option<PlaybackController>,
    status: PlaybackStatus,
    position: Duration,
    error: Option<String>,
}

impl PlaybackState {
    fn position(&self) -> Duration {
        self.position
    }

    fn is_playing(&self) -> bool {
        matches!(
            self.status,
            PlaybackStatus::Starting | PlaybackStatus::Playing
        )
    }

    fn is_paused(&self) -> bool {
        self.status == PlaybackStatus::Paused
    }

    fn synchronize(&mut self) {
        let Some(controller) = &self.controller else {
            return;
        };
        let snapshot = controller.snapshot();
        self.status = snapshot.status;
        self.position = snapshot.position;
        self.error = snapshot.error;
    }

    fn send(&mut self, command: PlaybackCommand) -> bool {
        let Some(controller) = &self.controller else {
            return false;
        };
        if let Err(error) = controller.send(command) {
            log::error!("failed to control audio playback: {error:?}");
            self.status = PlaybackStatus::Failed;
            self.error = Some(error.to_string());
            false
        } else {
            true
        }
    }

    fn stop(&mut self) {
        self.send(PlaybackCommand::Stop);
        self.status = PlaybackStatus::Idle;
        self.position = Duration::ZERO;
        self.error = None;
    }

    fn pause(&mut self) {
        if self.send(PlaybackCommand::Pause) {
            self.status = PlaybackStatus::Paused;
        }
    }

    fn seek_to(&mut self, offset: Duration) {
        self.position = offset;
        if matches!(
            self.status,
            PlaybackStatus::Starting | PlaybackStatus::Playing | PlaybackStatus::Paused
        ) {
            self.send(PlaybackCommand::Seek(offset));
        }
    }

    fn play(&mut self, bytes: Arc<Vec<u8>>, format_hint: Option<String>) {
        if self.is_playing() {
            return;
        }

        if self.is_paused() {
            if self.send(PlaybackCommand::Resume) {
                self.status = PlaybackStatus::Playing;
                self.error = None;
            }
            return;
        }

        if self.status == PlaybackStatus::Finished {
            self.position = Duration::ZERO;
        }
        if self.controller.is_none() {
            match PlaybackController::new() {
                Ok(controller) => self.controller = Some(controller),
                Err(error) => {
                    log::error!("failed to initialize audio playback: {error:?}");
                    self.status = PlaybackStatus::Failed;
                    self.error = Some(error.to_string());
                    return;
                }
            }
        }

        let command = PlaybackCommand::LoadAndPlay {
            bytes,
            format_hint,
            offset: self.position,
        };
        if self.send(command) {
            self.status = PlaybackStatus::Starting;
            self.error = None;
        }
    }
}

impl AudioView {
    pub fn new(
        audio_item: Entity<AudioItem>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&audio_item, Self::on_audio_event).detach();
        cx.on_release(|this, _| {
            this.playback.stop();
        })
        .detach();
        let bytes = audio_item.read(cx).bytes.clone();
        let format_hint = audio_item
            .read(cx)
            .file
            .path()
            .extension()
            .map(str::to_owned);
        let metadata = AudioMetadata::loading(bytes.len() as u64);
        let analysis_task = Self::start_analysis(bytes, format_hint, cx);

        Self {
            audio_item,
            project,
            focus_handle: cx.focus_handle(),
            playback: PlaybackState::default(),
            metadata,
            waveform_peaks: None,
            analysis_error: None,
            analysis_task,
            scrub_position: None,
            hover_position: None,
            seek_bar_hovered: false,
            progress_updates_running: false,
        }
    }

    fn start_analysis(
        bytes: Arc<Vec<u8>>,
        format_hint: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let analysis = cx
                .background_spawn(async move { analyze_audio(bytes, format_hint) })
                .await;
            this.update(cx, |this, cx| {
                match analysis {
                    Ok(analysis) => {
                        this.metadata = analysis.metadata;
                        this.waveform_peaks = Some(analysis.waveform_peaks);
                        this.analysis_error = None;
                    }
                    Err(error) => {
                        log::error!("failed to analyze audio file: {error:?}");
                        this.waveform_peaks = None;
                        this.analysis_error = Some(error.to_string());
                    }
                }
                cx.notify();
            })
            .log_err();
        })
    }

    fn format_hint(&self, cx: &App) -> Option<String> {
        self.audio_item
            .read(cx)
            .file
            .path()
            .extension()
            .map(str::to_owned)
    }

    fn on_audio_event(
        &mut self,
        _: Entity<AudioItem>,
        event: &AudioItemEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AudioItemEvent::ReloadNeeded | AudioItemEvent::FileHandleChanged => {
                self.playback.stop();
                cx.emit(AudioViewEvent::TitleChanged);
                cx.notify();
            }
            AudioItemEvent::Reloaded => {
                self.playback.stop();
                let bytes = self.audio_item.read(cx).bytes.clone();
                self.metadata = AudioMetadata::loading(bytes.len() as u64);
                self.waveform_peaks = None;
                self.analysis_error = None;
                self.scrub_position = None;
                self.hover_position = None;
                self.analysis_task = Self::start_analysis(bytes, self.format_hint(cx), cx);
                cx.notify();
            }
        }
    }

    fn toggle_playback(
        &mut self,
        _: &TogglePlayback,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.playback.synchronize();
        if self.playback.is_playing() {
            self.playback.pause();
        } else {
            let bytes = self.audio_item.read(cx).bytes.clone();
            self.playback.play(bytes, self.format_hint(cx));
            self.schedule_progress_updates(cx);
        }
        cx.notify();
    }

    fn reset_playback(&mut self, _: &ResetPlayback, _window: &mut Window, cx: &mut Context<Self>) {
        self.playback.seek_to(Duration::ZERO);
        cx.notify();
    }

    fn seek_backward(&mut self, _: &SeekBackward, _window: &mut Window, cx: &mut Context<Self>) {
        self.playback.synchronize();
        let position = self.playback.position();
        self.playback.seek_to(position.saturating_sub(SEEK_STEP));
        cx.notify();
    }

    fn seek_forward(&mut self, _: &SeekForward, _window: &mut Window, cx: &mut Context<Self>) {
        self.playback.synchronize();
        let duration = self.metadata.duration;
        let mut offset = self.playback.position() + SEEK_STEP;
        if let Some(duration) = duration {
            offset = offset.min(duration);
        }
        self.playback.seek_to(offset);
        cx.notify();
    }

    fn seek_position(
        &self,
        position: gpui::Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> Option<Duration> {
        let Some(duration) = self.metadata.duration else {
            return None;
        };
        if bounds.size.width <= Pixels::ZERO {
            return None;
        }
        let fraction = ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.);
        Some(duration.mul_f32(fraction))
    }

    fn seek_bar_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = *bounds.borrow() else {
            return;
        };
        self.scrub_position = self.seek_position(event.position, bounds);
        self.hover_position = self.scrub_position;
        cx.notify();
        cx.stop_propagation();
    }

    fn seek_bar_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = *bounds.borrow() else {
            return;
        };
        self.hover_position = self.seek_position(event.position, bounds);
        if event.pressed_button == Some(MouseButton::Left) {
            self.scrub_position = self.hover_position;
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn seek_bar_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self.scrub_position.or_else(|| {
            bounds
                .borrow()
                .as_ref()
                .and_then(|bounds| self.seek_position(event.position, *bounds))
        });
        if let Some(position) = position {
            self.playback.seek_to(position);
        }
        self.scrub_position = None;
        self.hover_position = position;
        cx.notify();
        cx.stop_propagation();
    }

    fn seek_bar_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.seek_bar_hovered = *hovered;
        if !*hovered && self.scrub_position.is_none() {
            self.hover_position = None;
        }
        cx.notify();
    }

    fn schedule_progress_updates(&mut self, cx: &mut Context<Self>) {
        if self.progress_updates_running {
            return;
        }
        self.progress_updates_running = true;
        cx.spawn(async move |this, cx| {
            let mut update_interval = PLAYBACK_UPDATE_INTERVAL;
            loop {
                cx.background_executor().timer(update_interval).await;
                let (keep_updating, playing) = this
                    .update(cx, |this, cx| {
                        let previous_snapshot = PlaybackSnapshot {
                            status: this.playback.status,
                            position: this.playback.position,
                            error: this.playback.error.clone(),
                        };
                        this.playback.synchronize();
                        let playing = this.playback.is_playing();
                        let keep_updating = matches!(
                            this.playback.status,
                            PlaybackStatus::Starting
                                | PlaybackStatus::Playing
                                | PlaybackStatus::Paused
                        );
                        let current_snapshot = PlaybackSnapshot {
                            status: this.playback.status,
                            position: this.playback.position,
                            error: this.playback.error.clone(),
                        };
                        if playing || current_snapshot != previous_snapshot {
                            cx.notify();
                        }
                        if !keep_updating {
                            this.progress_updates_running = false;
                        }
                        (keep_updating, playing)
                    })
                    .unwrap_or((false, false));

                if !keep_updating {
                    break;
                }
                update_interval = if playing {
                    PLAYBACK_UPDATE_INTERVAL
                } else {
                    Duration::from_millis(250)
                };
            }
        })
        .detach();
    }
}

impl Item for AudioView {
    type Event = AudioViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            AudioViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.audio_item.entity_id(), self.audio_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.audio_item.read(cx).abs_path(cx)?;
        Some(abs_path.compact().to_string_lossy().into_owned().into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.audio_item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.audio_item
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.audio_item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if EditorSettings::get_global(cx).toolbar.breadcrumbs {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        let text = breadcrumbs_text_for_audio(self.project.read(cx), self.audio_item.read(cx), cx);
        Some((
            vec![HighlightedText {
                text: text.into(),
                highlights: Vec::new(),
            }],
            Some(ThemeSettings::get_global(cx).buffer_font.clone()),
        ))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let audio_item = self.audio_item.clone();
        let project = self.project.clone();
        Task::ready(Some(
            cx.new(|cx| Self::new(audio_item, project, window, cx)),
        ))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.audio_item.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

fn breadcrumbs_text_for_audio(project: &Project, audio: &AudioItem, cx: &App) -> String {
    let mut path = audio.file.path().clone();
    if project.visible_worktrees(cx).count() > 1
        && let Some(worktree) = project.worktree_for_id(audio.project_path(cx).worktree_id, cx)
    {
        path = worktree.read(cx).root_name().join(&path);
    }

    path.display(project.path_style(cx)).to_string()
}

impl EventEmitter<()> for AudioView {}

impl Focusable for AudioView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AudioView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.playback.synchronize();
        let metadata = self.metadata;
        let playback_position = self
            .scrub_position
            .unwrap_or_else(|| self.playback.position());
        let position = metadata
            .duration
            .map(|duration| playback_position.min(duration))
            .unwrap_or(playback_position);
        let max_seconds = metadata
            .duration
            .map(|duration| duration.as_secs_f32().max(1.0))
            .unwrap_or(1.0);
        let progress = (position.as_secs_f32() / max_seconds).clamp(0., 1.);
        let title = self.audio_item.read(cx).file.file_name(cx).to_string();
        let format_label = self
            .format_hint(cx)
            .unwrap_or_else(|| "audio".to_string())
            .to_uppercase();
        let is_playing = self.playback.is_playing();
        let can_seek = metadata.duration.is_some();
        let seek_bounds: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        let seek_bounds_for_canvas = seek_bounds.clone();
        let waveform_peaks = self.waveform_peaks.clone();
        let unplayed_color = cx.theme().colors().border_variant;
        let played_color = cx.theme().colors().text_accent;
        let hover_color = cx.theme().colors().text_muted.opacity(0.55);
        let show_playhead = self.seek_bar_hovered
            || self.scrub_position.is_some()
            || self.focus_handle.is_focused(window);
        let hover_progress = self.hover_position.and_then(|hover_position| {
            metadata.duration.map(|duration| {
                (hover_position.as_secs_f32() / duration.as_secs_f32().max(f32::EPSILON))
                    .clamp(0.0, 1.0)
            })
        });
        let analysis_loading = self.waveform_peaks.is_none() && self.analysis_error.is_none();

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("AudioViewer")
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_backward))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::reset_playback))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .child(
                v_flex()
                    .id("audio-viewer")
                    .w_full()
                    .max_w(px(760.))
                    .p_6()
                    .gap_5()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().elevated_surface_background)
                    .child(
                        h_flex()
                            .min_w_0()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .gap_1()
                                    .child(Label::new(title).size(LabelSize::Large).truncate())
                                    .child(
                                        Label::new(metadata_description(metadata))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .line_clamp(2),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .bg(cx.theme().colors().editor_background)
                                    .child(
                                        Label::new(format_label)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("audio-seek-bar")
                                    .relative()
                                    .w_full()
                                    .h(px(132.))
                                    .rounded_md()
                                    .overflow_hidden()
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .bg(cx.theme().colors().editor_background)
                                    .when(can_seek, |this| {
                                        this.cursor_pointer()
                                            .on_hover(cx.listener(Self::seek_bar_hover))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener({
                                                    let seek_bounds = seek_bounds.clone();
                                                    move |this, event, window, cx| {
                                                        this.seek_bar_mouse_down(
                                                            event,
                                                            &seek_bounds,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                }),
                                            )
                                            .on_mouse_up(
                                                MouseButton::Left,
                                                cx.listener({
                                                    let seek_bounds = seek_bounds.clone();
                                                    move |this, event, window, cx| {
                                                        this.seek_bar_mouse_up(
                                                            event,
                                                            &seek_bounds,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                }),
                                            )
                                            .on_mouse_up_out(
                                                MouseButton::Left,
                                                cx.listener({
                                                    let seek_bounds = seek_bounds.clone();
                                                    move |this, event, window, cx| {
                                                        this.seek_bar_mouse_up(
                                                            event,
                                                            &seek_bounds,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                }),
                                            )
                                            .on_mouse_move(cx.listener({
                                                let seek_bounds = seek_bounds.clone();
                                                move |this, event, window, cx| {
                                                    this.seek_bar_mouse_move(
                                                        event,
                                                        &seek_bounds,
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }))
                                    })
                                    .when(!can_seek, |this| this.cursor_default())
                                    .child(
                                        canvas(
                                            move |bounds, _, _| {
                                                *seek_bounds_for_canvas.borrow_mut() = Some(bounds);
                                            },
                                            move |bounds, _, window, _| {
                                                paint_waveform(
                                                    bounds,
                                                    waveform_peaks.as_deref().map(Vec::as_slice),
                                                    progress,
                                                    hover_progress,
                                                    show_playhead,
                                                    played_color,
                                                    unplayed_color,
                                                    hover_color,
                                                    window,
                                                );
                                            },
                                        )
                                        .size_full(),
                                    )
                                    .when(analysis_loading, |this| {
                                        this.child(
                                            div()
                                                .absolute()
                                                .size_full()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(
                                                    Label::new("Analyzing waveform…")
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted),
                                                ),
                                        )
                                    })
                                    .when_some(self.analysis_error.clone(), |this, _| {
                                        this.child(
                                            div()
                                                .absolute()
                                                .size_full()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(
                                                    Label::new("Waveform unavailable")
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted),
                                                ),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .justify_between()
                                    .gap_3()
                                    .text_sm()
                                    .text_color(cx.theme().colors().text_muted)
                                    .child(format_duration(position))
                                    .when_some(self.hover_position, |this, hover_position| {
                                        this.child(format!(
                                            "Seek to {}",
                                            format_duration(hover_position)
                                        ))
                                    })
                                    .child(
                                        metadata
                                            .duration
                                            .map(format_duration)
                                            .unwrap_or_else(|| "Unknown duration".to_string()),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(
                                div()
                                    .id("audio-play-pause")
                                    .w(px(44.))
                                    .h(px(44.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .bg(cx.theme().colors().text_accent.opacity(0.14))
                                    .hover(|style| {
                                        style.bg(cx.theme().colors().text_accent.opacity(0.22))
                                    })
                                    .role(gpui::accesskit::Role::Button)
                                    .aria_label(if is_playing { "Pause" } else { "Play" })
                                    .tooltip(move |_window, cx| {
                                        Tooltip::for_action(
                                            if is_playing { "Pause" } else { "Play" },
                                            &TogglePlayback,
                                            cx,
                                        )
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_playback(&TogglePlayback, window, cx);
                                    }))
                                    .child(
                                        Icon::new(if is_playing {
                                            IconName::DebugPause
                                        } else {
                                            IconName::PlayFilled
                                        })
                                        .size(IconSize::Medium)
                                        .color(Color::Accent),
                                    ),
                            )
                            .child(
                                IconButton::new("audio-reset", IconName::RotateCcw)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .disabled(!can_seek || position.is_zero())
                                    .aria_label("Return to beginning")
                                    .tooltip(|_window, cx| {
                                        Tooltip::for_action(
                                            "Return to Beginning",
                                            &ResetPlayback,
                                            cx,
                                        )
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_playback(&ResetPlayback, window, cx);
                                    })),
                            ),
                    )
                    .when_some(self.playback.error.clone(), |this, error| {
                        this.child(
                            h_flex()
                                .w_full()
                                .p_3()
                                .gap_2()
                                .rounded_md()
                                .bg(cx.theme().status().error.opacity(0.12))
                                .child(
                                    Icon::new(IconName::Info)
                                        .size(IconSize::Small)
                                        .color(Color::Error),
                                )
                                .child(
                                    Label::new(error).size(LabelSize::Small).color(Color::Error),
                                ),
                        )
                    }),
            )
    }
}

impl ProjectItem for AudioView {
    type Item = AudioItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

fn paint_waveform(
    bounds: Bounds<Pixels>,
    waveform_peaks: Option<&[f32]>,
    progress: f32,
    hover_progress: Option<f32>,
    show_playhead: bool,
    played_color: gpui::Hsla,
    unplayed_color: gpui::Hsla,
    hover_color: gpui::Hsla,
    window: &mut Window,
) {
    let center_y = (bounds.top() + bounds.bottom()) / 2.;
    let horizontal_padding = px(12.);
    let waveform_left = bounds.left() + horizontal_padding;
    let waveform_width = (bounds.size.width - horizontal_padding * 2.).max(px(1.));
    let maximum_height = (bounds.size.height - px(24.)).max(px(2.));

    if let Some(peaks) = waveform_peaks.filter(|peaks| !peaks.is_empty()) {
        let waveform_width_f32: f32 = waveform_width.into();
        let bar_count = ((waveform_width_f32 / 4.0).floor() as usize).clamp(1, peaks.len());
        let bar_width = px(2.);

        for bar_index in 0..bar_count {
            let peak_start = bar_index.saturating_mul(peaks.len()) / bar_count;
            let peak_end = ((bar_index + 1).saturating_mul(peaks.len()) / bar_count)
                .max(peak_start + 1)
                .min(peaks.len());
            let peak = peaks
                .get(peak_start..peak_end)
                .unwrap_or_default()
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            let bar_progress = (bar_index as f32 + 0.5) / bar_count as f32;
            let bar_height = (maximum_height * (0.08 + peak * 0.92)).max(px(2.));
            let bar_center = point(waveform_left + waveform_width * bar_progress, center_y);
            let mut bar = fill(
                Bounds::centered_at(bar_center, size(bar_width, bar_height)),
                if bar_progress <= progress {
                    played_color
                } else {
                    unplayed_color
                },
            );
            bar.corner_radii = (1.).into();
            window.paint_quad(bar);
        }
    } else {
        let mut baseline = fill(
            Bounds::centered_at(
                point(waveform_left + waveform_width / 2., center_y),
                size(waveform_width, px(2.)),
            ),
            unplayed_color,
        );
        baseline.corner_radii = (1.).into();
        window.paint_quad(baseline);
    }

    if let Some(hover_progress) = hover_progress {
        let hover_x = waveform_left + waveform_width * hover_progress;
        window.paint_quad(fill(
            Bounds::centered_at(
                point(hover_x, center_y),
                size(px(1.), maximum_height + px(8.)),
            ),
            hover_color,
        ));
    }

    if show_playhead {
        let playhead_x = waveform_left + waveform_width * progress;
        window.paint_quad(fill(
            Bounds::centered_at(
                point(playhead_x, center_y),
                size(px(2.), maximum_height + px(8.)),
            ),
            played_color,
        ));
        let mut knob = fill(
            Bounds::centered_at(point(playhead_x, center_y), size(px(8.), px(8.))),
            played_color,
        );
        knob.corner_radii = (4.).into();
        window.paint_quad(knob);
    }
}

fn metadata_description(metadata: AudioMetadata) -> String {
    let mut parts = Vec::new();
    if let Some(channels) = metadata.channels {
        parts.push(match channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            _ => format!("{channels} channels"),
        });
    }
    if let Some(sample_rate) = metadata.sample_rate {
        parts.push(format_sample_rate(sample_rate));
    }
    parts.push(format_file_size(metadata.file_size));
    parts.join(" • ")
}

fn format_sample_rate(sample_rate: u32) -> String {
    if sample_rate % 1_000 == 0 {
        format!("{} kHz", sample_rate / 1_000)
    } else {
        format!("{:.1} kHz", sample_rate as f64 / 1_000.0)
    }
}

fn format_file_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;

    if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{:02}:{seconds:02}", minutes % 60)
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bits_per_sample = 16u16;
        let block_align = channels * bits_per_sample / 8;
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = (samples.len() * std::mem::size_of::<i16>()) as u32;
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

    #[test]
    fn analyzes_metadata_and_normalized_waveform_peaks() {
        let sample_rate = 8_000u32;
        let samples = (0..sample_rate)
            .map(|index| if index % 64 < 32 { i16::MAX } else { 0 })
            .collect::<Vec<_>>();
        let wav = pcm_wav(sample_rate, 1, &samples);

        let analysis = analyze_audio(Arc::new(wav), Some("wav".to_string()));
        assert!(analysis.is_ok(), "test WAV should decode: {analysis:?}");
        let Ok(analysis) = analysis else {
            return;
        };

        assert_eq!(analysis.metadata.duration, Some(Duration::from_secs(1)));
        assert_eq!(analysis.metadata.channels, Some(1));
        assert_eq!(analysis.metadata.sample_rate, Some(sample_rate));
        assert_eq!(analysis.waveform_peaks.len(), WAVEFORM_PEAK_COUNT);
        assert_eq!(
            analysis
                .waveform_peaks
                .iter()
                .copied()
                .fold(0.0_f32, f32::max),
            1.0
        );
    }

    #[test]
    fn formats_audio_metadata_for_display() {
        assert_eq!(format_sample_rate(44_100), "44.1 kHz");
        assert_eq!(format_sample_rate(48_000), "48 kHz");
        assert_eq!(format_duration(Duration::from_secs(3_751)), "1:02:31");
        assert_eq!(
            metadata_description(AudioMetadata {
                file_size: 8 * 1024 * 1024,
                duration: None,
                channels: Some(2),
                sample_rate: Some(44_100),
            }),
            "Stereo • 44.1 kHz • 8.0 MiB"
        );
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<AudioView>(cx);
}
