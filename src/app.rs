// SPDX-License-Identifier: MPL-2.0

//! The panel applet: a state machine over record -> transcribe -> type,
//! plus a popup for status and settings.

use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::iced::core::window;
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Length, Rectangle, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::widget::dropdown::popup_dropdown;
use cosmic::widget::{
    button, column, divider, list_column, progress_bar, row, settings, text, text_input, toggler,
};
use cosmic::{Element, cosmic_config};

use crate::config::{APP_ID, WhisprConfig};
use crate::{audio, ipc, stt, typer};

/// How often the level meter and elapsed timer refresh while recording.
const TICK: Duration = Duration::from_millis(100);
/// Level meter smoothing: how much of the previous reading to keep.
const METER_DECAY: f32 = 0.6;
/// Offered in the settings dropdown; 0 is fastest, higher values suit
/// applications that drop keys arriving in the same millisecond.
const DELAY_PRESETS: [u64; 6] = [0, 2, 4, 8, 16, 32];
const DELAY_LABELS: [&str; 6] = ["0 ms", "2 ms", "4 ms", "8 ms", "16 ms", "32 ms"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Idle,
    /// The capture thread is opening the device.
    Starting,
    Recording,
    Transcribing,
    Typing,
}

impl Status {
    fn icon(self) -> &'static str {
        match self {
            Self::Idle => "audio-input-microphone-symbolic",
            Self::Starting | Self::Recording => "media-record-symbolic",
            Self::Transcribing | Self::Typing => "content-loading-symbolic",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Starting => "Opening microphone…",
            Self::Recording => "Recording",
            Self::Transcribing => "Transcribing…",
            Self::Typing => "Typing…",
        }
    }

    fn is_busy(self) -> bool {
        matches!(self, Self::Transcribing | Self::Typing)
    }
}

pub struct Whispr {
    core: Core,
    popup: Option<Id>,
    config_handle: Option<cosmic_config::Config>,
    config: WhisprConfig,
    status: Status,
    recorder: Option<audio::Handle>,
    /// Smoothed peak level, 0.0..=1.0.
    meter: f32,
    elapsed: Duration,
    last_transcript: String,
    error: Option<String>,
    devices: Vec<String>,
    /// `devices` prefixed with the default entry, kept owned so the dropdown
    /// can borrow it for a render.
    device_labels: Vec<String>,
    can_type: bool,
}

#[derive(Clone, Debug)]
pub enum Message {
    // Shell plumbing
    Surface(cosmic::surface::Action<Message>),
    PopupClosed(Id),
    ConfigChanged(WhisprConfig),
    Tick,

    // Dictation lifecycle
    Control(ipc::Command),
    Toggle,
    Cancel,
    RecordingReady(Result<String, String>),
    RecordingFinished(Result<audio::Recording, String>),
    Transcribed(Result<String, String>),
    Typed(Result<(), String>),
    DismissError,

    // Settings
    ApiBaseChanged(String),
    ModelChanged(String),
    LanguageChanged(String),
    PromptChanged(String),
    EnvFileChanged(String),
    DeviceSelected(usize),
    TypeDelaySelected(usize),
    TrailingSpaceToggled(bool),
}

impl cosmic::Application for Whispr {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Message>) {
        let (config_handle, config) = WhisprConfig::load();
        let devices = audio::input_devices();
        let can_type = typer::is_available();
        if !can_type {
            tracing::warn!(
                "zwp_virtual_keyboard_manager_v1 is missing; transcripts cannot be typed"
            );
        }

        let applet = Self {
            core,
            popup: None,
            config_handle,
            config,
            status: Status::Idle,
            recorder: None,
            meter: 0.0,
            elapsed: Duration::ZERO,
            last_transcript: String::new(),
            error: None,
            device_labels: std::iter::once("System default".to_string())
                .chain(devices.iter().cloned())
                .collect(),
            devices,
            can_type,
        };

        (applet, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            ipc::listen().map(Message::Control),
            self.core()
                .watch_config::<WhisprConfig>(APP_ID)
                .map(|update| Message::ConfigChanged(update.config)),
        ];

        // Only drive the meter while there is something to show.
        if matches!(self.status, Status::Starting | Status::Recording) {
            subscriptions.push(cosmic::iced::time::every(TICK).map(|_| Message::Tick));
        }

        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Surface(action) => {
                return cosmic::task::message(cosmic::Action::Surface(action));
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::ConfigChanged(config) => self.config = config,
            Message::Tick => {
                if let Some(recorder) = &self.recorder {
                    let level = recorder.level();
                    // Decay smooths the jitter of per-callback peaks.
                    self.meter = self.meter * METER_DECAY + level * (1.0 - METER_DECAY);
                    self.elapsed = recorder.elapsed();
                }
            }

            Message::Control(command) => {
                return self.handle_control(command);
            }
            Message::Toggle => return self.toggle(),
            Message::Cancel => self.reset(None),
            Message::DismissError => self.error = None,

            Message::RecordingReady(Ok(device)) => {
                tracing::info!(%device, "recording");
                if self.status == Status::Starting {
                    self.status = Status::Recording;
                }
            }
            Message::RecordingReady(Err(error)) => self.reset(Some(error)),

            Message::RecordingFinished(Ok(recording)) => {
                tracing::info!(?recording, "captured");
                self.recorder = None;
                self.status = Status::Transcribing;
                return self.transcribe(recording);
            }
            Message::RecordingFinished(Err(error)) => self.reset(Some(error)),

            Message::Transcribed(Ok(transcript)) => {
                self.last_transcript = transcript.clone();
                if transcript.is_empty() {
                    self.reset(Some("nothing was recognized".into()));
                } else {
                    self.status = Status::Typing;
                    return self.type_out(transcript);
                }
            }
            Message::Transcribed(Err(error)) => self.reset(Some(error)),

            Message::Typed(Ok(())) => self.reset(None),
            Message::Typed(Err(error)) => self.reset(Some(error)),

            Message::ApiBaseChanged(value) => return self.edit(|config| config.api_base = value),
            Message::ModelChanged(value) => return self.edit(|config| config.model = value),
            Message::LanguageChanged(value) => return self.edit(|config| config.language = value),
            Message::PromptChanged(value) => return self.edit(|config| config.prompt = value),
            Message::EnvFileChanged(value) => {
                return self.edit(|config| config.env_file = value);
            }
            Message::DeviceSelected(index) => {
                // Index 0 is the "System default" entry.
                let device = match index.checked_sub(1) {
                    Some(index) => self.devices.get(index).cloned().unwrap_or_default(),
                    None => String::new(),
                };
                return self.edit(|config| config.input_device = device);
            }
            Message::TypeDelaySelected(index) => {
                let value = DELAY_PRESETS.get(index).copied().unwrap_or(4);
                return self.edit(|config| config.type_delay_ms = value);
            }
            Message::TrailingSpaceToggled(value) => {
                return self.edit(|config| config.trailing_space = value);
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let have_popup = self.popup;
        let button = self
            .core
            .applet
            .icon_button(self.status.icon())
            .on_press_with_rectangle(move |offset, bounds| match have_popup {
                Some(id) => Message::Surface(destroy_popup(id)),
                None => Message::Surface(app_popup::<Whispr>(
                    |_| Default::default(),
                    move |state: &mut Whispr| {
                        let id = Id::unique();
                        state.popup = Some(id);
                        let mut settings = state.core.applet.get_popup_settings(
                            state.core.main_window_id().unwrap(),
                            id,
                            None,
                            None,
                            None,
                        );
                        settings.positioner.anchor_rect = Rectangle {
                            x: (bounds.x - offset.x) as i32,
                            y: (bounds.y - offset.y) as i32,
                            width: bounds.width as i32,
                            height: bounds.height as i32,
                        };
                        settings
                    },
                    Some(Box::new(|state: &Whispr| {
                        Element::from(state.core.applet.popup_container(state.popup_view()))
                            .map(cosmic::Action::App)
                    })),
                )),
            });

        let tooltip = match self.status {
            Status::Recording => format!("Recording — {}", format_duration(self.elapsed)),
            other => other.label().to_string(),
        };

        Element::from(self.core.applet.applet_tooltip::<Message>(
            button,
            tooltip,
            self.popup.is_some(),
            Message::Surface,
            None,
        ))
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        // Popup content is supplied through the surface action in `view`.
        cosmic::widget::text("").into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl Whispr {
    fn handle_control(&mut self, command: ipc::Command) -> Task<Message> {
        match command {
            ipc::Command::Toggle => self.toggle(),
            ipc::Command::Start => match self.status {
                Status::Idle => self.start(),
                _ => Task::none(),
            },
            ipc::Command::Stop => match self.status {
                Status::Starting | Status::Recording => self.stop(),
                _ => Task::none(),
            },
            ipc::Command::Cancel => {
                self.reset(None);
                Task::none()
            }
        }
    }

    fn toggle(&mut self) -> Task<Message> {
        match self.status {
            Status::Idle => self.start(),
            Status::Starting | Status::Recording => self.stop(),
            // Ignore a toggle that lands mid-transcription rather than
            // queueing a second recording behind it.
            Status::Transcribing | Status::Typing => Task::none(),
        }
    }

    fn start(&mut self) -> Task<Message> {
        self.error = None;
        self.meter = 0.0;
        self.elapsed = Duration::ZERO;
        self.status = Status::Starting;

        let device =
            (!self.config.input_device.is_empty()).then(|| self.config.input_device.clone());
        let (handle, channels) = audio::start(device, Duration::from_secs(self.config.max_seconds));
        self.recorder = Some(handle);

        let ready = channels.ready;
        let finished = channels.finished;
        let mut tasks = vec![
            Task::perform(
                async move {
                    ready
                        .await
                        .unwrap_or_else(|_| Err("capture thread stopped".into()))
                },
                |result| cosmic::Action::App(Message::RecordingReady(result)),
            ),
            Task::perform(
                async move {
                    finished
                        .await
                        .unwrap_or_else(|_| Err("recording was cancelled".into()))
                },
                |result| cosmic::Action::App(Message::RecordingFinished(result)),
            ),
        ];

        // Close the popup so keyboard focus returns to the window the user
        // is dictating into before we start typing.
        if let Some(id) = self.popup.take() {
            tasks.push(cosmic::task::message(cosmic::Action::Surface(
                destroy_popup(id),
            )));
        }

        Task::batch(tasks)
    }

    fn stop(&mut self) -> Task<Message> {
        if let Some(recorder) = &self.recorder {
            recorder.stop();
        }
        self.status = Status::Transcribing;
        self.meter = 0.0;
        Task::none()
    }

    fn transcribe(&mut self, recording: audio::Recording) -> Task<Message> {
        let config = self.config.clone();
        let url = config.transcription_url();
        let model = config.model.clone();
        let language = Some(config.language.clone()).filter(|value| !value.is_empty());
        let prompt = Some(config.prompt.clone()).filter(|value| !value.is_empty());

        Task::perform(
            async move {
                // Resolving the key may run the 1Password CLI, which can
                // block on a biometric prompt; keep it off the UI thread.
                let api_key = tokio::task::spawn_blocking(move || config.resolve_api_key())
                    .await
                    .map_err(|error| format!("api key lookup failed: {error}"))?;

                let request = stt::Request {
                    url,
                    api_key,
                    model,
                    language,
                    prompt,
                };
                stt::transcribe(request, recording.wav)
                    .await
                    .map_err(|error| format!("{error:#}"))
            },
            |result| cosmic::Action::App(Message::Transcribed(result)),
        )
    }

    fn type_out(&mut self, transcript: String) -> Task<Message> {
        let mut text = transcript;
        if self.config.trailing_space {
            text.push(' ');
        }
        let delay = Duration::from_millis(self.config.type_delay_ms);

        Task::perform(
            async move {
                // Wayland I/O and per-key sleeps must not block the UI thread.
                tokio::task::spawn_blocking(move || typer::type_text(&text, delay))
                    .await
                    .map_err(|error| format!("typing task failed: {error}"))?
                    .map_err(|error| format!("{error:#}"))
            },
            |result| cosmic::Action::App(Message::Typed(result)),
        )
    }

    /// Return to idle, tearing down any in-flight recording.
    fn reset(&mut self, error: Option<String>) {
        if let Some(recorder) = self.recorder.take() {
            recorder.cancel();
        }
        if let Some(error) = error {
            tracing::error!(%error, "dictation failed");
            self.error = Some(error);
        }
        self.status = Status::Idle;
        self.meter = 0.0;
        self.elapsed = Duration::ZERO;
    }

    /// Apply a settings edit and persist it.
    fn edit(&mut self, change: impl FnOnce(&mut WhisprConfig)) -> Task<Message> {
        change(&mut self.config);
        if let Some(handle) = &self.config_handle {
            use cosmic_config::CosmicConfigEntry;
            if let Err(error) = self.config.write_entry(handle) {
                tracing::warn!(%error, "cannot save settings");
            }
        }
        Task::none()
    }

    fn popup_view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let mut content = column::with_capacity(6).spacing(spacing.space_xs);
        content = content.push(self.status_row());
        content = content.push(self.action_row());

        if !self.can_type {
            content = content.push(cosmic::applet::padded_control(text::caption(
                "This compositor does not offer the virtual-keyboard protocol, \
                 so transcripts cannot be typed.",
            )));
        }

        if let Some(error) = &self.error {
            content = content.push(cosmic::applet::padded_control(
                row::with_capacity(2)
                    .spacing(spacing.space_xs)
                    .align_y(Alignment::Center)
                    .push(text::caption(error.clone()).width(Length::Fill))
                    .push(button::text("Dismiss").on_press(Message::DismissError)),
            ));
        }

        if !self.last_transcript.is_empty() {
            content = content.push(cosmic::applet::padded_control(text::caption(
                self.last_transcript.clone(),
            )));
        }

        content = content.push(cosmic::applet::padded_control(
            divider::horizontal::default(),
        ));
        content = content.push(self.settings_view());

        content.into()
    }

    fn status_row(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let label = match self.status {
            Status::Recording => format!(
                "{} — {}",
                self.status.label(),
                format_duration(self.elapsed)
            ),
            other => other.label().to_string(),
        };

        let mut children = row::with_capacity(2)
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .push(text::body(label).width(Length::Fill));

        if matches!(self.status, Status::Starting | Status::Recording) {
            children = children.push(
                progress_bar::determinate_linear(self.meter.clamp(0.0, 1.0))
                    .width(Length::Fixed(96.0))
                    .girth(Length::Fixed(4.0)),
            );
        }

        cosmic::applet::padded_control(children).into()
    }

    fn action_row(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let primary = match self.status {
            Status::Idle => button::suggested("Start dictation").on_press(Message::Toggle),
            Status::Starting | Status::Recording => {
                button::suggested("Stop and transcribe").on_press(Message::Toggle)
            }
            // Busy: the button stays visible but inert, so the popup does
            // not change shape underneath the pointer.
            _ => button::suggested("Working…"),
        };

        let mut children = row::with_capacity(2)
            .spacing(spacing.space_xs)
            .push(primary);

        if matches!(self.status, Status::Starting | Status::Recording) || self.status.is_busy() {
            children = children.push(button::standard("Cancel").on_press(Message::Cancel));
        }

        cosmic::applet::padded_control(children).into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let popup = self.popup.unwrap_or(Id::NONE);

        // Index 0 of `device_labels` is "System default"; the rest mirror
        // `self.devices`.
        let selected = if self.config.input_device.is_empty() {
            Some(0)
        } else {
            self.devices
                .iter()
                .position(|name| *name == self.config.input_device)
                .map(|index| index + 1)
        };

        list_column()
            .add(settings::item(
                "Microphone",
                popup_dropdown(
                    &self.device_labels[..],
                    selected,
                    Message::DeviceSelected,
                    popup,
                    Message::Surface,
                    |message| message,
                ),
            ))
            .add(settings::item(
                "Endpoint",
                text_input("https://api.openai.com/v1", &self.config.api_base)
                    .on_input(Message::ApiBaseChanged),
            ))
            .add(settings::item(
                "Model",
                text_input("whisper-1", &self.config.model).on_input(Message::ModelChanged),
            ))
            .add(settings::item(
                "Language",
                text_input("auto", &self.config.language).on_input(Message::LanguageChanged),
            ))
            .add(settings::item(
                "Prompt",
                text_input("names, jargon, punctuation style", &self.config.prompt)
                    .on_input(Message::PromptChanged),
            ))
            .add(settings::item(
                "Env file",
                text_input("~/.config/cosmic-whispr/.env", &self.config.env_file)
                    .on_input(Message::EnvFileChanged),
            ))
            .add(settings::item(
                "Keystroke delay",
                popup_dropdown(
                    &DELAY_LABELS[..],
                    delay_index(self.config.type_delay_ms),
                    Message::TypeDelaySelected,
                    popup,
                    Message::Surface,
                    |message| message,
                ),
            ))
            .add(settings::item(
                "Trailing space",
                toggler(self.config.trailing_space).on_toggle(Message::TrailingSpaceToggled),
            ))
            .into()
    }
}

/// Nearest preset to the stored value, so a hand-edited config still shows
/// something sensible in the dropdown.
fn delay_index(delay_ms: u64) -> Option<usize> {
    DELAY_PRESETS
        .iter()
        .enumerate()
        .min_by_key(|(_, preset)| preset.abs_diff(delay_ms))
        .map(|(index, _)| index)
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_render_as_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(format_duration(Duration::from_secs(9)), "0:09");
        assert_eq!(format_duration(Duration::from_secs(75)), "1:15");
        assert_eq!(format_duration(Duration::from_secs(3600)), "60:00");
    }

    #[test]
    fn delay_presets_map_to_the_nearest_entry() {
        assert_eq!(delay_index(0), Some(0));
        assert_eq!(delay_index(4), Some(2));
        assert_eq!(delay_index(5), Some(2));
        assert_eq!(delay_index(1_000), Some(DELAY_PRESETS.len() - 1));
    }

    #[test]
    fn busy_states_ignore_a_toggle() {
        assert!(!Status::Idle.is_busy());
        assert!(!Status::Recording.is_busy());
        assert!(Status::Transcribing.is_busy());
        assert!(Status::Typing.is_busy());
    }
}
