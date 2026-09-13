// SPDX-License-Identifier: MPL-2.0

//! The panel applet: a state machine over record -> transcribe -> type,
//! plus a popup for status and settings.

use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::iced::core::window;
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Length, Limits, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::widget::dropdown::popup_dropdown;
use cosmic::widget::text_input::secure_input;
use cosmic::widget::{
    button, column, divider, list_column, mouse_area, progress_bar, row, settings, text,
    text_input, toggler,
};
use cosmic::{Element, cosmic_config};
use zeroize::{Zeroize, Zeroizing};

use crate::config::{APP_ID, WhisprConfig};
use crate::{audio, cleanup, clipboard, ipc, secret, stt, typer};

/// How often the level meter and elapsed timer refresh while recording.
const TICK: Duration = Duration::from_millis(100);
/// Level meter smoothing: how much of the previous reading to keep.
const METER_DECAY: f32 = 0.6;
/// Popup width. Wider than libcosmic's 360 px default, because the fields
/// here hold endpoints, API keys, and `op://` references rather than the
/// short values a panel popup usually shows.
const POPUP_WIDTH: f32 = 480.0;
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
    /// Handing the transcript over, by whichever route was chosen.
    Delivering,
}

impl Status {
    fn icon(self) -> &'static str {
        match self {
            Self::Idle => "audio-input-microphone-symbolic",
            Self::Starting | Self::Recording => "media-record-symbolic",
            Self::Transcribing | Self::Delivering => "content-loading-symbolic",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Starting => "Opening microphone…",
            Self::Recording => "Recording",
            Self::Transcribing => "Transcribing…",
            Self::Delivering => "Delivering…",
        }
    }

    fn is_busy(self) -> bool {
        matches!(self, Self::Transcribing | Self::Delivering)
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
    /// What the user has typed into the API key box. Deliberately transient:
    /// it is wiped the moment the key reaches the keyring, and is never
    /// written to the config.
    ///
    /// Each keystroke hands us a fresh `String` from the text widget, and the
    /// one it replaces is dropped inside the widget where we cannot reach it.
    /// Wiping what we hold is therefore a reduction of the exposure, not an
    /// elimination of it — pasting in one go leaves far less behind than
    /// typing a key out character by character.
    key_input: Zeroizing<String>,
    /// Whether the key box masks what it holds.
    key_hidden: bool,
    /// Cached answer to "what does the keyring hold". Refreshed by a task,
    /// because answering it means a D-Bus round trip and `view` cannot block.
    key_status: secret::Status,
    /// A key operation is in flight; the buttons stay inert until it lands.
    key_busy: bool,
    /// Where the transcript in flight is headed. Chosen when the recording
    /// starts, so the shortcut you press to begin decides.
    delivery: ipc::Delivery,
    /// Transient confirmation for a delivery that leaves nothing on screen —
    /// a clipboard copy types nothing, so it needs to say so somewhere.
    notice: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Message {
    // Shell plumbing
    Surface(cosmic::surface::Action<Message>),
    TogglePopup,
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
    Delivered(Result<(), String>),
    Copied(Result<(), String>),
    DismissError,

    // Settings
    ApiBaseChanged(String),
    ModelChanged(String),
    LanguageChanged(String),
    PromptChanged(String),
    DeviceSelected(usize),
    TypeDelaySelected(usize),
    TrailingSpaceToggled(bool),
    CleanupToggled(bool),
    CleanupModelChanged(String),

    // API key
    KeyInputChanged(String),
    KeyVisibilityToggled,
    SaveKey,
    OpReferenceChanged(String),
    ImportKey,
    ClearKey,
    KeyStored(Result<(), String>),
    KeyStatusLoaded(secret::Status),
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
            key_input: Zeroizing::new(String::new()),
            key_hidden: true,
            key_status: secret::Status::Empty,
            key_busy: false,
            delivery: ipc::Delivery::default(),
            notice: None,
        };

        (applet, refresh_key_status())
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
            Message::TogglePopup => return self.toggle_popup(),
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    // The notice is a confirmation for whoever is looking at
                    // the popup, so it belongs to this visit and not the next.
                    self.notice = None;
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
            Message::DismissError => {
                self.error = None;
                self.notice = None;
            }

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
                    self.status = Status::Delivering;
                    return self.deliver(transcript);
                }
            }
            Message::Transcribed(Err(error)) => self.reset(Some(error)),

            Message::Delivered(Ok(())) => self.reset(None),
            Message::Delivered(Err(error)) => self.reset(Some(error)),

            // Nothing was typed, so the notice is the only sign the dictation
            // worked — and it is only set once the copy actually succeeded.
            Message::Copied(Ok(())) => {
                self.notice = Some("Copied to clipboard".to_string());
                self.reset(None);
            }
            Message::Copied(Err(error)) => self.reset(Some(error)),

            Message::ApiBaseChanged(value) => return self.edit(|config| config.api_base = value),
            Message::ModelChanged(value) => return self.edit(|config| config.model = value),
            Message::LanguageChanged(value) => return self.edit(|config| config.language = value),
            Message::PromptChanged(value) => return self.edit(|config| config.prompt = value),

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
            Message::CleanupToggled(value) => return self.edit(|config| config.cleanup = value),
            Message::CleanupModelChanged(value) => {
                return self.edit(|config| config.cleanup_model = value);
            }

            Message::KeyInputChanged(value) => {
                self.key_input.zeroize();
                self.key_input = Zeroizing::new(value);
            }
            Message::KeyVisibilityToggled => self.key_hidden = !self.key_hidden,
            Message::OpReferenceChanged(value) => {
                return self.edit(|config| config.op_reference = value);
            }
            Message::SaveKey => {
                // Copy rather than take: storing fails on a locked or absent
                // keyring, and emptying the box first would send the user
                // back to 1Password to copy the key again.
                let key = Zeroizing::new(self.key_input.trim().to_string());
                if key.is_empty() {
                    return Task::none();
                }
                self.key_busy = true;
                return store_key(move || secret::store(&key));
            }
            Message::ImportKey => {
                let reference = self.config.op_reference.clone();
                if reference.trim().is_empty() {
                    return Task::none();
                }
                self.key_busy = true;
                return store_key(move || secret::import_reference(&reference));
            }
            Message::ClearKey => {
                self.key_input.zeroize();
                self.key_busy = true;
                return store_key(secret::clear);
            }
            Message::KeyStored(result) => {
                self.key_busy = false;
                match result {
                    // Safely stored, so the copy in the box can go; the
                    // status line takes over as the record of what happened.
                    Ok(()) => self.key_input.zeroize(),
                    Err(error) => {
                        tracing::error!(%error, "api key");
                        self.error = Some(error);
                    }
                }
                return refresh_key_status();
            }
            Message::KeyStatusLoaded(status) => self.key_status = status,
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        // Left click dictates, because that is the action taken every time;
        // right click opens the popup, which is for the rare visit.
        let button = self
            .core
            .applet
            .icon_button(self.status.icon())
            .on_press(Message::Toggle);

        let tooltip = match self.status {
            Status::Idle => "Click to dictate, right-click for settings".to_string(),
            Status::Recording => format!("Recording — {}", format_duration(self.elapsed)),
            _ => self.status_label().to_string(),
        };

        let tooltip = self.core.applet.applet_tooltip::<Message>(
            button,
            tooltip,
            self.popup.is_some(),
            Message::Surface,
            None,
        );

        mouse_area(tooltip)
            .on_right_press(Message::TogglePopup)
            .into()
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
    /// Open the popup, or close it if it is already open.
    ///
    /// The default anchor rectangle is the whole applet surface, which is
    /// exactly this applet's single button, so unlike the libcosmic example
    /// there is no rectangle to thread through from the press.
    fn toggle_popup(&mut self) -> Task<Message> {
        let action = match self.popup.take() {
            Some(id) => {
                // `PopupClosed` cannot do this: `popup` is already `None` by
                // the time it arrives, so its guard never matches.
                self.notice = None;
                destroy_popup(id)
            }
            None => app_popup::<Whispr>(
                |_| Default::default(),
                |state: &mut Whispr| {
                    let id = Id::unique();
                    state.popup = Some(id);
                    let mut settings = state.core.applet.get_popup_settings(
                        state.core.main_window_id().unwrap(),
                        id,
                        None,
                        None,
                        None,
                    );
                    // The default is a fixed 360 px, which an API key or an
                    // op:// reference has no hope of fitting into.
                    //
                    // Built from scratch rather than adjusted: the `Limits`
                    // setters only ever narrow — `max_width` takes a `min`
                    // with the current value and `min_width` a `max` — so
                    // widening an existing 360/360 pair is impossible in
                    // either order, and doing it that way silently left the
                    // popup at its default.
                    settings.positioner.size_limits = Limits::NONE
                        .min_height(1.0)
                        .max_height(1080.0)
                        .min_width(POPUP_WIDTH)
                        .max_width(POPUP_WIDTH);
                    settings
                },
                Some(Box::new(|state: &Whispr| {
                    Element::from(state.core.applet.popup_container(state.popup_view()))
                        .map(cosmic::Action::App)
                })),
            ),
        };

        cosmic::task::message(cosmic::Action::Surface(action))
    }

    fn handle_control(&mut self, command: ipc::Command) -> Task<Message> {
        match command {
            ipc::Command::Toggle(delivery) => self.toggle_with(delivery),
            ipc::Command::Start(delivery) => match self.status {
                Status::Idle => {
                    self.delivery = delivery.unwrap_or_default();
                    self.start()
                }
                _ => Task::none(),
            },
            ipc::Command::Stop(delivery) => match self.status {
                Status::Starting | Status::Recording => {
                    // Only an explicit mode overrides what the start chose,
                    // so stopping with the other shortcut by accident does
                    // not redirect the transcript.
                    if let Some(delivery) = delivery {
                        self.delivery = delivery;
                    }
                    self.stop()
                }
                _ => Task::none(),
            },
            ipc::Command::Cancel => {
                self.reset(None);
                Task::none()
            }
        }
    }

    /// Toggle from the panel button or the popup.
    ///
    /// It says "type" only when it is the press that starts. Stopping a
    /// recording that was begun with the clipboard shortcut must not
    /// redirect it into the window underneath — which, given why someone
    /// chose the clipboard, could be a password field.
    fn toggle(&mut self) -> Task<Message> {
        let delivery = matches!(self.status, Status::Idle).then_some(ipc::Delivery::Type);
        self.toggle_with(delivery)
    }

    fn toggle_with(&mut self, delivery: Option<ipc::Delivery>) -> Task<Message> {
        match self.status {
            Status::Idle => {
                self.delivery = delivery.unwrap_or_default();
                self.start()
            }
            Status::Starting | Status::Recording => {
                if let Some(delivery) = delivery {
                    self.delivery = delivery;
                }
                self.stop()
            }
            // Ignore a toggle that lands mid-transcription rather than
            // queueing a second recording behind it.
            Status::Transcribing | Status::Delivering => Task::none(),
        }
    }

    fn start(&mut self) -> Task<Message> {
        self.error = None;
        self.notice = None;
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
        let cleanup_enabled = config.cleanup;
        let cleanup_url = config.chat_url();
        let cleanup_model = config.cleanup_model.clone();

        Task::perform(
            async move {
                // Reading the keyring is a blocking D-Bus round trip, and a
                // locked keyring can raise an unlock prompt; keep it off the
                // UI thread.
                let api_key = tokio::task::spawn_blocking(move || config.resolve_api_key())
                    .await
                    .map_err(|error| format!("api key lookup failed: {error}"))?;

                let request = stt::Request {
                    url,
                    api_key: api_key.clone(),
                    model,
                    language,
                    prompt,
                };
                let transcript = stt::transcribe(request, recording.wav)
                    .await
                    .map_err(|error| format!("{error:#}"))?;

                if !cleanup_enabled || transcript.is_empty() {
                    return Ok(transcript);
                }

                // Never fails: a cleanup problem falls back to the raw
                // transcript rather than losing what was dictated.
                Ok(cleanup::clean_or_keep(
                    cleanup::Request {
                        url: cleanup_url,
                        api_key,
                        model: cleanup_model,
                    },
                    transcript,
                )
                .await)
            },
            |result| cosmic::Action::App(Message::Transcribed(result)),
        )
    }

    /// Send the transcript wherever this recording was headed.
    fn deliver(&mut self, transcript: String) -> Task<Message> {
        if self.delivery == ipc::Delivery::Clipboard {
            // No trailing space here: that setting exists so consecutive
            // dictations do not run together as they are typed, and a
            // clipboard copy has no such neighbour — it would just be a
            // stray space on the end of every paste.
            return Task::perform(
                async move {
                    // Spawning a process, so not on the UI thread.
                    tokio::task::spawn_blocking(move || clipboard::copy(&transcript))
                        .await
                        .map_err(|error| format!("clipboard task failed: {error}"))?
                        .map_err(|error| format!("{error:#}"))
                },
                |result| cosmic::Action::App(Message::Copied(result)),
            );
        }

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
            |result| cosmic::Action::App(Message::Delivered(result)),
        )
    }

    /// Return to idle, tearing down any in-flight recording.
    fn reset(&mut self, error: Option<String>) {
        if let Some(recorder) = self.recorder.take() {
            recorder.cancel();
        }
        // A cancelled or failed dictation must not leave "Copied to
        // clipboard" sitting under it from the one before.
        if error.is_some() {
            self.notice = None;
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

        if let Some(notice) = &self.notice {
            content = content.push(cosmic::applet::padded_control(
                text::caption(notice.clone()).width(Length::Fill),
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
        content = content.push(self.api_key_view());
        content = content.push(cosmic::applet::padded_control(
            divider::horizontal::default(),
        ));
        content = content.push(self.settings_view());

        content.into()
    }

    /// `Status::label`, corrected for a delivery that does no typing.
    fn status_label(&self) -> &'static str {
        match (self.status, self.delivery) {
            (Status::Delivering, ipc::Delivery::Clipboard) => "Copying…",
            (Status::Delivering, ipc::Delivery::Type) => "Typing…",
            (status, _) => status.label(),
        }
    }

    fn status_row(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let label = match self.status {
            Status::Recording => format!(
                "{} — {}",
                self.status.label(),
                format_duration(self.elapsed)
            ),
            _ => self.status_label().to_string(),
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

    /// The API key section: paste a key, or fetch one from 1Password. Both
    /// paths end in the keyring, which is the only place the key is read
    /// from later.
    fn api_key_view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let ready = !self.key_busy;
        let has_key = matches!(self.key_status, secret::Status::Stored { .. });

        let save = (ready && !self.key_input.trim().is_empty()).then_some(Message::SaveKey);
        let import =
            (ready && !self.config.op_reference.trim().is_empty()).then_some(Message::ImportKey);

        let key_row = row::with_capacity(2)
            .spacing(spacing.space_xxs)
            .align_y(Alignment::Center)
            .push(
                secure_input(
                    "paste your API key",
                    self.key_input.as_str(),
                    Some(Message::KeyVisibilityToggled),
                    self.key_hidden,
                )
                .width(Length::Fill)
                .on_input(Message::KeyInputChanged)
                .on_submit(|_| Message::SaveKey),
            )
            .push(button::standard("Save").on_press_maybe(save));

        let op_row = row::with_capacity(2)
            .spacing(spacing.space_xxs)
            .align_y(Alignment::Center)
            .push(
                text_input("op://Private/OpenAI/credential", &self.config.op_reference)
                    .width(Length::Fill)
                    .on_input(Message::OpReferenceChanged)
                    .on_submit(|_| Message::ImportKey),
            )
            .push(button::standard("Fetch").on_press_maybe(import));

        let mut status = row::with_capacity(2)
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .push(text::caption(self.key_status.describe()).width(Length::Fill));
        if has_key {
            status = status.push(
                button::text("Remove").on_press_maybe(ready.then_some(Message::ClearKey)),
            );
        }

        list_column()
            .add(stacked("API key", key_row))
            .add(status)
            .add(stacked("Import from 1Password", op_row))
            .into()
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
            .add(stacked(
                "Endpoint",
                text_input("https://api.openai.com/v1", &self.config.api_base)
                    .width(Length::Fill)
                    .on_input(Message::ApiBaseChanged),
            ))
            .add(stacked(
                "Model",
                text_input("whisper-1", &self.config.model)
                    .width(Length::Fill)
                    .on_input(Message::ModelChanged),
            ))
            .add(stacked(
                "Language",
                text_input("auto", &self.config.language)
                    .width(Length::Fill)
                    .on_input(Message::LanguageChanged),
            ))
            .add(stacked(
                "Prompt",
                text_input("names, jargon, punctuation style", &self.config.prompt)
                    .width(Length::Fill)
                    .on_input(Message::PromptChanged),
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
                "Clean up filler words",
                toggler(self.config.cleanup).on_toggle(Message::CleanupToggled),
            ))
            .add(stacked(
                "Cleanup model",
                text_input("gpt-5.4-nano", &self.config.cleanup_model)
                    .width(Length::Fill)
                    .on_input(Message::CleanupModelChanged),
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

/// A settings row with its label on its own line above the control.
///
/// `settings::item` lays the label and the control out side by side with a
/// spacer between them, which leaves a text field a sliver of the popup.
/// Long values need the width more than the label needs company.
fn stacked<'a>(label: &'a str, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column::with_capacity(2)
        .spacing(cosmic::theme::spacing().space_xxxs)
        .width(Length::Fill)
        .push(text::body(label))
        .push(control.into())
        .into()
}

/// Ask the keyring what it holds, off the UI thread.
fn refresh_key_status() -> Task<Message> {
    Task::perform(
        async {
            tokio::task::spawn_blocking(secret::status)
                .await
                .unwrap_or_else(|error| {
                    secret::Status::Unavailable(format!("status task failed: {error}"))
                })
        },
        |status| cosmic::Action::App(Message::KeyStatusLoaded(status)),
    )
}

/// Run a keyring write off the UI thread.
///
/// The closure owns the secret, so it never has to travel back through a
/// message: the reply is only whether it worked.
fn store_key(
    operation: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(operation)
                .await
                .map_err(|error| format!("keyring task failed: {error}"))?
                .map_err(|error| format!("{error:#}"))
        },
        |result| cosmic::Action::App(Message::KeyStored(result)),
    )
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
        assert!(Status::Delivering.is_busy());
    }
}
