use iced::widget::{
    button, column, container, pick_list, progress_bar, row, rule, scrollable, text, text_input,
};
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
                        self.discovered = devices
                            .into_iter()
                            .filter(|d| d.device_type == DeviceType::AppleTV)
                            .collect();
                        if self.discovered.is_empty() {
                            self.status =
                                Some(StatusMessage::info("No Apple TVs found on this network."));
                        } else {
                            self.status = Some(StatusMessage::info(format!(
                                "Found {} Apple TV(s). Select one to pair.",
                                self.discovered.len()
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
                if self.pin.len() != 6 {
                    return Task::none();
                }

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
                let pin = self.pin.clone();
                let cache_dir = get_data_path();

                self.pairing = true;
                self.status = Some(StatusMessage::info("Pairing..."));

                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
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
                            .pair_tvos(pin, cache_dir)
                            .await
                            .map(|_| ())
                            .map_err(|e| format!("{e}"))
                    });
                    let _ = tx.send(result);
                });

                Task::perform(
                    async move {
                        std::thread::spawn(move || {
                            rx.recv()
                                .unwrap_or_else(|_| Err("Pairing thread error".to_string()))
                        })
                        .join()
                        .unwrap()
                    },
                    Message::PairComplete,
                )
            }

            Message::PairComplete(result) => {
                self.pairing = false;
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

        // PIN entry (only when a device is selected and not yet paired)
        if self.selected_name.is_some() && self.paired_device.is_none() {
            content = content
                .push(container(rule::horizontal(1)).padding([appearance::THEME_PADDING, 0.0]));

            content =
                content.push(text("Enter the 6-digit PIN shown on your Apple TV:").size(13));

            let pin_input = text_input("123456", &self.pin)
                .on_input(Message::PinChanged)
                .width(iced::Length::Fixed(120.0));

            let pair_enabled = self.pin.len() == 6 && !self.pairing;
            let pair_label = if self.pairing { "Pairing..." } else { "Pair" };
            let pair_btn = button(text(pair_label).align_x(Center))
                .on_press_maybe(if pair_enabled {
                    Some(Message::Pair)
                } else {
                    None
                })
                .style(appearance::s_button);

            content = content.push(
                row![pin_input, pair_btn]
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
