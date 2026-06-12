use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Task, Window, div,
};
use language::{DiskState, File as _};
use project::{Project, ProjectEntryId, ProjectPath};
use rodio::{Decoder, Source};
use theme_settings::ThemeSettings;
use ui::{Color, Icon, IconButton, IconName, Label, LabelSize, ProgressBar, Tooltip, prelude::*};
use util::paths::PathExt;
use workspace::{
    ItemSettings, Pane, ToolbarItemLocation, WorkspaceId,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ProjectItem, TabContentParams},
};
use worktree::LoadedBinaryFile;

const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "flac", "ogg"];
const SEEK_STEP: Duration = Duration::from_secs(5);

pub struct AudioItem {
    file: Arc<worktree::File>,
    bytes: Arc<Vec<u8>>,
    metadata: AudioMetadata,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioMetadata {
    file_size: u64,
    duration: Option<Duration>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
}

impl AudioItem {
    fn new(file: Arc<worktree::File>, bytes: Vec<u8>) -> Self {
        let metadata = AudioMetadata::from_bytes(&bytes);
        Self {
            file,
            bytes: Arc::new(bytes),
            metadata,
        }
    }

    fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        }
    }

    fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }
}

impl AudioMetadata {
    fn from_bytes(bytes: &[u8]) -> Self {
        let file_size = bytes.len() as u64;
        let Ok(decoder) = Decoder::new(Cursor::new(bytes.to_vec())) else {
            return Self {
                file_size,
                ..Default::default()
            };
        };

        Self {
            file_size,
            duration: decoder.total_duration(),
            channels: Some(decoder.channels().get()),
            sample_rate: Some(decoder.sample_rate().get()),
        }
    }
}

pub fn is_audio_file(project: &Entity<Project>, path: &ProjectPath, cx: &App) -> bool {
    let extension = util::maybe!({
        let worktree_abs_path = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)?
            .read(cx)
            .abs_path();
        path.path
            .extension()
            .or_else(|| worktree_abs_path.extension()?.to_str())
            .map(str::to_lowercase)
    });

    extension
        .as_deref()
        .is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension))
}

impl project::ProjectItem for AudioItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !is_audio_file(project, path, cx) {
            return None;
        }

        let project_path = path.clone();
        let worktree = project
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx)?;
        let load_file = worktree.update(cx, |worktree, cx| {
            worktree.load_binary_file(project_path.path.as_ref(), cx)
        });

        Some(cx.spawn(async move |cx| {
            let LoadedBinaryFile { file, content } = load_file.await?;
            Ok(cx.new(|_| AudioItem::new(file, content)))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
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
}

#[derive(Default)]
struct PlaybackState {
    handle: Option<PlaybackHandle>,
    paused_at: Duration,
    error: Option<String>,
}

struct PlaybackHandle {
    stop_signal: Arc<AtomicBool>,
    started_at: Instant,
    offset: Duration,
}

impl Drop for PlaybackHandle {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::Relaxed);
    }
}

impl PlaybackState {
    fn position(&self) -> Duration {
        if let Some(handle) = &self.handle {
            handle.offset + handle.started_at.elapsed()
        } else {
            self.paused_at
        }
    }

    fn is_playing(&self) -> bool {
        self.handle.is_some()
    }

    fn stop(&mut self) {
        self.handle.take();
        self.paused_at = Duration::ZERO;
    }

    fn pause(&mut self) {
        let position = self.position();
        self.handle.take();
        self.paused_at = position;
    }

    fn seek_to(&mut self, offset: Duration, bytes: Arc<Vec<u8>>, cx: &mut Context<AudioView>) {
        let should_resume = self.is_playing();
        self.handle.take();
        self.paused_at = offset;
        if should_resume {
            self.play(bytes, cx);
        }
    }

    fn play(&mut self, bytes: Arc<Vec<u8>>, cx: &mut Context<AudioView>) {
        if self.handle.is_some() {
            return;
        }

        let offset = self.paused_at;
        match start_playback(bytes, offset) {
            Ok(handle) => {
                self.error = None;
                self.handle = Some(handle);
            }
            Err(error) => {
                self.error = Some(error.to_string());
                log::error!("failed to play audio file: {error:?}");
            }
        }
        cx.notify();
    }
}

impl AudioView {
    pub fn new(
        audio_item: Entity<AudioItem>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this, _| {
            this.playback.stop();
        })
        .detach();

        Self {
            audio_item,
            project,
            focus_handle: cx.focus_handle(),
            playback: PlaybackState::default(),
        }
    }

    fn toggle_playback(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.playback.is_playing() {
            self.playback.pause();
        } else {
            let bytes = self.audio_item.read(cx).bytes.clone();
            self.playback.play(bytes, cx);
            self.schedule_progress_updates(cx);
        }
        cx.notify();
    }

    fn stop_playback(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.playback.stop();
        cx.notify();
    }

    fn seek_backward(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let bytes = self.audio_item.read(cx).bytes.clone();
        let position = self.playback.position();
        self.playback
            .seek_to(position.saturating_sub(SEEK_STEP), bytes, cx);
        cx.notify();
    }

    fn seek_forward(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let bytes = self.audio_item.read(cx).bytes.clone();
        let duration = self.audio_item.read(cx).metadata.duration;
        let mut offset = self.playback.position() + SEEK_STEP;
        if let Some(duration) = duration {
            offset = offset.min(duration);
        }
        self.playback.seek_to(offset, bytes, cx);
        cx.notify();
    }

    fn schedule_progress_updates(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let keep_playing = this
                    .update(cx, |this, cx| {
                        let Some(duration) = this.audio_item.read(cx).metadata.duration else {
                            let playing = this.playback.is_playing();
                            if playing {
                                cx.notify();
                            }
                            return playing;
                        };

                        if this.playback.position() >= duration {
                            this.playback.stop();
                            cx.notify();
                            false
                        } else {
                            let playing = this.playback.is_playing();
                            if playing {
                                cx.notify();
                            }
                            playing
                        }
                    })
                    .unwrap_or(false);

                if !keep_playing {
                    break;
                }
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
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self {
            audio_item: self.audio_item.clone(),
            project: self.project.clone(),
            focus_handle: cx.focus_handle(),
            playback: PlaybackState::default(),
        })))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metadata = self.audio_item.read(cx).metadata;
        let position = metadata
            .duration
            .map(|duration| self.playback.position().min(duration))
            .unwrap_or_else(|| self.playback.position());
        let max_seconds = metadata
            .duration
            .map(|duration| duration.as_secs_f32().max(1.0))
            .unwrap_or(1.0);
        let title = self.audio_item.read(cx).file.file_name(cx).to_string();
        let is_playing = self.playback.is_playing();

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("AudioViewer")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("audio-viewer")
                    .w_full()
                    .max_w(px(640.))
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(Label::new(title).size(LabelSize::Large))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                IconButton::new("audio-seek-backward", IconName::ArrowLeft)
                                    .tooltip(|_, cx| Tooltip::text("Back 5 seconds", cx))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.seek_backward(window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new(
                                    "audio-play-pause",
                                    if is_playing {
                                        IconName::DebugPause
                                    } else {
                                        IconName::PlayFilled
                                    },
                                )
                                .tooltip(|_, cx| {
                                    Tooltip::text(if is_playing { "Pause" } else { "Play" }, cx)
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_playback(window, cx);
                                })),
                            )
                            .child(
                                IconButton::new("audio-stop", IconName::Stop)
                                    .tooltip(|_, cx| Tooltip::text("Stop", cx))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.stop_playback(window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new("audio-seek-forward", IconName::ArrowRight)
                                    .tooltip(|_, cx| Tooltip::text("Forward 5 seconds", cx))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.seek_forward(window, cx);
                                    })),
                            ),
                    )
                    .child(ProgressBar::new(
                        "audio-progress",
                        position.as_secs_f32(),
                        max_seconds,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_sm()
                            .text_color(cx.theme().colors().text_muted)
                            .child(format_duration(position))
                            .child(
                                metadata
                                    .duration
                                    .map(format_duration)
                                    .unwrap_or_else(|| "Unknown duration".to_string()),
                            ),
                    )
                    .child(metadata_text(metadata))
                    .when_some(self.playback.error.clone(), |this, error| {
                        this.child(Label::new(error).color(Color::Error).buffer_font(cx))
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

fn metadata_text(metadata: AudioMetadata) -> String {
    let mut parts = vec![format_file_size(metadata.file_size)];
    if let Some(channels) = metadata.channels {
        parts.push(format!("{channels} channels"));
    }
    if let Some(sample_rate) = metadata.sample_rate {
        parts.push(format!("{sample_rate} Hz"));
    }
    parts.join(" | ")
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
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes}:{seconds:02}")
}

fn start_playback(bytes: Arc<Vec<u8>>, offset: Duration) -> Result<PlaybackHandle> {
    let stop_signal = Arc::new(AtomicBool::new(false));
    let thread_stop_signal = stop_signal.clone();
    let playback_stop_signal = stop_signal.clone();

    thread::Builder::new()
        .name("AudioFileViewerPlayback".to_string())
        .spawn(move || {
            let cursor = Cursor::new(bytes.as_ref().clone());
            let source = match Decoder::new(cursor) {
                Ok(source) => source,
                Err(error) => {
                    log::error!("failed to decode audio file: {error:?}");
                    return;
                }
            };
            let source = source
                .skip_duration(offset)
                .stoppable()
                .periodic_access(
                    Duration::from_millis(50),
                    move |source: &mut rodio::source::Stoppable<_>| {
                        if thread_stop_signal.load(Ordering::Relaxed) {
                            source.stop();
                        }
                    },
                );

            let Ok(output) = audio::open_test_output(None) else {
                log::error!("failed to open audio output device");
                return;
            };
            output.mixer().add(source);

            while !playback_stop_signal.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
        })?;

    Ok(PlaybackHandle {
        stop_signal,
        started_at: Instant::now(),
        offset,
    })
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<AudioView>(cx);
}
