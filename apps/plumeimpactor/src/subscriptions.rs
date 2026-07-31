use iced::{Subscription, window};
use idevice::usbmuxd::{UsbmuxdConnection, UsbmuxdListenEvent};
use std::sync::Arc;
use tray_icon::{TrayIconEvent, menu::MenuEvent};

use crate::{
    defaults::get_data_path,
    screen::{Message, general, progress::ProgressUpdate},
};
use plume_utils::discovery::{DeviceDiscovery, PlatformDiscovery};
use plume_utils::{Bundle, Device, InstallProgress, PlistInfoTrait};

pub(crate) fn device_listener() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            100,
            |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::{SinkExt, StreamExt};
                let (tx, mut rx) = iced::futures::channel::mpsc::unbounded::<Message>();

                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();

                    rt.block_on(async move {
                        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                        {
                            if let Some(mac_udid) = plume_gestalt::get_udid() {
                                let _ = tx.unbounded_send(Message::DeviceConnected(Device {
                                    name: "This Mac".into(),
                                    udid: mac_udid,
                                    device_id: u32::MAX,
                                    usbmuxd_device: None,
                                    is_mac: true,
                                    pairing_address: None,
                                    reconnect_address: None,
                                    pairing_identity: None,
                                    pairing_cache_dir: None,
                                }));
                            }
                        }

                        let Ok(mut muxer) = UsbmuxdConnection::default().await else {
                            return;
                        };

                        if let Ok(devices) = muxer.get_devices().await {
                            for dev in devices {
                                let device = Device::new(dev).await;
                                let _ = tx.unbounded_send(Message::DeviceConnected(device));
                            }
                        }

                        let Ok(mut stream) = muxer.listen().await else {
                            return;
                        };

                        while let Some(event) = stream.next().await {
                            let msg = match event {
                                Ok(UsbmuxdListenEvent::Connected(dev)) => {
                                    Message::DeviceConnected(Device::new(dev).await)
                                }
                                Ok(UsbmuxdListenEvent::Disconnected(id)) => {
                                    Message::DeviceDisconnected(id)
                                }
                                Err(_) => continue,
                            };
                            let _ = tx.unbounded_send(msg);
                        }
                    });
                });

                while let Some(message) = rx.next().await {
                    let _ = output.send(message).await;
                }
            },
        )
    })
}

/// Discovers network Apple TVs over mDNS and feeds them into the same `DeviceConnected`/
/// `DeviceDisconnected` stream USB devices arrive on, so `screen/mod.rs` can list and select
/// them the same way. Never fabricates a UDID (`Device::new_tvos` leaves it empty; a real one is
/// adopted only via `Device::fetch_tvos_info`/`apply_tvos_info` once a pairing file exists); the
/// udid checks in `run_installation` and `RefreshDaemon::resign_and_reinstall` are what stop a
/// still-unenriched network device from being registered with Apple.
pub(crate) fn network_device_listener() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            100,
            |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::{SinkExt, StreamExt};
                use std::collections::{HashMap, HashSet};

                let (tx, mut rx) = iced::futures::channel::mpsc::unbounded::<Message>();

                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();

                    rt.block_on(async move {
                        // Addresses and udid last sent to the UI for each id, so a device can be
                        // re-emitted (screen/mod.rs replaces its stored copy in place) when
                        // either changes - e.g. a device first seen with only a reconnect
                        // address later gains a pairing address once the user opens its pairing
                        // screen, or a device adopts its real udid via enrichment below. udid is
                        // part of this key because otherwise a device already present here would
                        // never be re-emitted purely for having gone from an empty udid to a
                        // real one, and the enrichment below would have no way to reach the UI.
                        type EmittedState = (
                            Option<(std::net::IpAddr, u16)>,
                            Option<(std::net::IpAddr, u16)>,
                            String,
                        );

                        // Ids currently believed present: emitted as DeviceConnected and not yet
                        // followed by a DeviceDisconnected.
                        let mut present_ids: HashSet<u32> = HashSet::new();
                        let mut last_emitted: HashMap<u32, EmittedState> = HashMap::new();
                        // Consecutive scans in a row a present id was missing from the results.
                        // Reaching 2 is what actually emits DeviceDisconnected, so a single
                        // missed scan (a common mDNS hiccup, e.g. a dropped companion-link
                        // response) does not silently move the user's device selection -
                        // screen/mod.rs falls back to devices.first() on disconnect, which the
                        // user might not notice before installing to the wrong device.
                        let mut miss_counts: HashMap<u32, u32> = HashMap::new();
                        // Successfully fetched real identities, cached so a tunnel is attempted
                        // at most once per id rather than on every 30s scan.
                        let mut enriched: HashMap<u32, plume_utils::TvosDeviceInfo> =
                            HashMap::new();

                        loop {
                            let scan_started = std::time::Instant::now();
                            let scan_result = PlatformDiscovery::new()
                                .discover(std::time::Duration::from_secs(5))
                                .await;

                            match scan_result {
                                Ok(discovered) => {
                                    let cache_dir = get_data_path();
                                    let devices = plume_utils::discovery::group_network_devices(
                                        &discovered,
                                        &cache_dir,
                                    );

                                    let mut current_ids: HashSet<u32> = HashSet::new();

                                    for mut device in devices {
                                        let id = device.device_id;
                                        current_ids.insert(id);
                                        // Seen this round: any earlier miss streak is over.
                                        miss_counts.remove(&id);

                                        // A tunnel is attempted at most once per id, and only
                                        // once a pairing file exists - a device that has never
                                        // been paired has nothing to verify against, so a tunnel
                                        // attempt would fail on every single scan for no benefit.
                                        if let Some(info) = enriched.get(&id) {
                                            device.apply_tvos_info(info);
                                        } else if device.has_cached_pairing_file(&cache_dir) {
                                            match device.fetch_tvos_info(cache_dir.clone()).await {
                                                Ok(info) => {
                                                    device.apply_tvos_info(&info);
                                                    enriched.insert(id, info);
                                                }
                                                Err(e) => {
                                                    log::warn!(
                                                        "Could not fetch tvOS identity for {}: {e}",
                                                        device.name
                                                    );
                                                }
                                            }
                                        }

                                        let state: EmittedState = (
                                            device.pairing_address,
                                            device.reconnect_address,
                                            device.udid.clone(),
                                        );
                                        let changed = last_emitted.get(&id) != Some(&state);

                                        if !present_ids.contains(&id) || changed {
                                            present_ids.insert(id);
                                            last_emitted.insert(id, state);
                                            let _ =
                                                tx.unbounded_send(Message::DeviceConnected(device));
                                        }
                                    }

                                    let missing: Vec<u32> = present_ids
                                        .iter()
                                        .copied()
                                        .filter(|id| !current_ids.contains(id))
                                        .collect();

                                    for id in missing {
                                        let misses = miss_counts.entry(id).or_insert(0);
                                        *misses += 1;
                                        if *misses >= 2 {
                                            let _ =
                                                tx.unbounded_send(Message::DeviceDisconnected(id));
                                            present_ids.remove(&id);
                                            last_emitted.remove(&id);
                                            miss_counts.remove(&id);
                                        }
                                    }
                                }
                                Err(e) => {
                                    // Left entirely untouched: a scan failure carries no
                                    // information about which devices are still there, so it
                                    // must not count as a miss for anything, let alone an
                                    // immediate disconnect.
                                    log::warn!("Network device scan failed: {e}");
                                }
                            }

                            // A scan itself takes up to 5s, so sleeping a fixed 30s on top of
                            // it would drift the real cadence to ~35s; subtracting the elapsed
                            // scan time keeps each iteration starting roughly 30s after the
                            // previous one started.
                            let elapsed = scan_started.elapsed();
                            let sleep_for =
                                std::time::Duration::from_secs(30).saturating_sub(elapsed);
                            tokio::time::sleep(sleep_for).await;
                        }
                    });
                });

                while let Some(message) = rx.next().await {
                    let _ = output.send(message).await;
                }
            },
        )
    })
}

pub(crate) fn tray_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            100,
            |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::{SinkExt, StreamExt};
                let (tx, mut rx) = iced::futures::channel::mpsc::unbounded::<Message>();

                std::thread::spawn(move || {
                    let menu_channel = MenuEvent::receiver();
                    let tray_channel = TrayIconEvent::receiver();
                    loop {
                        if let Ok(event) = menu_channel.try_recv() {
                            let _ = tx.unbounded_send(Message::TrayMenuClicked(event.id));
                        }

                        if let Ok(event) = tray_channel.try_recv() {
                            match event {
                                TrayIconEvent::DoubleClick {
                                    button: tray_icon::MouseButton::Left,
                                    ..
                                } => {
                                    let _ = tx.unbounded_send(Message::TrayIconClicked);
                                }
                                _ => {}
                            }
                        }

                        #[cfg(target_os = "linux")]
                        {
                            let _ = tx.unbounded_send(Message::GtkTick);
                        }

                        #[cfg(target_os = "macos")]
                        {
                            let _ = tx.unbounded_send(Message::MacOsActivationTick);
                        }

                        std::thread::sleep(std::time::Duration::from_millis(32));
                    }
                });

                while let Some(message) = rx.next().await {
                    let _ = output.send(message).await;
                }
            },
        )
    })
}

pub(crate) fn tray_menu_refresh_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            10,
            |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::{SinkExt, StreamExt};
                let (tx, mut rx) = iced::futures::channel::mpsc::unbounded::<Message>();

                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(30));
                        let _ = tx.unbounded_send(Message::UpdateTrayMenu);
                    }
                });

                while let Some(message) = rx.next().await {
                    let _ = output.send(message).await;
                }
            },
        )
    })
}

pub(crate) fn certificate_reset_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            10,
            |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::{SinkExt, StreamExt};
                let (tx, mut rx) = iced::futures::channel::mpsc::unbounded::<Message>();

                std::thread::spawn(move || {
                    while let Some(request) = crate::certificate_reset::wait_for_request() {
                        let _ = tx.unbounded_send(Message::CertificateResetRequested(request));
                    }
                });

                while let Some(message) = rx.next().await {
                    let _ = output.send(message).await;
                }
            },
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub(crate) fn relaunch_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            10,
            |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::{SinkExt, StreamExt};
                let (tx, mut rx) = iced::futures::channel::mpsc::unbounded::<Message>();

                if let Err(err) = crate::relaunch::start_listener({
                    let tx = tx.clone();
                    move || {
                        let _ = tx.unbounded_send(Message::RelaunchRequested);
                    }
                }) {
                    log::warn!("Failed to start relaunch listener: {err}");
                }

                while let Some(message) = rx.next().await {
                    let _ = output.send(message).await;
                }
            },
        )
    })
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub(crate) fn relaunch_subscription() -> Subscription<Message> {
    Subscription::none()
}

pub(crate) fn file_hover_subscription() -> Subscription<Message> {
    let window_events = window::events().filter_map(|(_id, event)| match event {
        window::Event::FileHovered(_) => Some(Message::MainScreen(general::Message::FilesHovered)),
        window::Event::FilesHoveredLeft => {
            Some(Message::MainScreen(general::Message::FilesHoveredLeft))
        }
        window::Event::FileDropped(path) => {
            Some(Message::MainScreen(general::Message::FilesDropped(vec![
                path,
            ])))
        }
        _ => None,
    });

    window_events
}

pub(crate) fn installation_progress_listener(
    progress_rx: Option<Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ProgressUpdate>>>>,
) -> Subscription<ProgressUpdate> {
    match progress_rx {
        Some(rx) => {
            struct State {
                rx: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ProgressUpdate>>>,
            }

            impl std::hash::Hash for State {
                fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                    Arc::as_ptr(&self.rx).hash(state);
                }
            }

            let state = State { rx };
            Subscription::run_with(state, |state| {
                let rx = state.rx.clone();
                iced::stream::channel(
                    100,
                    move |mut output: iced::futures::channel::mpsc::Sender<ProgressUpdate>| async move {
                        use iced::futures::{SinkExt, StreamExt};

                        let (tx, mut rx_stream) =
                            iced::futures::channel::mpsc::unbounded::<ProgressUpdate>();

                        let rx_thread = rx.clone();
                        std::thread::spawn(move || {
                            loop {
                                // Drained to empty rather than one per tick: the terminal -1 and
                                // 100 updates must not sit behind a queue of earlier ones.
                                if let Ok(guard) = rx_thread.lock() {
                                    while let Ok(update) = guard.try_recv() {
                                        let _ = tx.unbounded_send(update);
                                    }
                                }

                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                        });

                        while let Some(message) = rx_stream.next().await {
                            let _ = output.send(message).await;
                        }
                    },
                )
            })
        }
        None => Subscription::none(),
    }
}

/// Samples upload throughput so a slow transfer can be told apart from a stalling one.
///
/// Reports the rate since the previous sample alongside the running average: a steady figure
/// means a systemic rate ceiling, while a figure that collapses and recovers means the transfer
/// is stalling, which points at the transport rather than at the rate itself.
struct UploadRate {
    started: std::time::Instant,
    last_at: std::time::Instant,
    last_bytes: u64,
}

impl UploadRate {
    /// How long to let pass between samples. Chunk callbacks arrive far too often to log each.
    const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last_at: now,
            last_bytes: 0,
        }
    }

    fn sample(&mut self, bytes_sent: u64) {
        let now = std::time::Instant::now();
        let since_last = now.duration_since(self.last_at);
        if since_last < Self::SAMPLE_INTERVAL {
            return;
        }

        let mb = |bytes: u64, seconds: f64| {
            if seconds > 0.0 {
                bytes as f64 / seconds / 1_000_000.0
            } else {
                0.0
            }
        };
        let current = mb(
            bytes_sent.saturating_sub(self.last_bytes),
            since_last.as_secs_f64(),
        );
        let overall = mb(bytes_sent, now.duration_since(self.started).as_secs_f64());

        log::info!(
            "upload: {} sent, {current:.2} MB/s now, {overall:.2} MB/s average",
            plume_utils::format_bytes(bytes_sent)
        );

        self.last_at = now;
        self.last_bytes = bytes_sent;
    }
}

/// Maps an upload byte count onto the 70..=90 band of the installation progress bar.
///
/// `bytes_total` is an estimate taken by a directory walk before the transfer starts, so
/// `bytes_sent` can overshoot it; the ratio is clamped so the bar stays inside the band. A
/// total of zero says nothing about how far the upload has got and reports the band floor
/// rather than dividing.
fn upload_progress_percent(bytes_sent: u64, bytes_total: u64) -> i32 {
    if bytes_total == 0 {
        return 70;
    }

    let scaled = (u128::from(bytes_sent) * 20 / u128::from(bytes_total)).min(20) as i32;
    70 + scaled
}

/// Maps an InstallationProxy percentage onto the 90..=99 band of the installation progress bar.
///
/// The ceiling of 99 is load bearing: `ProgressScreen::update` treats a progress value of 100
/// as the install being finished and tears the progress subscription down, while the pairing
/// record install and the refresh-store save still run after this callback.
fn install_progress_percent(percent: u64) -> i32 {
    let scaled = (percent.min(100) * 9 / 100) as i32;
    90 + scaled
}

pub(crate) async fn run_installation(
    package: &plume_utils::Package,
    device: Option<&Device>,
    options: &plume_utils::SignerOptions,
    account: Option<&plume_store::GsaAccount>,
    mut store: Option<&mut plume_store::AccountStore>,
    tx: &std::sync::mpsc::Sender<ProgressUpdate>,
) -> Result<(), String> {
    use plume_core::{
        AnisetteConfiguration, CertificateIdentity,
        developer::{DeveloperPlatform, DeveloperSession},
    };
    use plume_utils::{Signer, SignerInstallMode, SignerMode};

    let package_file: Bundle;
    let mut options = options.clone();
    let send = |msg: String, progress: i32| {
        let _ = tx.send(ProgressUpdate::new(msg, progress));
    };
    let platform = match device {
        Some(dev) if dev.is_tvos() => DeveloperPlatform::TvOs,
        _ => DeveloperPlatform::IOs,
    };

    send("Preparing package...".to_string(), 10);

    match options.mode {
        SignerMode::Pem => {
            let Some(account) = account else {
                return Err("GSA account is required for PEM signing".to_string());
            };

            // Checked before any portal call, not just before qh_ensure_device: on a
            // free-tier account at its certificate limit, CertificateIdentity::new_with_session
            // below can revoke an existing certificate to make room for a new one, which
            // invalidates apps already sideloaded on the user's other devices. That cost must
            // not be paid for an install that is going to fail this same check a few lines
            // later anyway.
            if let Some(dev) = &device {
                if dev.udid.is_empty() {
                    return Err("Device UDID is unknown; cannot register it with Apple".to_string());
                }
            }

            send("Ensuring account is valid...".to_string(), 20);

            let session = DeveloperSession::new(
                account.adsid().clone(),
                account.xcode_gs_token().clone(),
                AnisetteConfiguration::default()
                    .set_configuration_path(crate::defaults::get_data_path()),
            )
            .await
            .map_err(|e| e.to_string())?;

            let teams_response = session.qh_list_teams().await.map_err(|e| e.to_string())?;

            if teams_response.teams.is_empty() {
                return Err("No teams available for this account".to_string());
            }

            let team_id = account.team_id();

            if !team_id.is_empty() && !teams_response.teams.iter().any(|t| &t.team_id == team_id) {
                return Err(format!(
                    "Stored team ID '{}' not found in available teams. Please update your team selection in Settings.",
                    team_id
                ));
            }

            let team_id = if team_id.is_empty() {
                &teams_response.teams[0].team_id
            } else {
                team_id
            };

            let mut on_certificate_reset = || {
                send(crate::certificate_reset::WARNING.to_string(), 20);
                crate::certificate_reset::confirm()
            };
            let identity = CertificateIdentity::new_with_session(
                &session,
                crate::defaults::get_data_path(),
                None,
                team_id,
                false,
                Some(&mut on_certificate_reset),
            )
            .await
            .map_err(|e| e.to_string())?;

            send("Ensuring device is registered...".to_string(), 30);

            if let Some(dev) = &device {
                session
                    .qh_ensure_device(team_id, &dev.name, &dev.udid, platform)
                    .await
                    .map_err(|e| e.to_string())?;
            }

            send("Extracting package...".to_string(), 50);

            let mut signer = Signer::new(Some(identity), options.clone());

            let bundle = package.get_package_bundle().map_err(|e| e.to_string())?;

            send("Signing package...".to_string(), 70);

            signer
                .modify_bundle(&bundle, &Some(team_id.clone()))
                .await
                .map_err(|e| e.to_string())?;
            signer
                .register_bundle(&bundle, &session, team_id, false, platform)
                .await
                .map_err(|e| e.to_string())?;
            signer
                .sign_bundle(&bundle)
                .await
                .map_err(|e| e.to_string())?;

            options = signer.options.clone();
            package_file = bundle;
        }
        SignerMode::Adhoc => {
            send("Extracting package...".to_string(), 50);

            let mut signer = Signer::new(None, options.clone());

            let bundle = package.get_package_bundle().map_err(|e| e.to_string())?;

            send("Signing package...".to_string(), 70);

            signer
                .modify_bundle(&bundle, &None)
                .await
                .map_err(|e| e.to_string())?;
            signer
                .sign_bundle(&bundle)
                .await
                .map_err(|e| e.to_string())?;

            options = signer.options.clone();
            package_file = bundle;
        }
        _ => {
            send("Extracting package...".to_string(), 50);

            let bundle = package.get_package_bundle().map_err(|e| e.to_string())?;

            package_file = bundle;
        }
    }

    match options.install_mode {
        SignerInstallMode::Install => {
            if let Some(dev) = &device {
                if !dev.is_mac {
                    let upload_status = match plume_utils::dir_total_bytes(
                        package_file.bundle_dir(),
                    )
                    .await
                    {
                        Ok(bytes) => format!(
                            "Sending to device ({}) - this can take several minutes over the network.",
                            plume_utils::format_bytes(bytes)
                        ),
                        Err(_) => {
                            "Sending to device - this can take several minutes over the network."
                                .to_string()
                        }
                    };
                    let _ = tx.send(ProgressUpdate::indeterminate(upload_status, 70));

                    let tx_clone = tx.clone();
                    // A chunk callback fires per megabyte and per file, far faster than the
                    // progress listener drains, so only updates that change what the bar shows
                    // are queued. The phase is part of the key because the upload band ends on
                    // the same value the install band starts on.
                    let last_sent: Arc<std::sync::Mutex<Option<(bool, i32)>>> =
                        Arc::new(std::sync::Mutex::new(None));
                    let rate = Arc::new(std::sync::Mutex::new(UploadRate::new()));

                    dev.install_app(
                        &package_file.bundle_dir(),
                        move |progress: InstallProgress| {
                            let tx = tx_clone.clone();
                            let last_sent = last_sent.clone();
                            let rate = rate.clone();
                            // Some libraries expect this future to be processed.
                            // We ensure it sends and resolves immediately.
                            Box::pin(async move {
                                if let InstallProgress::Uploading { bytes_sent, .. } = progress
                                    && let Ok(mut rate) = rate.lock()
                                {
                                    rate.sample(bytes_sent);
                                }

                                let update = match progress {
                                    InstallProgress::Uploading {
                                        bytes_sent,
                                        bytes_total,
                                    } => Some((
                                        false,
                                        upload_progress_percent(bytes_sent, bytes_total),
                                        format!(
                                            "Sending to device ({} / {})...",
                                            plume_utils::format_bytes(bytes_sent),
                                            plume_utils::format_bytes(bytes_total)
                                        ),
                                    )),
                                    InstallProgress::Installing { percent } => Some((
                                        true,
                                        install_progress_percent(percent),
                                        "Installing...".to_string(),
                                    )),
                                    _ => None,
                                };

                                let Some((installing, percent, status)) = update else {
                                    return;
                                };

                                let changed = match last_sent.lock() {
                                    Ok(mut last) => {
                                        let key = (installing, percent);
                                        let changed = *last != Some(key);
                                        *last = Some(key);
                                        changed
                                    }
                                    Err(_) => true,
                                };

                                if changed {
                                    let _ = tx.send(ProgressUpdate::new(status, percent));
                                }
                            })
                        },
                    )
                    .await
                    .map_err(|e| format!("Install error: {}", e))?;

                    if options.app.supports_pairing_file() {
                        if let (Some(custom_identifier), Some(pairing_file_bundle_path)) = (
                            options.custom_identifier.as_ref(),
                            options.app.pairing_file_path(),
                        ) {
                            let _ = dev
                                .install_pairing_record(
                                    custom_identifier,
                                    &pairing_file_bundle_path,
                                )
                                .await;
                        }
                    }
                } else {
                    send("Installing...".to_string(), 90);

                    plume_utils::install_app_mac(&package_file.bundle_dir())
                        .await
                        .map_err(|e| e.to_string())?;
                }
            } else {
                return Err("No device connected for installation".to_string());
            }
        }
        SignerInstallMode::Export => {
            send("Exporting...".to_string(), 90);

            let archive_path = package
                .get_archive_based_on_path(&package_file.bundle_dir())
                .map_err(|e| e.to_string())?;

            let file = rfd::AsyncFileDialog::new()
                .set_title("Save Package As")
                .set_file_name(
                    archive_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("package.ipa"),
                )
                .save_file()
                .await;

            if let Some(save_path) = file {
                tokio::fs::copy(&archive_path, &save_path.path())
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    if options.refresh && options.mode == SignerMode::Pem {
        send("Saving for refresh...".to_string(), 99);
        let path = get_data_path().join("refresh_store");
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| e.to_string())?;

        let original_name = package_file
            .bundle_dir()
            .file_name()
            .unwrap()
            .to_string_lossy();
        let uuid = uuid::Uuid::new_v4();
        let dest_name = if let Some(dot_pos) = original_name.rfind('.') {
            let (name, ext) = original_name.split_at(dot_pos);
            format!("{}-{}{}", name, uuid, ext)
        } else {
            format!("{}-{}", original_name, uuid)
        };
        let dest_path = path.join(dest_name);

        plume_utils::copy_dir_recursively(&package_file.bundle_dir(), &dest_path)
            .await
            .map_err(|e| e.to_string())?;

        if let (Some(dev), Some(account), Some(store)) = (&device, &account, store.as_mut()) {
            let embedded_prov_path = dest_path.join("embedded.mobileprovision");

            let provision_path = if embedded_prov_path.exists() {
                Some(embedded_prov_path)
            } else {
                None
            };

            if let Some(prov_path) = provision_path {
                use plume_core::MobileProvision;

                if let Ok(provision) = MobileProvision::load_with_path(&prov_path) {
                    let expiration_date = provision.expiration_date().clone();
                    let scheduled_refresh = expiration_date
                        .to_xml_format()
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .unwrap_or_else(|_| chrono::Utc::now() + chrono::Duration::days(4));
                    let scheduled_refresh = scheduled_refresh - chrono::Duration::days(3);

                    let refresh_app = plume_store::RefreshApp {
                        name: package_file.get_name(),
                        bundle_id: package_file.get_bundle_identifier(),
                        path: dest_path.clone(),
                        scheduled_refresh,
                    };

                    let mut refresh_device = store
                        .get_refresh_device(&dev.udid)
                        .cloned()
                        .unwrap_or_else(|| plume_store::RefreshDevice {
                            udid: dev.udid.clone(),
                            name: dev.name.clone(),
                            account: account.email().clone(),
                            apps: Vec::new(),
                            is_mac: dev.is_mac,
                        });

                    if let Some(existing_app) = refresh_device
                        .apps
                        .iter_mut()
                        .find(|a| a.bundle_id == refresh_app.bundle_id)
                    {
                        *existing_app = refresh_app;
                    } else {
                        refresh_device.apps.push(refresh_app);
                    }

                    store
                        .add_or_update_refresh_device_sync(refresh_device)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }

    send("Finished!".to_string(), 100);

    Ok(())
}

#[allow(dead_code)]
pub(crate) async fn export_certificate(account: plume_store::GsaAccount) -> Result<(), String> {
    use plume_core::{AnisetteConfiguration, CertificateIdentity, developer::DeveloperSession};

    let session = DeveloperSession::new(
        account.adsid().clone(),
        account.xcode_gs_token().clone(),
        AnisetteConfiguration::default().set_configuration_path(crate::defaults::get_data_path()),
    )
    .await
    .map_err(|e| e.to_string())?;

    let teams_response = session.qh_list_teams().await.map_err(|e| e.to_string())?;

    if teams_response.teams.is_empty() {
        return Err("No teams available for this account".to_string());
    }

    let team_id = account.team_id();

    if !team_id.is_empty() && !teams_response.teams.iter().any(|t| &t.team_id == team_id) {
        return Err(format!(
            "Stored team ID '{}' not found in available teams. Please update your team selection in Settings.",
            team_id
        ));
    }

    let team_id = if team_id.is_empty() {
        &teams_response.teams[0].team_id
    } else {
        team_id
    };

    let mut on_certificate_reset = crate::certificate_reset::confirm;
    let identity = CertificateIdentity::new_with_session(
        &session,
        crate::defaults::get_data_path(),
        None,
        team_id,
        true,
        Some(&mut on_certificate_reset),
    )
    .await
    .map_err(|e| e.to_string())?;

    let Some(p12_data) = identity.p12_data else {
        return Err("Missing p12 data".to_string());
    };

    let archive_path =
        crate::defaults::get_data_path().join(format!("{}_certificate.p12", team_id));
    tokio::fs::write(&archive_path, p12_data)
        .await
        .map_err(|e| e.to_string())?;

    let file = rfd::AsyncFileDialog::new()
        .set_title("Save Certificate As")
        .set_file_name(
            archive_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("certificate.p12"),
        )
        .save_file()
        .await;

    if let Some(save_path) = file {
        tokio::fs::copy(&archive_path, &save_path.path())
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

pub(crate) async fn fetch_teams(
    account: &plume_store::GsaAccount,
) -> Result<Vec<crate::screen::settings::Team>, String> {
    use plume_core::{AnisetteConfiguration, developer::DeveloperSession};

    let session = DeveloperSession::new(
        account.adsid().clone(),
        account.xcode_gs_token().clone(),
        AnisetteConfiguration::default().set_configuration_path(crate::defaults::get_data_path()),
    )
    .await
    .map_err(|e| e.to_string())?;

    let teams_response = session.qh_list_teams().await.map_err(|e| e.to_string())?;

    Ok(teams_response
        .teams
        .into_iter()
        .map(|t| crate::screen::settings::Team {
            name: t.name,
            id: t.team_id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{install_progress_percent, upload_progress_percent};

    #[test]
    fn upload_percent_with_zero_total_reports_the_band_floor() {
        assert_eq!(upload_progress_percent(0, 0), 70);
        assert_eq!(upload_progress_percent(999, 0), 70);
    }

    #[test]
    fn upload_percent_spans_the_band_ends() {
        assert_eq!(upload_progress_percent(0, 54_741_568), 70);
        assert_eq!(upload_progress_percent(54_741_568, 54_741_568), 90);
    }

    #[test]
    fn upload_percent_is_halfway_at_half_the_bytes() {
        assert_eq!(upload_progress_percent(27_370_784, 54_741_568), 80);
    }

    #[test]
    fn upload_percent_clamps_when_sent_exceeds_total() {
        assert_eq!(upload_progress_percent(54_741_569, 54_741_568), 90);
        assert_eq!(upload_progress_percent(u64::MAX, 1), 90);
    }

    #[test]
    fn upload_percent_does_not_overflow_on_large_transfers() {
        let four_gb: u64 = 4 * 1024 * 1024 * 1024;
        assert_eq!(upload_progress_percent(four_gb, four_gb), 90);
        assert_eq!(upload_progress_percent(four_gb / 2, four_gb), 80);
        assert_eq!(upload_progress_percent(u64::MAX, u64::MAX), 90);
    }

    #[test]
    fn upload_percent_stays_inside_the_band() {
        let total: u64 = 54_741_568;
        for step in 0..=40u64 {
            let sent = total * step / 20;
            let value = upload_progress_percent(sent, total);
            assert!((70..=90).contains(&value), "sent {sent} produced {value}");
        }
    }

    #[test]
    fn install_percent_spans_the_band_without_reaching_one_hundred() {
        assert_eq!(install_progress_percent(0), 90);
        assert_eq!(install_progress_percent(50), 94);
        assert_eq!(install_progress_percent(100), 99);
    }

    #[test]
    fn install_percent_clamps_out_of_range_input() {
        assert_eq!(install_progress_percent(250), 99);
        assert_eq!(install_progress_percent(u64::MAX), 99);
    }

    #[test]
    fn install_percent_never_reports_finished() {
        for percent in 0..=300u64 {
            let value = install_progress_percent(percent);
            assert!(
                (90..=99).contains(&value),
                "percent {percent} produced {value}"
            );
        }
    }

    /// A completed upload and a starting install must meet, so the bar neither jumps nor
    /// stalls at the handover between the two bands.
    #[test]
    fn the_bands_meet_at_the_upload_install_seam() {
        assert_eq!(upload_progress_percent(54_741_568, 54_741_568), 90);
        assert_eq!(install_progress_percent(0), 90);
    }

    /// The install band has to stop below the value that "Saving for refresh..." reports, which
    /// runs after the install finishes.
    #[test]
    fn the_install_band_stays_under_the_refresh_save() {
        assert!(install_progress_percent(100) <= 99);
    }
}
