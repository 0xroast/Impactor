use std::fmt;
use std::path::{Component, Path, PathBuf};

use idevice::core_device_proxy::CoreDeviceProxy;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::misagent::MisagentClient;
use idevice::provider::UsbmuxdProvider;
use idevice::remote_pairing::{
    RemotePairingClient, RpPairingFile, RpPairingSocket, connect_tls_psk_tunnel_native,
};
use idevice::rsd::RsdHandshake;
use idevice::tcp::adapter::Adapter;
use idevice::tcp::handle::AdapterHandle;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice};
use idevice::utils::installation;
use idevice::{IdeviceService, RsdService, RemoteXpcClient};
use plume_core::MobileProvision;

use crate::Error;
use crate::options::SignerAppReal;
use idevice::afc::opcode::AfcFopenMode;
use idevice::house_arrest::HouseArrestClient;
use idevice::usbmuxd::UsbmuxdConnection;
use plist::Value;

pub const CONNECTION_LABEL: &str = "plume_info";
pub const INSTALLATION_LABEL: &str = "plume_install";
pub const HOUSE_ARREST_LABEL: &str = "plume_house_arrest";

macro_rules! get_dict_string {
    ($dict:expr, $key:expr) => {
        $dict
            .as_dictionary()
            .and_then(|dict| dict.get($key))
            .and_then(|v| v.as_string())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "".to_string())
    };
}

#[derive(Debug, Clone)]
pub struct Device {
    pub name: String,
    pub udid: String,
    pub device_id: u32,
    pub usbmuxd_device: Option<UsbmuxdDevice>,
    // On x86_64 macs, `is_mac` variable should never be true
    // since its only true if the device is added manually.
    pub is_mac: bool,
    /// Address of the `_remotepairing-manual-pairing._tcp.local.` service, advertised only
    /// while the Apple TV is actively showing a pairing PIN. Required for first-time pairing;
    /// not valid once the pairing UI is dismissed.
    pub pairing_address: Option<(std::net::IpAddr, u16)>,
    /// Address of the `_remotepairing._tcp.local.` service, advertised whenever the Apple TV
    /// has an established pairing. Used to reconnect (pair-verify) after the initial pairing.
    pub reconnect_address: Option<(std::net::IpAddr, u16)>,
}

impl Device {
    pub async fn new(usbmuxd_device: UsbmuxdDevice) -> Self {
        let name = Self::get_name_from_usbmuxd_device(&usbmuxd_device)
            .await
            .unwrap_or_default();

        Device {
            name,
            udid: usbmuxd_device.udid.clone(),
            device_id: usbmuxd_device.device_id.clone(),
            usbmuxd_device: Some(usbmuxd_device),
            is_mac: false,
            pairing_address: None,
            reconnect_address: None,
        }
    }

    /// Create a Device representing a network Apple TV (no USB connection).
    /// `hostname` is used as the stable identifier (e.g. "Apple-TV.local").
    ///
    /// `pairing_address` should come from `_remotepairing-manual-pairing._tcp.local.` and is
    /// required for `pair_tvos()`. `reconnect_address` should come from
    /// `_remotepairing._tcp.local.` and is preferred by `establish_tvos_tunnel()` once a device
    /// is already paired; when absent, `pairing_address` is used as a fallback.
    pub fn new_tvos(
        name: String,
        hostname: String,
        ip: std::net::IpAddr,
        pairing_port: Option<u16>,
        reconnect_port: Option<u16>,
    ) -> Self {
        Device {
            name,
            udid: hostname,
            device_id: 0,
            usbmuxd_device: None,
            is_mac: false,
            pairing_address: pairing_port.map(|port| (ip, port)),
            reconnect_address: reconnect_port.map(|port| (ip, port)),
        }
    }

    async fn get_name_from_usbmuxd_device(device: &UsbmuxdDevice) -> Result<String, Error> {
        let mut lockdown =
            LockdownClient::connect(&device.to_provider(UsbmuxdAddr::default(), CONNECTION_LABEL))
                .await?;
        let values = lockdown.get_value(None, None).await?;
        Ok(get_dict_string!(values, "DeviceName"))
    }

    pub async fn installed_apps(&self) -> Result<Vec<SignerAppReal>, Error> {
        let device = match &self.usbmuxd_device {
            Some(dev) => dev,
            None => return Err(Error::Other("Device is not connected via USB".to_string())),
        };

        let provider = device.to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            INSTALLATION_LABEL,
        );

        let mut ic = InstallationProxyClient::connect(&provider).await?;
        let apps = ic.get_apps(Some("User"), None).await?;

        let mut found_apps = Vec::new();

        for (bundle_id, info) in apps {
            let app_name = get_app_name_from_info(&info);
            let signer_app = SignerAppReal::from_bundle_identifier_and_name(
                Some(bundle_id.as_str()),
                app_name.as_deref(),
            );

            if signer_app.app.supports_pairing_file_alt()
                && !found_apps
                    .iter()
                    .any(|a: &SignerAppReal| a.bundle_id == signer_app.bundle_id)
            {
                found_apps.push(signer_app);
            }
        }

        Ok(found_apps)
    }

    pub async fn is_app_installed(&self, bundle_id: &str) -> Result<bool, Error> {
        let device = match &self.usbmuxd_device {
            Some(dev) => dev,
            None => return Err(Error::Other("Device is not connected via USB".to_string())),
        };

        let provider = device.to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            INSTALLATION_LABEL,
        );

        let mut ic = InstallationProxyClient::connect(&provider).await?;
        let apps = ic.get_apps(Some("User"), None).await?;

        Ok(apps.contains_key(bundle_id))
    }

    pub async fn install_profile(&self, profile: &MobileProvision) -> Result<(), Error> {
        if self.usbmuxd_device.is_none() {
            return Err(Error::Other("Device is not connected via USB".to_string()));
        }

        let provider = self.usbmuxd_device.clone().unwrap().to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            INSTALLATION_LABEL,
        );

        let mut mc = MisagentClient::connect(&provider).await?;
        mc.install(profile.data.clone()).await?;

        Ok(())
    }

    pub async fn pair(&self) -> Result<(), Error> {
        if self.usbmuxd_device.is_none() {
            return Err(Error::Other("Device is not connected via USB".to_string()));
        }

        let mut usbmuxd = UsbmuxdConnection::default().await?;

        let provider = self.usbmuxd_device.clone().unwrap().to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            INSTALLATION_LABEL,
        );

        let mut lc = LockdownClient::connect(&provider).await?;
        let id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let buid = usbmuxd.get_buid().await?;
        let mut pairing_file = lc.pair(id, buid, None).await?;
        pairing_file.udid = Some(self.udid.clone());
        let pairing_file = pairing_file.serialize()?;

        usbmuxd.save_pair_record(&self.udid, pairing_file).await?;

        Ok(())
    }

    pub async fn install_pairing_record(
        &self,
        identifier: &String,
        path: &str,
    ) -> Result<(), Error> {
        if self.usbmuxd_device.is_none() {
            return Err(Error::Other("Device is not connected via USB".to_string()));
        }

        let mut usbmuxd = UsbmuxdConnection::default().await?;
        let provider = self
            .usbmuxd_device
            .clone()
            .unwrap()
            .to_provider(UsbmuxdAddr::default(), HOUSE_ARREST_LABEL);
        let mut pairing_file = usbmuxd.get_pair_record(&self.udid).await?;

        // saving pairing record requires enabling wifi debugging
        // since operations are done over wifi
        let mut lc = LockdownClient::connect(&provider).await?;
        lc.start_session(&pairing_file).await.ok();
        lc.set_value(
            "EnableWifiDebugging",
            true.into(),
            Some("com.apple.mobile.wireless_lockdown"),
        )
        .await
        .ok();

        pairing_file.udid = Some(self.udid.clone());

        let hc = HouseArrestClient::connect(&provider).await?;
        let mut ac = hc.vend_documents(identifier.clone()).await?;
        if let Some(parent) = Path::new(path).parent() {
            let mut current = String::new();
            let has_root = parent.has_root();

            for component in parent.components() {
                if let Component::Normal(dir) = component {
                    if has_root && current.is_empty() {
                        current.push('/');
                    } else if !current.is_empty() && !current.ends_with('/') {
                        current.push('/');
                    }

                    current.push_str(&dir.to_string_lossy());
                    ac.mk_dir(&current).await?;
                }
            }
        }

        let mut f = ac.open(path, AfcFopenMode::Wr).await?;
        f.write_entire(&pairing_file.serialize().unwrap()).await?;

        Ok(())
    }

    pub async fn install_remote_pairing_record(
        &self,
        identifier: &String,
        path: &str,
        path_to_store: PathBuf,
    ) -> Result<(), Error> {
        if self.usbmuxd_device.is_none() {
            return Err(Error::Other("Device is not connected via USB".to_string()));
        }

        let provider = self
            .usbmuxd_device
            .clone()
            .unwrap()
            .to_provider(UsbmuxdAddr::default(), HOUSE_ARREST_LABEL);

        let pairing_file = self.get_rsd_pairing_file(&provider, path_to_store).await?;

        let hc = HouseArrestClient::connect(&provider).await?;
        let mut ac = hc.vend_documents(identifier.clone()).await?;
        if let Some(parent) = Path::new(path).parent() {
            let mut current = String::new();
            let has_root = parent.has_root();

            for component in parent.components() {
                if let Component::Normal(dir) = component {
                    if has_root && current.is_empty() {
                        current.push('/');
                    } else if !current.is_empty() && !current.ends_with('/') {
                        current.push('/');
                    }

                    current.push_str(&dir.to_string_lossy());
                    ac.mk_dir(&current).await?;
                }
            }
        }

        let mut f = ac.open(path, AfcFopenMode::Wr).await?;
        f.write_entire(&pairing_file.to_bytes()).await?;

        Ok(())
    }

    async fn get_rsd_pairing_file(
        &self,
        provider: &UsbmuxdProvider,
        path: PathBuf,
    ) -> Result<RpPairingFile, Error> {
        let pairing_file_path = path.join(format!("plume_{}.plist", self.udid));

        if pairing_file_path.exists() {
            return Ok(RpPairingFile::read_from_file(pairing_file_path).await?);
        } else {
            let cdp = CoreDeviceProxy::connect(provider).await?;
            let cdp_port = cdp.tunnel_info().server_rsd_port;
            let cdp_adapter = cdp.create_software_tunnel()?;
            let mut cdp_adapter = cdp_adapter.to_async_handle();

            let cdp_stream = cdp_adapter.connect(cdp_port).await?;
            let cdp_handshake = RsdHandshake::new(cdp_stream).await?;

            let tunnel_service = cdp_handshake
                .services
                .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
                .ok_or_else(|| Error::Other("Tunnel service not found".to_string()))?;

            let tunnel_service_stream = cdp_adapter.connect(tunnel_service.port).await?;
            let mut remote_xpc = RemoteXpcClient::new(tunnel_service_stream).await?;
            remote_xpc.do_handshake().await?;
            let _ = remote_xpc.recv_root().await;

            let suffix: String = uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(6)
                .collect();

            let hostname = format!("plume-{}", suffix);

            let mut pairing_file = RpPairingFile::generate(&hostname);
            let mut pairing_client =
                RemotePairingClient::new(remote_xpc, &hostname, &mut pairing_file);
            pairing_client
                .connect(async |_| "000000".to_string(), ())
                .await?;

            let tunnel_service_stream = cdp_adapter.connect(tunnel_service.port).await?;
            let mut remote_xpc = RemoteXpcClient::new(tunnel_service_stream).await?;
            remote_xpc.do_handshake().await?;
            let _ = remote_xpc.recv_root().await;
            let mut pairing_client =
                RemotePairingClient::new(remote_xpc, &hostname, &mut pairing_file);
            pairing_client
                .connect(async |_| "000000".to_string(), ())
                .await?;

            let pairing_file_bytes = pairing_file.to_bytes();

            tokio::fs::write(&pairing_file_path, &pairing_file_bytes).await?;

            Ok(pairing_file)
        }
    }

    /// Pair with a network Apple TV (tvOS 17.4+) using the RPPairing binary protocol.
    /// Connects to the mDNS `_remotepairing-manual-pairing._tcp.local.` service - the only
    /// service that accepts a first-time SRP pair-setup - which is advertised only while the
    /// Apple TV is actively showing a pairing PIN (Settings > Remotes and Devices > Remote App
    /// and Devices). `pin` is the 6-digit code shown on the Apple TV screen.
    /// The resulting pairing file is cached at `cache_dir/plume_<hostname>.plist`.
    /// `pin_provider` is invoked only once the device has accepted the pair-setup request, which
    /// is the moment its PIN appears on screen. Taking a provider rather than a string is what
    /// lets a caller prompt for the code at that point: the code does not exist before then, and
    /// the device stops displaying it as soon as the session closes.
    pub async fn pair_tvos<F, Fut>(
        &self,
        pin_provider: F,
        cache_dir: PathBuf,
    ) -> Result<RpPairingFile, Error>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = String>,
    {
        let (ip, port) = self.pairing_address.ok_or_else(|| {
            Error::Other(
                "Device is not advertising the pairing service. On the Apple TV, open Settings \
                 > Remotes and Devices > Remote App and Devices and wait for \"Waiting to \
                 Pair...\", then scan again."
                    .to_string(),
            )
        })?;

        let cache_path = cache_dir.join(format!("plume_{}.plist", self.udid));

        if cache_path.exists() {
            return Ok(RpPairingFile::read_from_file(&cache_path).await?);
        }

        let addr = std::net::SocketAddr::new(ip, port);
        log::info!("tvOS pairing: connecting to {addr}");
        let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
            Error::Other(format!(
                "Failed to connect to Apple TV at {addr}: {e}. The manual-pairing port changes \
                 each time the Apple TV re-advertises, so a stale scan result will not connect - \
                 scan again immediately before pairing."
            ))
        })?;
        log::info!("tvOS pairing: TCP connected to {addr}, starting RPPairing handshake");

        let conn = RpPairingSocket::new(stream);

        let suffix: String = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(6)
            .collect();
        let hostname = format!("plume-{suffix}");

        let mut pairing_file = RpPairingFile::generate(&hostname);
        let mut pairing_client = RemotePairingClient::new(conn, &hostname, &mut pairing_file);
        pairing_client
            .connect(
                move |_: u8| {
                    log::info!("tvOS pairing: device requested the PIN");
                    pin_provider()
                },
                0u8,
            )
            .await
            .map_err(|e| Error::Other(format!("RPPairing handshake failed: {e}")))?;
        log::info!("tvOS pairing: handshake succeeded, caching pairing file");

        tokio::fs::create_dir_all(&cache_dir).await?;
        pairing_file.write_to_file(&cache_path).await?;

        Ok(pairing_file)
    }

    /// Establish a TLS-PSK tunnel to a paired Apple TV and discover RSD services.
    /// This is the post-pairing step that gives access to InstallationProxy, AFC, etc.
    ///
    /// Prefers `reconnect_address` (`_remotepairing._tcp.local.`), which is what a paired
    /// device keeps advertising and only needs a pair-verify against - no PIN required in the
    /// common case. Falls back to `pairing_address` if no reconnect address is known (e.g.
    /// installing immediately after pairing, in the same scan, before a reconnect address has
    /// been observed). `pin` is only actually used if pair-verify fails and the client falls
    /// back to a full SRP pair-setup.
    pub async fn establish_tvos_tunnel(
        &self,
        pin: String,
        cache_dir: PathBuf,
    ) -> Result<(AdapterHandle, RsdHandshake), Error> {
        let (ip, port) = self
            .reconnect_address
            .or(self.pairing_address)
            .ok_or_else(|| Error::Other("Device has no network address".to_string()))?;

        let connect_addr = std::net::SocketAddr::new(ip, port);

        // Load or generate pairing file
        let cache_path = cache_dir.join(format!("plume_{}.plist", self.udid));
        let mut pairing_file = if cache_path.exists() {
            RpPairingFile::read_from_file(&cache_path).await?
        } else {
            let suffix: String = uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(6)
                .collect();
            RpPairingFile::generate(&format!("plume-{suffix}"))
        };

        // Connect via RPPairing binary protocol
        let stream = tokio::net::TcpStream::connect(connect_addr)
            .await
            .map_err(|e| {
                Error::Other(format!("Could not connect to Apple TV at {connect_addr}: {e}"))
            })?;
        let conn = RpPairingSocket::new(stream);

        let hostname = pairing_file.identifier.clone();
        let tunnel = {
            let mut rpc = RemotePairingClient::new(conn, &hostname, &mut pairing_file);
            rpc.connect(
                move |_: u8| {
                    let p = pin.clone();
                    async move { p }
                },
                0u8,
            )
            .await
            .map_err(|e| Error::Other(format!("Pairing/verification failed: {e}")))?;

            let tunnel_port = rpc
                .create_tcp_listener()
                .await
                .map_err(|e| Error::Other(format!("Failed to create tunnel listener: {e}")))?;

            let tunnel_addr = std::net::SocketAddr::new(connect_addr.ip(), tunnel_port);
            let tunnel_stream = tokio::net::TcpStream::connect(tunnel_addr)
                .await
                .map_err(|e| Error::Other(format!("TLS tunnel connect failed: {e}")))?;

            connect_tls_psk_tunnel_native(Box::new(tunnel_stream), rpc.encryption_key())
                .await
                .map_err(|e| Error::Other(format!("TLS-PSK tunnel handshake failed: {e}")))?
        };

        // Cache pairing file now that rpc is dropped
        if !cache_path.exists() {
            tokio::fs::create_dir_all(&cache_dir).await?;
            pairing_file.write_to_file(&cache_path).await?;
        }

        let client_ip: std::net::IpAddr = tunnel
            .info
            .client_address
            .parse()
            .map_err(|e| Error::Other(format!("Invalid tunnel client address: {e}")))?;
        let server_ip: std::net::IpAddr = tunnel
            .info
            .server_address
            .parse()
            .map_err(|e| Error::Other(format!("Invalid tunnel server address: {e}")))?;
        let rsd_port = tunnel.info.server_rsd_port;

        let raw = tunnel.into_inner();
        let adapter = Adapter::new(Box::new(raw), client_ip, server_ip);
        let mut adapter_handle = adapter.to_async_handle();

        let rsd_stream = adapter_handle
            .connect(rsd_port)
            .await
            .map_err(|e| Error::Other(format!("RSD connection failed: {e}")))?;
        let handshake = RsdHandshake::new(rsd_stream)
            .await
            .map_err(|e| Error::Other(format!("RSD handshake failed: {e}")))?;

        Ok((adapter_handle, handshake))
    }

    /// Install an IPA on a paired Apple TV over a TLS-PSK tunnel.
    /// Establishes tunnel, uploads via AFC, installs via InstallationProxy.
    pub async fn install_app_tvos(
        &self,
        app_path: &PathBuf,
        pin: String,
        cache_dir: PathBuf,
    ) -> Result<(), Error> {
        let (mut adapter, mut handshake) =
            self.establish_tvos_tunnel(pin, cache_dir).await?;

        // Upload IPA via AFC
        let mut afc = idevice::afc::AfcClient::connect_rsd(&mut adapter, &mut handshake)
            .await
            .map_err(|e| Error::Other(format!("AFC connection failed: {e}")))?;

        let ipa_data = tokio::fs::read(app_path).await?;
        let staging_path = "/PublicStaging/staging.ipa";

        afc.mk_dir("/PublicStaging")
            .await
            .ok(); // ignore if exists
        {
            let mut f = afc
                .open(staging_path, AfcFopenMode::Wr)
                .await
                .map_err(|e| Error::Other(format!("AFC open failed: {e}")))?;
            f.write_entire(&ipa_data)
                .await
                .map_err(|e| Error::Other(format!("AFC upload failed: {e}")))?;
        } // drop FileDescriptor before reusing afc

        // Install via InstallationProxy
        let mut instproxy =
            InstallationProxyClient::connect_rsd(&mut adapter, &mut handshake)
                .await
                .map_err(|e| Error::Other(format!("InstallationProxy connection failed: {e}")))?;

        instproxy
            .install(staging_path, None)
            .await
            .map_err(|e| Error::Other(format!("Installation failed: {e}")))?;

        // Cleanup staging file
        afc.remove(staging_path).await.ok();

        Ok(())
    }

    pub async fn install_app<F, Fut>(
        &self,
        app_path: &PathBuf,
        progress_callback: F,
    ) -> Result<(), Error>
    where
        F: FnMut(i32) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        if self.usbmuxd_device.is_none() {
            return Err(Error::Other("Device is not connected via USB".to_string()));
        }

        let provider = self.usbmuxd_device.clone().unwrap().to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            INSTALLATION_LABEL,
        );

        let callback = move |(progress, _): (u64, ())| {
            let mut cb = progress_callback.clone();
            async move {
                cb(progress as i32).await;
            }
        };

        let state = ();

        installation::install_package_with_callback(&provider, app_path, None, callback, state)
            .await?;

        Ok(())
    }
}

fn get_app_name_from_info(info: &Value) -> Option<String> {
    let dict = info.as_dictionary()?;
    dict.get("CFBundleDisplayName")
        .and_then(|value| value.as_string())
        .or_else(|| dict.get("CFBundleName").and_then(|value| value.as_string()))
        .or_else(|| {
            dict.get("CFBundleExecutable")
                .and_then(|value| value.as_string())
        })
        .map(|value| value.to_string())
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let conn = if self.pairing_address.is_some() || self.reconnect_address.is_some() {
            "WiFi (tvOS)"
        } else {
            match &self.usbmuxd_device {
                Some(device) => match &device.connection_type {
                    Connection::Usb => "USB",
                    Connection::Network(_) => "WiFi",
                    Connection::Unknown(_) => "Unknown",
                },
                None => "LOCAL",
            }
        };
        write!(f, "[{conn}] {}", self.name)
    }
}

pub async fn get_device_for_id(device_id: &str) -> Result<Device, Error> {
    let mut usbmuxd = UsbmuxdConnection::default().await?;
    let usbmuxd_device = usbmuxd
        .get_devices()
        .await?
        .into_iter()
        .find(|d| d.device_id.to_string() == device_id)
        .ok_or_else(|| Error::Other(format!("Device ID {device_id} not found")))?;

    Ok(Device::new(usbmuxd_device).await)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub async fn install_app_mac(app_path: &PathBuf) -> Result<(), Error> {
    use crate::copy_dir_recursively;
    use std::env;
    use tokio::fs;
    use uuid::Uuid;

    let stage_dir = env::temp_dir().join(format!(
        "plume_mac_stage_{}",
        Uuid::new_v4().to_string().to_uppercase()
    ));
    let app_name = app_path
        .file_name()
        .ok_or(Error::Other("Invalid app path".to_string()))?;

    // iOS Apps on macOS need to be wrapped in a special structure, more specifically
    // ```
    // LiveContainer.app
    // ├── WrappedBundle -> Wrapper/LiveContainer.app
    // └── Wrapper
    //     └── LiveContainer.app
    // ```
    // Then install to /Applications/...

    let outer_app_dir = stage_dir.join(app_name);
    let wrapper_dir = outer_app_dir.join("Wrapper");

    fs::create_dir_all(&wrapper_dir).await?;

    copy_dir_recursively(app_path, &wrapper_dir.join(app_name)).await?;

    let wrapped_bundle_path = outer_app_dir.join("WrappedBundle");
    fs::symlink(
        PathBuf::from("Wrapper").join(app_name),
        &wrapped_bundle_path,
    )
    .await?;

    let applications_dir = PathBuf::from("/Applications/iOS");
    fs::create_dir_all(&applications_dir).await?;

    let applications_dir = applications_dir.join(app_name);

    fs::remove_dir_all(&applications_dir).await.ok();

    fs::rename(&outer_app_dir, &applications_dir)
        .await
        .map_err(|_| Error::BundleFailedToCopy(applications_dir.to_string_lossy().into_owned()))?;

    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub async fn install_app_mac(_app_path: &PathBuf) -> Result<(), Error> {
    Ok(())
}
