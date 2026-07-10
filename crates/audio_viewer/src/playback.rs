use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result};
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};

use super::decode_audio;

const PLAYBACK_POSITION_UPDATE_INTERVAL: Duration = Duration::from_millis(8);
const SEEK_FADE_DURATION: Duration = Duration::from_millis(8);
const SEEK_FADE_STEPS: u32 = 8;

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
    completed_seek_id: u64,
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
        volume: f32,
    },
    Pause,
    Resume,
    Seek {
        position: Duration,
        seek_id: u64,
    },
    SetVolume {
        volume: f32,
    },
    Stop,
    Shutdown,
}

struct PlaybackController {
    command_sender: Sender<PlaybackCommand>,
    snapshot: Arc<SharedPlaybackSnapshot>,
    thread: Option<thread::JoinHandle<()>>,
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
            thread: Some(thread),
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

    fn set_idle_position(&self, position: Duration) {
        self.snapshot.update(|snapshot| {
            snapshot.status = PlaybackStatus::Idle;
            snapshot.position = position;
            snapshot.error = None;
        });
    }
}

impl Drop for PlaybackController {
    fn drop(&mut self) {
        if self.command_sender.send(PlaybackCommand::Shutdown).is_err() {
            log::debug!("audio playback thread had already stopped");
        }
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
        {
            log::error!("audio playback thread panicked: {error:?}");
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
        let command = if session
            .as_ref()
            .is_some_and(|session| !session.player.is_paused())
        {
            match command_receiver.recv_timeout(PLAYBACK_POSITION_UPDATE_INTERVAL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match command_receiver.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            }
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
            volume,
        } => {
            snapshot.update(|snapshot| {
                snapshot.status = PlaybackStatus::Starting;
                snapshot.position = offset;
                snapshot.error = None;
            });
            match start_playback_session(
                bytes,
                format_hint.as_deref(),
                offset,
                volume,
                snapshot.clone(),
            ) {
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
        PlaybackCommand::Seek { position, seek_id } => {
            if let Some(session) = session {
                match seek_with_fade(&session.player, position) {
                    Ok(()) => snapshot.update(|snapshot| {
                        snapshot.position = position;
                        snapshot.error = None;
                        snapshot.completed_seek_id = seek_id;
                    }),
                    Err(error) => {
                        let position = session.player.get_pos();
                        log::error!("failed to seek audio file: {error:?}");
                        snapshot.update(|snapshot| {
                            snapshot.position = position;
                            snapshot.error =
                                Some(format!("Could not seek in the audio file: {error}"));
                            snapshot.completed_seek_id = seek_id;
                        });
                    }
                }
            }
        }
        PlaybackCommand::SetVolume { volume } => {
            if let Some(session) = session {
                session.player.set_volume(volume);
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
    volume: f32,
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
    if offset.is_zero() {
        player.set_volume(volume);
    } else {
        player.set_volume(0.0);
    }
    player.append(decoder);
    if !offset.is_zero() {
        player
            .try_seek(offset)
            .context("Could not seek to the playback position")?;
        fade_player_volume(&player, 0.0, volume);
    }

    Ok(PlaybackSession {
        _output: output,
        player,
    })
}

fn seek_with_fade(player: &Player, position: Duration) -> Result<()> {
    if player.is_paused() {
        return player
            .try_seek(position)
            .context("Could not seek to the playback position");
    }

    let volume = player.volume();
    fade_player_volume(player, volume, 0.0);
    let seek_result = player
        .try_seek(position)
        .context("Could not seek to the playback position");
    fade_player_volume(player, 0.0, volume);
    seek_result
}

fn fade_player_volume(player: &Player, start: f32, end: f32) {
    let step_duration = SEEK_FADE_DURATION / SEEK_FADE_STEPS;
    for step in 1..=SEEK_FADE_STEPS {
        let progress = step as f32 / SEEK_FADE_STEPS as f32;
        player.set_volume(start + (end - start) * progress);
        thread::sleep(step_duration);
    }
}

pub(super) struct PlaybackState {
    controller: Option<PlaybackController>,
    status: PlaybackStatus,
    position: Duration,
    error: Option<String>,
    volume: f32,
    last_nonzero_volume: f32,
    next_seek_id: u64,
    pending_seek_id: Option<u64>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            controller: None,
            status: PlaybackStatus::default(),
            position: Duration::ZERO,
            error: None,
            volume: 1.0,
            last_nonzero_volume: 1.0,
            next_seek_id: 0,
            pending_seek_id: None,
        }
    }
}

impl PlaybackState {
    pub(super) fn position(&self) -> Duration {
        self.position
    }

    pub(super) fn volume(&self) -> f32 {
        self.volume
    }

    pub(super) fn is_playing(&self) -> bool {
        matches!(
            self.status,
            PlaybackStatus::Starting | PlaybackStatus::Playing
        )
    }

    pub(super) fn is_finished(&self) -> bool {
        self.status == PlaybackStatus::Finished
    }

    pub(super) fn error(&self) -> Option<String> {
        self.error.clone()
    }

    fn is_paused(&self) -> bool {
        self.status == PlaybackStatus::Paused
    }

    pub(super) fn synchronize(&mut self) {
        let Some(controller) = &self.controller else {
            return;
        };
        let snapshot = controller.snapshot();
        self.apply_snapshot(snapshot);
    }

    fn apply_snapshot(&mut self, snapshot: PlaybackSnapshot) {
        if self
            .pending_seek_id
            .is_some_and(|seek_id| snapshot.completed_seek_id < seek_id)
            && snapshot.status != PlaybackStatus::Failed
        {
            return;
        }

        self.pending_seek_id = None;
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

    pub(super) fn stop(&mut self) {
        if self.controller.is_some() && !self.send(PlaybackCommand::Stop) {
            self.controller = None;
            return;
        }
        self.status = PlaybackStatus::Idle;
        self.position = Duration::ZERO;
        self.error = None;
        self.pending_seek_id = None;
    }

    pub(super) fn pause(&mut self) {
        if self.send(PlaybackCommand::Pause) {
            self.status = PlaybackStatus::Paused;
        }
    }

    pub(super) fn set_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.volume = volume;
        if volume > 0.0 {
            self.last_nonzero_volume = volume;
        }

        if matches!(
            self.status,
            PlaybackStatus::Starting | PlaybackStatus::Playing | PlaybackStatus::Paused
        ) && !self.send(PlaybackCommand::SetVolume { volume })
        {
            return;
        }
    }

    pub(super) fn toggle_mute(&mut self) {
        if self.volume > 0.0 {
            self.set_volume(0.0);
        } else {
            self.set_volume(self.last_nonzero_volume);
        }
    }

    pub(super) fn seek_to(&mut self, offset: Duration) {
        self.position = offset;
        if matches!(
            self.status,
            PlaybackStatus::Starting | PlaybackStatus::Playing | PlaybackStatus::Paused
        ) {
            self.next_seek_id = self.next_seek_id.wrapping_add(1).max(1);
            let seek_id = self.next_seek_id;
            self.pending_seek_id = Some(seek_id);
            if !self.send(PlaybackCommand::Seek {
                position: offset,
                seek_id,
            }) {
                self.pending_seek_id = None;
            }
        } else if matches!(self.status, PlaybackStatus::Idle | PlaybackStatus::Finished) {
            self.status = PlaybackStatus::Idle;
            self.error = None;
            if let Some(controller) = &self.controller {
                controller.set_idle_position(offset);
            }
        }
    }

    pub(super) fn play(&mut self, bytes: Arc<Vec<u8>>, format_hint: Option<String>) {
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
            volume: self.volume,
        };
        if self.send(command) {
            self.status = PlaybackStatus::Starting;
            self.error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_snapshot_is_ignored_while_seek_is_pending() {
        let position = Duration::from_secs(12);
        let error = Some("existing error".to_string());
        let mut playback = PlaybackState {
            status: PlaybackStatus::Paused,
            position,
            error: error.clone(),
            pending_seek_id: Some(2),
            ..PlaybackState::default()
        };

        playback.apply_snapshot(PlaybackSnapshot {
            status: PlaybackStatus::Playing,
            position: Duration::from_secs(4),
            error: None,
            completed_seek_id: 1,
        });

        assert_eq!(playback.status, PlaybackStatus::Paused);
        assert_eq!(playback.position, position);
        assert_eq!(playback.error, error);
        assert_eq!(playback.pending_seek_id, Some(2));
    }

    #[test]
    fn acknowledged_failed_seek_applies_actual_position_and_error() {
        let actual_position = Duration::from_secs(7);
        let mut playback = PlaybackState {
            status: PlaybackStatus::Playing,
            position: Duration::from_secs(20),
            pending_seek_id: Some(3),
            ..PlaybackState::default()
        };

        playback.apply_snapshot(PlaybackSnapshot {
            status: PlaybackStatus::Playing,
            position: actual_position,
            error: Some("seek failed".to_string()),
            completed_seek_id: 3,
        });

        assert_eq!(playback.status, PlaybackStatus::Playing);
        assert_eq!(playback.position, actual_position);
        assert_eq!(playback.error.as_deref(), Some("seek failed"));
        assert_eq!(playback.pending_seek_id, None);
    }

    #[test]
    fn seeking_from_finished_transitions_to_idle_and_preserves_position() {
        let position = Duration::from_secs(9);
        let mut playback = PlaybackState {
            status: PlaybackStatus::Finished,
            position: Duration::from_secs(30),
            error: Some("previous error".to_string()),
            ..PlaybackState::default()
        };

        playback.seek_to(position);

        assert_eq!(playback.status, PlaybackStatus::Idle);
        assert_eq!(playback.position, position);
        assert_eq!(playback.error, None);
    }

    #[test]
    fn volume_is_clamped_to_supported_range() {
        let mut playback = PlaybackState::default();

        playback.set_volume(2.0);
        assert_eq!(playback.volume(), 1.0);

        playback.set_volume(-1.0);
        assert_eq!(playback.volume(), 0.0);
    }

    #[test]
    fn mute_restores_previous_volume() {
        let mut playback = PlaybackState::default();
        playback.set_volume(0.35);

        playback.toggle_mute();
        assert_eq!(playback.volume(), 0.0);

        playback.toggle_mute();
        assert_eq!(playback.volume(), 0.35);
    }
}
