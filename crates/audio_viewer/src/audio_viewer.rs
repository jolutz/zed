use std::{
    cell::RefCell,
    io::Cursor,
    path::Path,
    rc::Rc,
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
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, SharedString, Task, Window, canvas, div, fill,
    point, size,
};
use language::File as _;
use project::{AudioItem, AudioItemEvent, Project};
use rodio::{Decoder, DeviceSinkBuilder, Source};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{
    ButtonStyle, Color, Icon, IconButton, IconButtonShape, IconName, IconSize, Label, LabelSize,
    TintColor, Tooltip, prelude::*,
};
use util::paths::PathExt;
use workspace::{
    ItemSettings, Pane, ToolbarItemLocation, WorkspaceId,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ProjectItem, TabContentParams},
};

const SEEK_STEP: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioMetadata {
    file_size: u64,
    duration: Option<Duration>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
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
        cx.subscribe(&audio_item, Self::on_audio_event).detach();
        cx.on_release(|this, _| {
            this.playback.stop();
        })
        .detach();
        let metadata = AudioMetadata::from_bytes(audio_item.read(cx).bytes.as_ref());

        Self {
            audio_item,
            project,
            focus_handle: cx.focus_handle(),
            playback: PlaybackState::default(),
            metadata,
        }
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
                self.metadata = AudioMetadata::from_bytes(self.audio_item.read(cx).bytes.as_ref());
                cx.notify();
            }
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
        let duration = self.metadata.duration;
        let mut offset = self.playback.position() + SEEK_STEP;
        if let Some(duration) = duration {
            offset = offset.min(duration);
        }
        self.playback.seek_to(offset, bytes, cx);
        cx.notify();
    }

    fn seek_to_position(
        &mut self,
        position: gpui::Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(duration) = self.metadata.duration else {
            return;
        };
        let fraction = ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.);
        let offset = duration.mul_f32(fraction);
        let bytes = self.audio_item.read(cx).bytes.clone();
        self.playback.seek_to(offset, bytes, cx);
        cx.notify();
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
        self.seek_to_position(event.position, bounds, cx);
        cx.stop_propagation();
    }

    fn seek_bar_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(bounds) = *bounds.borrow() else {
            return;
        };
        self.seek_to_position(event.position, bounds, cx);
        cx.stop_propagation();
    }

    fn schedule_progress_updates(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let keep_playing = this
                    .update(cx, |this, cx| {
                        let Some(duration) = this.metadata.duration else {
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
            metadata: self.metadata,
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
        let metadata = self.metadata;
        let position = metadata
            .duration
            .map(|duration| self.playback.position().min(duration))
            .unwrap_or_else(|| self.playback.position());
        let max_seconds = metadata
            .duration
            .map(|duration| duration.as_secs_f32().max(1.0))
            .unwrap_or(1.0);
        let progress = (position.as_secs_f32() / max_seconds).clamp(0., 1.);
        let title = self.audio_item.read(cx).file.file_name(cx).to_string();
        let is_playing = self.playback.is_playing();
        let seek_bounds: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        let seek_bounds_for_canvas = seek_bounds.clone();
        let track_color = cx.theme().colors().border_variant;
        let played_color = cx.theme().status().info;
        let knob_color = cx.theme().colors().text;
        let shadow_color = gpui::black().opacity(0.18);

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("AudioViewer")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                h_flex()
                    .id("audio-viewer")
                    .w_full()
                    .max_w(px(820.))
                    .p_5()
                    .gap_6()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().elevated_surface_background)
                    .child(
                        v_flex()
                            .w(px(180.))
                            .h(px(180.))
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .gap_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                div()
                                    .w(px(72.))
                                    .h(px(72.))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(cx.theme().status().info.opacity(0.14))
                                    .child(
                                        Icon::new(IconName::AudioOn)
                                            .size(IconSize::XLarge)
                                            .color(Color::Info),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new(title).size(LabelSize::Large))
                                    .child(
                                        Label::new("Audio file")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .id("audio-seek-bar")
                                            .w_full()
                                            .h_8()
                                            .cursor_pointer()
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
                                            .child(
                                                canvas(
                                                    move |bounds, _, _| {
                                                        *seek_bounds_for_canvas.borrow_mut() =
                                                            Some(bounds);
                                                    },
                                                    move |bounds, _, window, _| {
                                                        let track_height = px(8.);
                                                        let center_y =
                                                            (bounds.top() + bounds.bottom()) / 2.;
                                                        let track_bounds = Bounds::from_corners(
                                                            point(
                                                                bounds.left(),
                                                                center_y - track_height / 2.,
                                                            ),
                                                            point(
                                                                bounds.right(),
                                                                center_y + track_height / 2.,
                                                            ),
                                                        );
                                                        let played_width =
                                                            bounds.size.width * progress;
                                                        let played_bounds = Bounds::from_corners(
                                                            track_bounds.origin,
                                                            point(
                                                                bounds.left() + played_width,
                                                                track_bounds.bottom(),
                                                            ),
                                                        );
                                                        let knob_center = point(
                                                            bounds.left() + played_width,
                                                            center_y,
                                                        );
                                                        let knob_bounds = Bounds::centered_at(
                                                            knob_center,
                                                            size(px(18.), px(18.)),
                                                        );

                                                        let mut track =
                                                            fill(track_bounds, track_color);
                                                        track.corner_radii = (4.).into();
                                                        window.paint_quad(track);

                                                        let mut played =
                                                            fill(played_bounds, played_color);
                                                        played.corner_radii = (4.).into();
                                                        window.paint_quad(played);

                                                        let mut knob_shadow =
                                                            fill(knob_bounds, shadow_color);
                                                        knob_shadow.corner_radii = (9.).into();
                                                        window.paint_quad(knob_shadow);

                                                        let mut knob = fill(
                                                            Bounds::centered_at(
                                                                knob_center,
                                                                size(px(12.), px(12.)),
                                                            ),
                                                            knob_color,
                                                        );
                                                        knob.corner_radii = (6.).into();
                                                        window.paint_quad(knob);
                                                    },
                                                )
                                                .size_full(),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .justify_between()
                                            .text_sm()
                                            .text_color(cx.theme().colors().text_muted)
                                            .child(format_duration(position))
                                            .child(
                                                metadata
                                                    .duration
                                                    .map(format_duration)
                                                    .unwrap_or_else(|| {
                                                        "Unknown duration".to_string()
                                                    }),
                                            ),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_4()
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                IconButton::new(
                                                    "audio-seek-backward",
                                                    IconName::ArrowLeft,
                                                )
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::Small)
                                                .tooltip(Tooltip::text("Back 5 seconds"))
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
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::Medium)
                                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                                .tooltip(Tooltip::text(if is_playing {
                                                    "Pause"
                                                } else {
                                                    "Play"
                                                }))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.toggle_playback(window, cx);
                                                })),
                                            )
                                            .child(
                                                IconButton::new("audio-stop", IconName::Stop)
                                                    .shape(IconButtonShape::Square)
                                                    .icon_size(IconSize::Small)
                                                    .tooltip(Tooltip::text("Stop"))
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.stop_playback(window, cx);
                                                        },
                                                    )),
                                            )
                                            .child(
                                                IconButton::new(
                                                    "audio-seek-forward",
                                                    IconName::ArrowRight,
                                                )
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::Small)
                                                .tooltip(Tooltip::text("Forward 5 seconds"))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.seek_forward(window, cx);
                                                })),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(metadata_chip(
                                                format_file_size(metadata.file_size),
                                                cx,
                                            ))
                                            .when_some(metadata.channels, |this, channels| {
                                                this.child(metadata_chip(
                                                    format!("{channels} ch"),
                                                    cx,
                                                ))
                                            })
                                            .when_some(
                                                metadata.sample_rate,
                                                |this, sample_rate| {
                                                    this.child(metadata_chip(
                                                        format!("{} kHz", sample_rate / 1000),
                                                        cx,
                                                    ))
                                                },
                                            ),
                                    ),
                            )
                            .when_some(self.playback.error.clone(), |this, error| {
                                this.child(Label::new(error).color(Color::Error).buffer_font(cx))
                            }),
                    ),
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

fn metadata_chip(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(cx.theme().colors().editor_background)
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Label::new(text.into())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
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
            let source = source.skip_duration(offset).stoppable().periodic_access(
                Duration::from_millis(50),
                move |source: &mut rodio::source::Stoppable<_>| {
                    if thread_stop_signal.load(Ordering::Relaxed) {
                        source.stop();
                    }
                },
            );

            let Ok(mut output) = DeviceSinkBuilder::open_default_sink() else {
                log::error!("failed to open audio output device");
                return;
            };
            output.log_on_drop(false);
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
