use iced::widget::{button, column, container, pick_list, row, rule, scrollable, text, text_input};
use iced::{Center, Color, Element, Fill, Task};
use plume_utils::discovery::mdns::MdnsDiscovery;
use plume_utils::discovery::{DeviceDiscovery, DeviceType, DiscoveredDevice};
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
        Self { content: s.into(), is_error: false }
    }
    fn error(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: true }
    }
    fn info(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false }
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
}

#[derive(Debug, Clone)]
pub struct TvOsPairingScreen {
    discovered: Vec<DiscoveredDevice>,
    selected_name: Option<String>,
    pin: String,
    scanning: bool,
    pairing: bool,
    status: Option<StatusMessage>,
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
        }
    }

    fn selected_device(&self) -> Option<&DiscoveredDevice> {
        let name = self.selected_name.as_deref()?;
        self.discovered.iter().find(|d| d.name == name)
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
                        MdnsDiscovery::new()
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
                // Only accept up to 6 digits
                self.pin = s.chars().filter(|c| c.is_ascii_digit()).take(6).collect();
                Task::none()
            }

            Message::Pair => {
                let Some(dev) = self.selected_device() else {
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
                let port = match dev.port {
                    Some(p) => p,
                    None => {
                        self.status = Some(StatusMessage::error("Selected device has no port."));
                        return Task::none();
                    }
                };

                let name = dev.name.clone();
                let hostname = dev.udid.clone().unwrap_or_else(|| {
                    ip_str.replace('.', "-")
                });
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
                        let device = Device::new_tvos(name, hostname, ip, port);
                        device
                            .pair_tvos(pin, cache_dir)
                            .await
                            .map(|_| ())
                            .map_err(|e| format!("Pairing failed: {e}"))
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
                    Ok(()) => {
                        self.status =
                            Some(StatusMessage::success("Paired successfully! You can now install apps on this Apple TV."));
                        self.pin.clear();
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
        let scan_label = if self.scanning { "Scanning..." } else { "Scan for Apple TVs" };
        let scan_btn = button(text(scan_label).align_x(Center))
            .on_press_maybe(if self.scanning { None } else { Some(Message::Scan) })
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

            let device_names: Vec<String> =
                self.discovered.iter().map(|d| d.name.clone()).collect();
            let selected = self.selected_name.clone();

            content = content.push(
                pick_list(device_names, selected, Message::SelectDevice)
                    .placeholder("Select an Apple TV")
                    .width(Fill),
            );
        }

        // PIN entry (only when a device is selected)
        if self.selected_name.is_some() {
            content = content
                .push(container(rule::horizontal(1)).padding([appearance::THEME_PADDING, 0.0]));

            content = content.push(
                text("Enter the 6-digit PIN shown on your Apple TV screen:").size(13),
            );

            let pin_input = text_input("123456", &self.pin)
                .on_input(Message::PinChanged)
                .width(iced::Length::Fixed(120.0));

            let pair_enabled = self.pin.len() == 6 && !self.pairing;
            let pair_label = if self.pairing { "Pairing..." } else { "Pair" };
            let pair_btn = button(text(pair_label).align_x(Center))
                .on_press_maybe(if pair_enabled { Some(Message::Pair) } else { None })
                .style(appearance::s_button);

            content = content.push(
                row![pin_input, pair_btn]
                    .spacing(appearance::THEME_PADDING)
                    .align_y(Center),
            );
        }

        container(scrollable(content)).into()
    }
}
