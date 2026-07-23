mod analysis;
mod playback;

use std::{cell::RefCell, io::Cursor, path::Path, rc::Rc, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, SharedString, Task, Window, actions, canvas,
    div, fill, point, size,
};
use language::File as _;
use project::{AudioItem, AudioItemEvent, Project};
use rodio::Decoder;
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

use self::{
    analysis::{AudioMetrics, SilenceRange, WaveformBucket, analyze_audio},
    playback::PlaybackState,
};

const SEEK_STEP: Duration = Duration::from_secs(5);
const WAVEFORM_HORIZONTAL_PADDING: f32 = 12.0;
const VOLUME_BAR_HORIZONTAL_PADDING: f32 = 4.0;

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
    waveform: Option<Arc<Vec<WaveformBucket>>>,
    silence_ranges: Option<Arc<Vec<SilenceRange>>>,
    metrics: Option<Arc<AudioMetrics>>,
    analysis_error: Option<String>,
    analysis_task: Task<()>,
    scrub_position: Option<Duration>,
    hover_position: Option<Duration>,
    seek_bar_hovered: bool,
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
            waveform: None,
            silence_ranges: None,
            metrics: None,
            analysis_error: None,
            analysis_task,
            scrub_position: None,
            hover_position: None,
            seek_bar_hovered: false,
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
                        this.waveform = Some(analysis.waveform);
                        this.silence_ranges = Some(analysis.silence_ranges);
                        this.metrics = Some(analysis.metrics);
                        this.analysis_error = None;
                    }
                    Err(error) => {
                        log::error!("failed to analyze audio file: {error:?}");
                        this.waveform = None;
                        this.silence_ranges = None;
                        this.metrics = None;
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
                self.waveform = None;
                self.silence_ranges = None;
                self.metrics = None;
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
        let mut offset = self.playback.position().saturating_add(SEEK_STEP);
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
        let fraction = horizontal_progress(position.x, bounds, px(WAVEFORM_HORIZONTAL_PADDING))?;
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

    fn seek_bar_mouse_up(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.scrub_position.take() else {
            return;
        };
        self.playback.seek_to(position);
        self.hover_position = Some(position);
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

    fn set_volume_from_position(
        &mut self,
        position: gpui::Point<Pixels>,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = *bounds.borrow() else {
            return;
        };
        let Some(volume) =
            horizontal_progress(position.x, bounds, px(VOLUME_BAR_HORIZONTAL_PADDING))
        else {
            return;
        };
        self.playback.set_volume(volume);
        cx.notify();
    }

    fn volume_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_volume_from_position(event.position, bounds, cx);
        cx.stop_propagation();
    }

    fn volume_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        self.set_volume_from_position(event.position, bounds, cx);
        cx.stop_propagation();
    }

    fn toggle_mute(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.playback.toggle_mute();
        cx.notify();
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
        path = worktree.read(cx).root_name().join(&path).into();
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
        if self.playback.is_playing() {
            window.request_animation_frame();
        }
        let metadata = self.metadata;
        let playback_position = self.scrub_position.unwrap_or_else(|| {
            if self.playback.is_finished() {
                metadata
                    .duration
                    .unwrap_or_else(|| self.playback.position())
            } else {
                self.playback.position()
            }
        });
        let position = metadata
            .duration
            .map(|duration| playback_position.min(duration))
            .unwrap_or(playback_position);
        let progress = metadata
            .duration
            .map(|duration| playback_progress(position, duration))
            .unwrap_or(0.0);
        let title = self.audio_item.read(cx).file.file_name(cx).to_string();
        let format_label = self
            .format_hint(cx)
            .unwrap_or_else(|| "audio".to_string())
            .to_uppercase();
        let is_playing = self.playback.is_playing();
        let can_seek = metadata.duration.is_some();
        let show_milliseconds = metadata
            .duration
            .is_some_and(|duration| duration < Duration::from_secs(1));
        let volume = self.playback.volume();
        let muted = volume <= f32::EPSILON;
        let seek_bounds: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        let seek_bounds_for_canvas = seek_bounds.clone();
        let volume_bounds: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        let volume_bounds_for_canvas = volume_bounds.clone();
        let waveform = self.waveform.clone();
        let silence_ranges = self.silence_ranges.clone();
        let metrics = self.metrics.clone();
        let unplayed_color = cx.theme().colors().border_variant;
        let played_color = cx.theme().colors().text_accent;
        let hover_color = cx.theme().colors().text_muted.opacity(0.55);
        let silence_color = cx.theme().colors().text_muted.opacity(0.10);
        let volume_track_color = cx.theme().colors().border_variant;
        let volume_fill_color = cx.theme().colors().text_accent;
        let show_playhead = self.seek_bar_hovered
            || self.scrub_position.is_some()
            || self.focus_handle.is_focused(window);
        let hover_progress = self.hover_position.and_then(|hover_position| {
            metadata
                .duration
                .map(|duration| playback_progress(hover_position, duration))
        });
        let analysis_loading = self.waveform.is_none() && self.analysis_error.is_none();

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("AudioViewer")
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_backward))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::reset_playback))
            .size_full()
            .id("audio-viewer-scroll")
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_start()
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
                                                cx.listener(|this, _, window, cx| {
                                                    this.seek_bar_mouse_up(window, cx);
                                                }),
                                            )
                                            .on_mouse_up_out(
                                                MouseButton::Left,
                                                cx.listener(|this, _, window, cx| {
                                                    this.seek_bar_mouse_up(window, cx);
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
                                                    waveform.as_deref().map(Vec::as_slice),
                                                    silence_ranges.as_deref().map(Vec::as_slice),
                                                    metadata.duration,
                                                    progress,
                                                    hover_progress,
                                                    show_playhead,
                                                    played_color,
                                                    unplayed_color,
                                                    hover_color,
                                                    silence_color,
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
                                                    Label::new("Analyzing audio…")
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
                                    .child(format_duration(position, show_milliseconds))
                                    .when_some(self.hover_position, |this, hover_position| {
                                        this.child(format!(
                                            "Seek to {}",
                                            format_duration(hover_position, show_milliseconds)
                                        ))
                                    })
                                    .child(
                                        metadata
                                            .duration
                                            .map(|duration| {
                                                format_duration(duration, show_milliseconds)
                                            })
                                            .unwrap_or_else(|| "Unknown duration".to_string()),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .items_center()
                            .gap_2()
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
                                                style.bg(cx
                                                    .theme()
                                                    .colors()
                                                    .text_accent
                                                    .opacity(0.22))
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
                                            .disabled(!can_seek)
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
                            .child(
                                h_flex()
                                    .w(px(208.))
                                    .items_center()
                                    .justify_center()
                                    .gap_1()
                                    .child(
                                        IconButton::new(
                                            "audio-mute",
                                            if muted {
                                                IconName::AudioOff
                                            } else {
                                                IconName::AudioOn
                                            },
                                        )
                                        .shape(IconButtonShape::Square)
                                        .icon_size(IconSize::Small)
                                        .aria_label(if muted { "Unmute" } else { "Mute" })
                                        .tooltip(Tooltip::text(if muted {
                                            "Unmute"
                                        } else {
                                            "Mute"
                                        }))
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.toggle_mute(window, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .id("audio-volume")
                                            .w(px(120.))
                                            .h(px(24.))
                                            .cursor_pointer()
                                            .role(gpui::accesskit::Role::Slider)
                                            .aria_label("Volume")
                                            .aria_numeric_value(f64::from(volume * 100.0))
                                            .aria_min_numeric_value(0.0)
                                            .aria_max_numeric_value(100.0)
                                            .aria_numeric_value_step(1.0)
                                            .tooltip(Tooltip::text(format!(
                                                "Volume: {:.0}%",
                                                volume * 100.0
                                            )))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener({
                                                    let volume_bounds = volume_bounds.clone();
                                                    move |this, event, window, cx| {
                                                        this.volume_mouse_down(
                                                            event,
                                                            &volume_bounds,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                }),
                                            )
                                            .on_mouse_move(cx.listener({
                                                move |this, event, window, cx| {
                                                    this.volume_mouse_move(
                                                        event,
                                                        &volume_bounds,
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }))
                                            .child(
                                                canvas(
                                                    move |bounds, _, _| {
                                                        *volume_bounds_for_canvas.borrow_mut() =
                                                            Some(bounds);
                                                    },
                                                    move |bounds, _, window, _| {
                                                        paint_volume_bar(
                                                            bounds,
                                                            volume,
                                                            volume_track_color,
                                                            volume_fill_color,
                                                            window,
                                                        );
                                                    },
                                                )
                                                .size_full(),
                                            ),
                                    )
                                    .child(
                                        div().w(px(36.)).child(
                                            Label::new(format!("{:.0}%", volume * 100.0))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                    ),
                            ),
                    )
                    .child(render_analysis_panel(
                        metrics.as_deref(),
                        analysis_loading,
                        self.analysis_error.as_deref(),
                        cx,
                    ))
                    .when_some(self.playback.error(), |this, error| {
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

fn horizontal_progress(
    position_x: Pixels,
    bounds: Bounds<Pixels>,
    horizontal_padding: Pixels,
) -> Option<f32> {
    let width = bounds.size.width - horizontal_padding * 2.;
    if width <= Pixels::ZERO {
        return None;
    }
    Some(((position_x - bounds.left() - horizontal_padding) / width).clamp(0.0, 1.0))
}

fn paint_waveform(
    bounds: Bounds<Pixels>,
    waveform: Option<&[WaveformBucket]>,
    silence_ranges: Option<&[SilenceRange]>,
    duration: Option<Duration>,
    progress: f32,
    hover_progress: Option<f32>,
    show_playhead: bool,
    played_color: gpui::Hsla,
    unplayed_color: gpui::Hsla,
    hover_color: gpui::Hsla,
    silence_color: gpui::Hsla,
    window: &mut Window,
) {
    let center_y = (bounds.top() + bounds.bottom()) / 2.;
    let horizontal_padding = px(WAVEFORM_HORIZONTAL_PADDING);
    let waveform_left = bounds.left() + horizontal_padding;
    let waveform_width = (bounds.size.width - horizontal_padding * 2.).max(px(1.));
    let maximum_height = (bounds.size.height - px(24.)).max(px(2.));
    let half_height = maximum_height / 2.;

    if let (Some(ranges), Some(duration)) = (silence_ranges, duration) {
        for range in ranges {
            let start = playback_progress(range.start, duration);
            let end = playback_progress(range.end, duration);
            let range_width = waveform_width * (end - start).max(0.0);
            if range_width > Pixels::ZERO {
                window.paint_quad(fill(
                    Bounds::centered_at(
                        point(
                            waveform_left + waveform_width * start + range_width / 2.,
                            center_y,
                        ),
                        size(range_width, maximum_height + px(8.)),
                    ),
                    silence_color,
                ));
            }
        }
    }

    for guide_dbfs in [-6.0_f32, -12.0, -24.0] {
        let amplitude = 10_f32.powf(guide_dbfs / 20.0);
        for direction in [-1.0_f32, 1.0] {
            let guide_y = center_y - half_height * amplitude * direction;
            window.paint_quad(fill(
                Bounds::centered_at(
                    point(waveform_left + waveform_width / 2., guide_y),
                    size(waveform_width, px(1.)),
                ),
                unplayed_color.opacity(0.35),
            ));
        }
    }
    window.paint_quad(fill(
        Bounds::centered_at(
            point(waveform_left + waveform_width / 2., center_y),
            size(waveform_width, px(1.)),
        ),
        unplayed_color.opacity(0.7),
    ));

    if let Some(buckets) = waveform.filter(|buckets| !buckets.is_empty()) {
        let waveform_width_f32: f32 = waveform_width.into();
        let bar_count = ((waveform_width_f32 / 3.0).floor() as usize).clamp(1, buckets.len());
        let bar_width = px(2.);
        for bar_index in 0..bar_count {
            let bucket_start = bar_index.saturating_mul(buckets.len()) / bar_count;
            let bucket_end = ((bar_index + 1).saturating_mul(buckets.len()) / bar_count)
                .max(bucket_start + 1)
                .min(buckets.len());
            let mut minimum = 0.0_f32;
            let mut maximum = 0.0_f32;
            for bucket in buckets.get(bucket_start..bucket_end).unwrap_or_default() {
                minimum = minimum.min(bucket.minimum);
                maximum = maximum.max(bucket.maximum);
            }
            let bar_progress = (bar_index as f32 + 0.5) / bar_count as f32;
            let top = center_y - half_height * maximum.clamp(-1.0, 1.0);
            let bottom = center_y - half_height * minimum.clamp(-1.0, 1.0);
            let bar_height = bottom - top;
            if bar_height > Pixels::ZERO {
                let mut bar = fill(
                    Bounds::centered_at(
                        point(
                            waveform_left + waveform_width * bar_progress,
                            (top + bottom) / 2.,
                        ),
                        size(bar_width, bar_height),
                    ),
                    if bar_progress <= progress {
                        played_color
                    } else {
                        unplayed_color
                    },
                );
                bar.corner_radii = (1.).into();
                window.paint_quad(bar);
            }
        }
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

fn paint_volume_bar(
    bounds: Bounds<Pixels>,
    volume: f32,
    track_color: gpui::Hsla,
    fill_color: gpui::Hsla,
    window: &mut Window,
) {
    let center_y = (bounds.top() + bounds.bottom()) / 2.;
    let horizontal_padding = px(VOLUME_BAR_HORIZONTAL_PADDING);
    let left = bounds.left() + horizontal_padding;
    let width = (bounds.size.width - horizontal_padding * 2.).max(px(1.));
    let volume = volume.clamp(0.0, 1.0);

    let mut track = fill(
        Bounds::centered_at(point(left + width / 2., center_y), size(width, px(3.))),
        track_color,
    );
    track.corner_radii = (1.5).into();
    window.paint_quad(track);

    if volume > 0.0 {
        let fill_width = width * volume;
        let mut filled_track = fill(
            Bounds::centered_at(
                point(left + fill_width / 2., center_y),
                size(fill_width, px(3.)),
            ),
            fill_color,
        );
        filled_track.corner_radii = (1.5).into();
        window.paint_quad(filled_track);
    }

    let mut knob = fill(
        Bounds::centered_at(point(left + width * volume, center_y), size(px(8.), px(8.))),
        fill_color,
    );
    knob.corner_radii = (4.).into();
    window.paint_quad(knob);
}

fn render_analysis_panel(
    metrics: Option<&AudioMetrics>,
    loading: bool,
    error: Option<&str>,
    cx: &App,
) -> AnyElement {
    let content = if loading {
        analysis_status("Analyzing loudness, levels, and silence…", cx)
    } else if let Some(error) = error {
        analysis_status(&format!("Analysis unavailable: {error}"), cx)
    } else if let Some(metrics) = metrics {
        let near_clip_percentage = if metrics.total_samples == 0 {
            0.0
        } else {
            metrics.near_clipped_samples as f64 * 100.0 / metrics.total_samples as f64
        };

        let dc_offsets = metrics
            .dc_offsets
            .iter()
            .enumerate()
            .map(|(channel, offset)| format!("Ch{} {:+.3}%", channel + 1, offset * 100.0))
            .collect::<Vec<_>>()
            .join(" • ");
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .child(analysis_metric_card(
                        "Integrated",
                        format_db(metrics.integrated_lufs, "LUFS"),
                        "EBU R128 gated perceived loudness in LUFS. Useful for comparing TTS output loudness across clips; it can be unavailable for clips that are too short or silent.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "Active RMS",
                        format_db(metrics.active_speech_rms_dbfs, "dBFS"),
                        "Raw RMS energy in 20 ms windows that are not below -50 dBFS. Useful for comparing active TTS signal level, but it is not perceptually weighted.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "Sample peak",
                        format_db(metrics.sample_peak_dbfs, "dBFS"),
                        "Largest stored sample value in dBFS. Useful for checking digital headroom and potential clipping, but it does not measure inter-sample overshoot.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "True peak",
                        format_db(metrics.true_peak_dbtp, "dBTP"),
                        "EBU R128 reconstructed inter-sample peak in dBTP. Useful for finding playback overshoot that can exceed the stored sample peak.",
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .child(analysis_metric_card(
                        "Leading",
                        format_duration_compact(metrics.leading_silence),
                        "Silence at the start measured as consecutive 20 ms RMS windows below -50 dBFS, with no sub-50 ms merging. Useful for spotting TTS response latency or excess padding.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "Trailing",
                        format_duration_compact(metrics.trailing_silence),
                        "Silence at the end measured as consecutive 20 ms RMS windows below -50 dBFS, with no sub-50 ms merging. Useful for spotting excess tail padding in TTS output.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "Internal pauses",
                        metrics.internal_pause_count.to_string(),
                        "Number of internal silent runs made from 20 ms RMS windows below -50 dBFS, with no sub-50 ms merging. Edge silence is excluded; useful for reviewing TTS phrasing and unexpected gaps.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "Pause total",
                        format_duration_compact(metrics.internal_pause_total),
                        "Total duration of all internal silent runs, excluding leading and trailing silence. Runs use 20 ms RMS windows below -50 dBFS with no sub-50 ms merging; useful for comparing TTS pacing.",
                        cx,
                    ))
                    .child(analysis_metric_card(
                        "Longest pause",
                        format_duration_compact(metrics.internal_pause_longest),
                        "Duration of the longest internal silent run, excluding leading and trailing silence. Runs use 20 ms RMS windows below -50 dBFS with no sub-50 ms merging; useful for finding disruptive TTS pauses.",
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .child(analysis_wide_card(
                                            "DC offset",
                                            dc_offsets,
                                            "Per-channel sample mean shown as a signed percentage of full scale. DC offset wastes headroom and can indicate synthesis or processing bugs in a TTS pipeline.",
                                            cx,
                                        ))
                    .child(analysis_metric_card(
                        "Near-clipped",
                        format!(
                            "{} ({near_clip_percentage:.3}%)",
                            metrics.near_clipped_samples
                        ),
                        "Individual channel samples at or above -0.1 dBFS, shown as count and percentage. Useful for finding low headroom in TTS renders, but it is not proof that clipping occurred.",
                        cx,
                    )),
            )

            .into_any_element()
    } else {
        analysis_status("Analysis unavailable", cx)
    };

    v_flex()
        .w_full()
        .p_3()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().editor_background)
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_1()
                .child(Label::new("Analysis").size(LabelSize::Small))
                .child(
                    Label::new("Silence: 20 ms RMS windows below -50 dBFS")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
        .child(content)
        .into_any_element()
}

fn analysis_metric_card(
    label: &'static str,
    value: String,
    tooltip: &'static str,
    cx: &App,
) -> AnyElement {
    v_flex()
        .id(label)
        .min_w(px(120.))
        .flex_1()
        .p_2()
        .gap_0p5()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().elevated_surface_background)
        .tooltip(Tooltip::text(tooltip))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value).size(LabelSize::Small).truncate())
        .into_any_element()
}

fn analysis_wide_card(
    label: &'static str,
    value: String,
    tooltip: &'static str,
    cx: &App,
) -> AnyElement {
    v_flex()
        .id(label)
        .min_w(px(260.))
        .flex_1()
        .p_2()
        .gap_0p5()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().elevated_surface_background)
        .tooltip(Tooltip::text(tooltip))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value).size(LabelSize::Small).line_clamp(2))
        .into_any_element()
}

fn analysis_status(message: &str, cx: &App) -> AnyElement {
    div()
        .w_full()
        .p_3()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().elevated_surface_background)
        .child(
            Label::new(message.to_string())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .line_clamp(2),
        )
        .into_any_element()
}

fn format_db(value: Option<f64>, unit: &str) -> String {
    value
        .map(|value| format!("{value:.1} {unit}"))
        .unwrap_or_else(|| format!("Unavailable {unit}"))
}

fn format_duration_compact(duration: Duration) -> String {
    if duration < Duration::from_secs(10) {
        format!("{:.2} s", duration.as_secs_f64())
    } else {
        format_duration(duration, false)
    }
}

fn playback_progress(position: Duration, duration: Duration) -> f32 {
    (position.as_secs_f32() / duration.as_secs_f32().max(f32::EPSILON)).clamp(0.0, 1.0)
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
    if sample_rate.is_multiple_of(1_000) {
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

fn format_duration(duration: Duration, show_milliseconds: bool) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{:02}:{seconds:02}", minutes % 60)
    } else if show_milliseconds {
        format!("{minutes}:{seconds:02}.{:03}", duration.subsec_millis())
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_pointer_positions_to_padded_waveform() {
        let bounds = Bounds::new(point(px(100.), px(0.)), size(px(200.), px(20.)));
        let padding = px(WAVEFORM_HORIZONTAL_PADDING);

        assert_eq!(horizontal_progress(px(100.), bounds, padding), Some(0.0));
        assert_eq!(horizontal_progress(px(112.), bounds, padding), Some(0.0));
        assert_eq!(horizontal_progress(px(200.), bounds, padding), Some(0.5));
        assert_eq!(horizontal_progress(px(288.), bounds, padding), Some(1.0));
        assert_eq!(horizontal_progress(px(300.), bounds, padding), Some(1.0));
    }

    #[test]
    fn fills_progress_for_subsecond_audio() {
        let duration = Duration::from_micros(887_755);
        assert_eq!(playback_progress(duration, duration), 1.0);
        assert_eq!(playback_progress(duration / 2, duration), 0.5);
    }

    #[test]
    fn formats_audio_metadata_for_display() {
        assert_eq!(format_sample_rate(44_100), "44.1 kHz");
        assert_eq!(format_sample_rate(48_000), "48 kHz");
        assert_eq!(
            format_duration(Duration::from_secs(3_751), false),
            "1:02:31"
        );
        assert_eq!(
            format_duration(Duration::from_micros(887_755), true),
            "0:00.887"
        );
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
