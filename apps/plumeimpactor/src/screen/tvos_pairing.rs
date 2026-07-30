use iced::widget::{
    button, column, container, pick_list, progress_bar, row, rule, scrollable, text, text_input,
};
use iced::futures::StreamExt;
use iced::{Center, Color, Element, Fill, Task};
use plume_utils::discovery::{
    DeviceDiscovery, DeviceType, DiscoveredDevice, PlatformDiscovery,
    REMOTEPAIRING_MANUAL_PAIRING_SERVICE, REMOTEPAIRING_SERVICE,
};
use plume_utils::Device;
use std::time::Duration;

use crate::appearance;
use crate::defaults::get_data_path;

#[derive(Debug, Clone)]
struct StatusMessage {
    content: String,
    is_error: bool,
}

impl StatusMessage {
    fn success(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: false,
        }
    }
    fn error(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: true,
        }
    }
    fn info(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: false,
        }
    }
    fn color(&self) -> Color {
        if self.is_error {
            Color::from_rgb(0.9, 0.2, 0.2)
        } else {
            Color::from_rgb(0.2, 0.8, 0.4)
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Scan,
    ScanComplete(Result<Vec<DiscoveredDevice>, String>),
    SelectDevice(String),
    PinChanged(String),
    Pair,
    /// The pairing session reached the point where the Apple TV displays its code. `false`
    /// means the session ended without ever asking, so there is nothing to prompt for.
    PinRequested(bool),
    SubmitPin,
    PairComplete(Result<(), String>),
    SelectIpa,
    IpaSelected(Option<std::path::PathBuf>),
    InstallApp,
    InstallComplete(Result<(), String>),
}

/// Info about a successfully paired Apple TV, retained for installation.
#[derive(Debug, Clone)]
struct PairedDevice {
    name: String,
    hostname: String,
    ip: std::net::IpAddr,
    /// `_remotepairing-manual-pairing._tcp.local.` port, if still known.
    pairing_port: Option<u16>,
    /// `_remotepairing._tcp.local.` port, if known - preferred for reconnecting.
    reconnect_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct TvOsPairingScreen {
    discovered: Vec<DiscoveredDevice>,
    selected_name: Option<String>,
    pin: String,
    scanning: bool,
    pairing: bool,
    /// True while the pairing session is held open waiting for the code the Apple TV is
    /// currently showing. The PIN entry is visible only in this state.
    awaiting_pin: bool,
    /// Hands the typed code to the pairing thread's PIN provider. Present only for the
    /// duration of a pairing attempt.
    pin_sender: Option<std::sync::mpsc::SyncSender<String>>,
    status: Option<StatusMessage>,
    paired_device: Option<PairedDevice>,
    ipa_path: Option<std::path::PathBuf>,
    installing: bool,
}

impl TvOsPairingScreen {
    pub fn new() -> Self {
        Self {
            discovered: Vec::new(),
            selected_name: None,
            pin: String::new(),
            scanning: false,
            pairing: false,
            awaiting_pin: false,
            pin_sender: None,
            status: None,
            paired_device: None,
            ipa_path: None,
            installing: false,
        }
    }

    /// The manual-pairing service entry for the selected device, if the Apple TV is currently
    /// advertising it (i.e. actively showing a pairing PIN). Required to start a new pairing.
    fn manual_pairing_entry(&self) -> Option<&DiscoveredDevice> {
        let name = self.selected_name.as_deref()?;
        // Names are compared case-insensitively: they derive from the advertised host name, and
        // DNS names are case-insensitive, so a device that varied its own capitalization between
        // service types would otherwise fail to correlate with itself.
        self.discovered.iter().find(|d| {
            d.name.eq_ignore_ascii_case(name)
                && d.service_type == REMOTEPAIRING_MANUAL_PAIRING_SERVICE
        })
    }

    /// The standard (already-paired) service entry for the selected device, if known.
    fn reconnect_entry(&self) -> Option<&DiscoveredDevice> {
        let name = self.selected_name.as_deref()?;
        self.discovered
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name) && d.service_type == REMOTEPAIRING_SERVICE)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Scan => {
                self.scanning = true;
                self.status = Some(StatusMessage::info("Scanning for Apple TVs..."));
                self.discovered.clear();
                self.selected_name = None;
                self.pin.clear();

                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    let result: Result<Vec<DiscoveredDevice>, String> = rt.block_on(async {
                        PlatformDiscovery::new()
                            .discover(Duration::from_secs(5))
                            .await
                            .map_err(|e| format!("Scan failed: {e}"))
                    });
                    let _ = tx.send(result);
                });

                Task::perform(
                    async move {
                        std::thread::spawn(move || {
                            rx.recv().unwrap_or_else(|_| Err("Scan error".to_string()))
                        })
                        .join()
                        .unwrap()
                    },
                    Message::ScanComplete,
                )
            }

            Message::ScanComplete(result) => {
                self.scanning = false;
                match result {
                    Ok(devices) => {
                        // A device's service-type entries do not all carry model information:
                        // the manual-pairing advertisement identifies itself as an Apple TV,
                        // but the established-pairing one advertises no model at all. Keeping
                        // only entries that are themselves typed AppleTV would discard the
                        // reconnect entry and with it the port needed to reach a paired device,
                        // so every entry belonging to a name that identified as an Apple TV
                        // under any service type is kept.
                        let tv_names: std::collections::HashSet<String> = devices
                            .iter()
                            .filter(|d| d.device_type == DeviceType::AppleTV)
                            .map(|d| d.name.to_ascii_lowercase())
                            .collect();
                        self.discovered = devices
                            .into_iter()
                            .filter(|d| tv_names.contains(&d.name.to_ascii_lowercase()))
                            .collect();
                        if tv_names.is_empty() {
                            self.status =
                                Some(StatusMessage::info("No Apple TVs found on this network."));
                        } else {
                            self.status = Some(StatusMessage::info(format!(
                                "Found {} Apple TV(s). Select one to pair.",
                                tv_names.len()
                            )));
                        }
                    }
                    Err(e) => {
                        self.status = Some(StatusMessage::error(e));
                    }
                }
                Task::none()
            }

            Message::SelectDevice(name) => {
                self.selected_name = Some(name);
                self.pin.clear();
                self.status = None;
                Task::none()
            }

            Message::PinChanged(s) => {
                self.pin = s.chars().filter(|c| c.is_ascii_digit()).take(6).collect();
                Task::none()
            }

            Message::Pair => {
                let Some(dev) = self.manual_pairing_entry() else {
                    self.status = Some(StatusMessage::error(
                        "This Apple TV isn't showing a pairing PIN. On the Apple TV, open \
                         Settings > Remotes and Devices > Remote App and Devices, wait for \
                         \"Waiting to Pair...\", then Scan again.",
                    ));
                    return Task::none();
                };

                let ip_str = match &dev.ip_address {
                    Some(s) => s.clone(),
                    None => {
                        self.status =
                            Some(StatusMessage::error("Selected device has no IP address."));
                        return Task::none();
                    }
                };
                let pairing_port = match dev.port {
                    Some(p) => p,
                    None => {
                        self.status = Some(StatusMessage::error("Selected device has no port."));
                        return Task::none();
                    }
                };
                let reconnect_port = self.reconnect_entry().and_then(|d| d.port);

                let name = dev.name.clone();
                let hostname = dev
                    .udid
                    .clone()
                    .unwrap_or_else(|| ip_str.replace('.', "-"));
                let cache_dir = get_data_path();

                // The Apple TV shows no code until it has accepted the pair-setup request, so
                // any digits left in the field predate this attempt and must not be reused.
                self.pin.clear();
                self.awaiting_pin = false;
                self.pairing = true;
                self.status = Some(StatusMessage::info("Connecting to Apple TV..."));

                // pin_req carries the moment the device asks for its code from the pairing
                // thread to the UI; pin_resp carries the typed code back the other way.
                //
                // Both UI-facing channels are async and must stay that way: these two tasks run
                // in one batch, and a task that blocks its executor thread waiting for a result
                // prevents every other task in the batch from being polled. Blocking here would
                // leave the PIN prompt undelivered while the pairing thread waits for the code
                // that only that prompt can produce.
                let (pin_req_tx, mut pin_req_rx) = iced::futures::channel::mpsc::unbounded::<()>();
                let (result_tx, mut result_rx) =
                    iced::futures::channel::mpsc::unbounded::<Result<(), String>>();
                let (pin_resp_tx, pin_resp_rx) = std::sync::mpsc::sync_channel::<String>(1);
                self.pin_sender = Some(pin_resp_tx);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    // The provider is `Fn` and so cannot move the receiver out of itself; the
                    // Arc<Mutex<..>> lets every invocation borrow the one receiver.
                    let pin_resp_rx = std::sync::Arc::new(std::sync::Mutex::new(pin_resp_rx));
                    let result = rt.block_on(async move {
                        let ip: std::net::IpAddr = ip_str
                            .parse()
                            .map_err(|e| format!("Invalid IP: {e}"))?;
                        let device = Device::new_tvos(
                            name,
                            hostname,
                            ip,
                            Some(pairing_port),
                            reconnect_port,
                        );
                        device
                            .pair_tvos(
                                move || {
                                    let pin_req_tx = pin_req_tx.clone();
                                    let pin_resp_rx = pin_resp_rx.clone();
                                    async move {
                                        let _ = pin_req_tx.unbounded_send(());
                                        // Blocking here is load-bearing and must stay: the
                                        // Apple TV displays its code only while the pair-setup
                                        // session is open, so the session has to be held for as
                                        // long as the user takes to read and type it. This runs
                                        // on this thread's own runtime with nothing else on it.
                                        // An empty code on timeout or on a closed channel fails
                                        // the handshake cleanly instead of waiting forever.
                                        let Ok(rx) = pin_resp_rx.lock() else {
                                            return String::new();
                                        };
                                        rx.recv_timeout(Duration::from_secs(180))
                                            .unwrap_or_default()
                                    }
                                },
                                cache_dir,
                            )
                            .await
                            .map(|_| ())
                            .map_err(|e| format!("{e}"))
                    });
                    let _ = result_tx.unbounded_send(result);
                });

                Task::batch([
                    Task::perform(
                        async move {
                            result_rx
                                .next()
                                .await
                                .unwrap_or_else(|| Err("Pairing thread error".to_string()))
                        },
                        Message::PairComplete,
                    ),
                    // Resolves to false when the pairing thread drops its sender without ever
                    // asking - the attempt ended before the device got as far as showing a code.
                    Task::perform(
                        async move { pin_req_rx.next().await.is_some() },
                        Message::PinRequested,
                    ),
                ])
            }

            Message::PinRequested(requested) => {
                if requested {
                    self.awaiting_pin = true;
                    self.status = Some(StatusMessage::info(
                        "Enter the code shown on your Apple TV",
                    ));
                }
                Task::none()
            }

            Message::SubmitPin => {
                if self.pin.len() != 6 {
                    return Task::none();
                }
                if let Some(tx) = self.pin_sender.as_ref() {
                    // try_send so a submit can never block the UI thread; the pairing thread is
                    // already parked on this channel, so the single slot is free.
                    let _ = tx.try_send(self.pin.clone());
                }
                self.awaiting_pin = false;
                self.status = Some(StatusMessage::info("Verifying..."));
                Task::none()
            }

            Message::PairComplete(result) => {
                self.pairing = false;
                self.awaiting_pin = false;
                self.pin_sender = None;
                match result {
                    Ok(_) => {
                        // Reconstruct paired device info from the discovered entries. Prefer
                        // the reconnect service's ip/udid if present (same device, same udid,
                        // but this keeps things consistent if only one entry resolved).
                        let manual = self.manual_pairing_entry().cloned();
                        let reconnect = self.reconnect_entry().cloned();
                        if let Some(dev) = reconnect.as_ref().or(manual.as_ref()) {
                            if let Some(ip_str) = &dev.ip_address {
                                if let Ok(ip) = ip_str.parse() {
                                    self.paired_device = Some(PairedDevice {
                                        name: dev.name.clone(),
                                        hostname: dev
                                            .udid
                                            .clone()
                                            .unwrap_or_else(|| ip_str.replace('.', "-")),
                                        ip,
                                        pairing_port: manual.as_ref().and_then(|d| d.port),
                                        reconnect_port: reconnect.as_ref().and_then(|d| d.port),
                                    });
                                }
                            }
                        }
                        self.status = Some(StatusMessage::success(
                            "Paired successfully! Select an IPA to install.",
                        ));
                        self.pin.clear();
                    }
                    Err(e) => {
                        self.status = Some(StatusMessage::error(e));
                    }
                }
                Task::none()
            }

            Message::SelectIpa => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("iOS/tvOS App", &["ipa", "tipa"])
                        .set_title("Select IPA to install on Apple TV")
                        .pick_file()
                        .await
                        .map(|file| file.path().to_path_buf())
                },
                Message::IpaSelected,
            ),

            Message::IpaSelected(path) => {
                self.ipa_path = path;
                Task::none()
            }

            Message::InstallApp => {
                let Some(ref paired) = self.paired_device else {
                    return Task::none();
                };
                let Some(ref ipa_path) = self.ipa_path else {
                    return Task::none();
                };

                let name = paired.name.clone();
                let hostname = paired.hostname.clone();
                let ip = paired.ip;
                let pairing_port = paired.pairing_port;
                let reconnect_port = paired.reconnect_port;
                let ipa = ipa_path.clone();
                let pin = self.pin.clone();
                let cache_dir = get_data_path();

                self.installing = true;
                self.status = Some(StatusMessage::info("Installing..."));

                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    let result = rt.block_on(async move {
                        let device =
                            Device::new_tvos(name, hostname, ip, pairing_port, reconnect_port);
                        device
                            .install_app_tvos(&ipa, pin, cache_dir)
                            .await
                            .map_err(|e| format!("{e}"))
                    });
                    let _ = tx.send(result);
                });

                Task::perform(
                    async move {
                        std::thread::spawn(move || {
                            rx.recv()
                                .unwrap_or_else(|_| Err("Install thread error".to_string()))
                        })
                        .join()
                        .unwrap()
                    },
                    Message::InstallComplete,
                )
            }

            Message::InstallComplete(result) => {
                self.installing = false;
                match result {
                    Ok(()) => {
                        self.status = Some(StatusMessage::success(
                            "App installed successfully on Apple TV!",
                        ));
                        self.ipa_path = None;
                    }
                    Err(e) => {
                        self.status = Some(StatusMessage::error(e));
                    }
                }
                Task::none()
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let mut content = column![].spacing(appearance::THEME_PADDING);

        // Scan button
        let scan_label = if self.scanning {
            "Scanning..."
        } else {
            "Scan for Apple TVs"
        };
        let scan_btn = button(text(scan_label).align_x(Center))
            .on_press_maybe(if self.scanning {
                None
            } else {
                Some(Message::Scan)
            })
            .style(appearance::s_button)
            .width(Fill);
        content = content.push(scan_btn);

        // Status message
        if let Some(ref s) = self.status {
            content = content.push(text(&s.content).size(13).color(s.color()));
        }

        // Device list (pick_list)
        if !self.discovered.is_empty() {
            content = content
                .push(container(rule::horizontal(1)).padding([appearance::THEME_PADDING, 0.0]));

            // A device can appear twice (once per RPPairing service type it advertises) -
            // dedupe by name for the picker; the specific service entries are looked up by
            // name when needed.
            let mut device_names: Vec<String> =
                self.discovered.iter().map(|d| d.name.clone()).collect();
            device_names.sort();
            device_names.dedup();
            let selected = self.selected_name.clone();

            content = content.push(
                pick_list(device_names, selected, Message::SelectDevice)
                    .placeholder("Select an Apple TV")
                    .width(Fill),
            );
        }

        // Pair button (a device is selected, pairing has not succeeded yet, and the device is
        // not currently asking for its code). No PIN is collected here: the Apple TV does not
        // display one until the pair-setup request it has not yet received is accepted.
        if self.selected_name.is_some() && self.paired_device.is_none() && !self.awaiting_pin {
            content = content
                .push(container(rule::horizontal(1)).padding([appearance::THEME_PADDING, 0.0]));

            let pair_label = if self.pairing { "Pairing..." } else { "Pair" };
            let pair_btn = button(text(pair_label).align_x(Center))
                .on_press_maybe(if self.pairing {
                    None
                } else {
                    Some(Message::Pair)
                })
                .style(appearance::s_button)
                .width(Fill);

            content = content.push(pair_btn);
        }

        // PIN entry (only while the device is showing its code and the session is held open)
        if self.awaiting_pin {
            content = content
                .push(container(rule::horizontal(1)).padding([appearance::THEME_PADDING, 0.0]));

            content =
                content.push(text("Enter the 6-digit code shown on your Apple TV:").size(13));

            let pin_input = text_input("123456", &self.pin)
                .on_input(Message::PinChanged)
                .width(iced::Length::Fixed(120.0));

            let submit_btn = button(text("Submit").align_x(Center))
                .on_press_maybe(if self.pin.len() == 6 {
                    Some(Message::SubmitPin)
                } else {
                    None
                })
                .style(appearance::s_button);

            content = content.push(
                row![pin_input, submit_btn]
                    .spacing(appearance::THEME_PADDING)
                    .align_y(Center),
            );
        }

        // Install section (shown after successful pairing)
        if self.paired_device.is_some() {
            content = content
                .push(container(rule::horizontal(1)).padding([appearance::THEME_PADDING, 0.0]));

            // IPA file picker
            let select_label = if let Some(ref path) = self.ipa_path {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Selected".to_string());
                format!("Selected: {name}")
            } else {
                "Select IPA File".to_string()
            };

            let select_btn = button(text(select_label).align_x(Center))
                .on_press_maybe(if self.installing {
                    None
                } else {
                    Some(Message::SelectIpa)
                })
                .style(appearance::s_button)
                .width(Fill);
            content = content.push(select_btn);

            // Install button
            if self.ipa_path.is_some() {
                let install_label = if self.installing {
                    "Installing..."
                } else {
                    "Install on Apple TV"
                };
                let install_btn = button(text(install_label).align_x(Center))
                    .on_press_maybe(if self.installing || self.ipa_path.is_none() {
                        None
                    } else {
                        Some(Message::InstallApp)
                    })
                    .style(appearance::p_button)
                    .width(Fill);
                content = content.push(install_btn);
            }

            if self.installing {
                content = content.push(progress_bar(0.0..=1.0, 0.5));
            }
        }

        container(scrollable(content)).into()
    }
}
