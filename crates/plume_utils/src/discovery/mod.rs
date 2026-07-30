pub mod mdns;
#[cfg(windows)]
pub mod windows_dnssd;

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

/// Service for devices with no existing pairing, actively showing a pairing PIN on screen.
/// This is the only service that supports first-time SRP pair-setup.
pub const REMOTEPAIRING_MANUAL_PAIRING_SERVICE: &str = "_remotepairing-manual-pairing._tcp.local.";
/// Service for devices with an existing pairing. Reconnect (pair-verify) only - does not
/// support first-time pairing.
pub const REMOTEPAIRING_SERVICE: &str = "_remotepairing._tcp.local.";
/// Legacy lockdown-over-network service, advertised by devices already paired with this host.
pub const APPLE_MOBDEV2_SERVICE: &str = "_apple-mobdev2._tcp.local.";
/// Legacy service advertised by devices willing to accept a new lockdown pairing.
pub const APPLE_PAIRABLE_SERVICE: &str = "_apple-pairable._tcp.local.";

/// The service types scanned by every discovery backend, in a fixed order.
pub const SERVICE_TYPES: [&str; 4] = [
    APPLE_MOBDEV2_SERVICE,
    APPLE_PAIRABLE_SERVICE,
    REMOTEPAIRING_SERVICE,
    REMOTEPAIRING_MANUAL_PAIRING_SERVICE,
];

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceType {
    IPhone,
    IPad,
    AppleTV,
    AppleMac,
    Unknown,
}

impl DeviceType {
    pub fn from_device_class(device_class: &str) -> Self {
        match device_class {
            "iPhone" => DeviceType::IPhone,
            "iPad" => DeviceType::IPad,
            "AppleTV" => DeviceType::AppleTV,
            "Mac" => DeviceType::AppleMac,
            _ => DeviceType::Unknown,
        }
    }

    pub fn from_product_type(product_type: &str) -> Self {
        if product_type.starts_with("iPhone") {
            DeviceType::IPhone
        } else if product_type.starts_with("iPad") {
            DeviceType::IPad
        } else if product_type.starts_with("AppleTV") {
            DeviceType::AppleTV
        } else if product_type.starts_with("Mac") {
            DeviceType::AppleMac
        } else {
            DeviceType::Unknown
        }
    }
}

impl std::fmt::Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceType::IPhone => write!(f, "iPhone"),
            DeviceType::IPad => write!(f, "iPad"),
            DeviceType::AppleTV => write!(f, "Apple TV"),
            DeviceType::AppleMac => write!(f, "Mac"),
            DeviceType::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionType {
    USB,
    WiFi,
}

impl std::fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionType::USB => write!(f, "USB"),
            ConnectionType::WiFi => write!(f, "WiFi"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub name: String,
    pub udid: Option<String>,
    pub ip_address: Option<String>,
    pub port: Option<u16>,
    pub device_type: DeviceType,
    pub connection_type: ConnectionType,
    pub is_paired: bool,
    pub product_type: Option<String>,
    pub os_version: Option<String>,
    /// The mDNS service type this entry was resolved from (e.g.
    /// `_remotepairing-manual-pairing._tcp.local.`). Devices advertise different RPPairing
    /// services depending on whether they're actively showing a pairing PIN or already paired,
    /// and those services listen on different ports - callers must not treat entries from
    /// different service types as interchangeable.
    pub service_type: String,
}

// ---------------------------------------------------------------------------------------------
// Shared advertisement mapping
//
// Every backend funnels its raw advertisement facts through these helpers, so the two backends
// produce identical `DiscoveredDevice` values and remain interchangeable.
// ---------------------------------------------------------------------------------------------

/// Case-insensitive check that `s` ends with `suffix`.
///
/// Compares bytes rather than slicing the `&str`: instance names carry literal non-ASCII (a real
/// device on this network is named `Frankie<U+2019>s MacBook Pro`), and slicing at
/// `len - suffix.len()` panics whenever that offset lands inside a multi-byte character.
pub(crate) fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    let (haystack, needle) = (s.as_bytes(), suffix.as_bytes());
    needle.len() <= haystack.len()
        && haystack[haystack.len() - needle.len()..].eq_ignore_ascii_case(needle)
}

/// Extracts the instance label from a full DNS-SD instance name.
///
/// Instance names arrive with literal spaces and literal non-ASCII - there is no `\032` escaping
/// and no punycode - so the label is recovered by stripping the known `.<service type>` suffix,
/// never by splitting on `.`.
pub(crate) fn parse_instance_name(full_name: &str, service_type: &str) -> String {
    let full = full_name.trim_end_matches('.');
    let service = service_type.trim_end_matches('.');

    if !service.is_empty() {
        let suffix_len = service.len() + 1;
        if full.len() > suffix_len && ends_with_ignore_case(full, service) {
            let cut = full.len() - suffix_len;
            // A `.` at `cut` proves `cut` is a character boundary, so the slice below is safe.
            if full.as_bytes()[cut] == b'.' {
                return full[..cut].to_string();
            }
        }
    }
    full.to_string()
}

/// Strips the trailing dot and the `.local` label from a host name.
pub(crate) fn short_hostname(hostname: &str) -> &str {
    hostname.trim_end_matches('.').trim_end_matches(".local")
}

/// Key under which a discovered entry is stored.
///
/// The same physical device legitimately advertises several service types at once on *different*
/// ports (manual pairing versus an established pairing), so the service type is part of the key
/// and those entries must not collapse into one. The host part is lower-cased because DNS names
/// are case-insensitive and the same device may be advertised with differing case.
pub(crate) fn dedup_key(hostname: &str, instance_name: &str, service_type: &str) -> (String, String) {
    let host = short_hostname(hostname);
    let base = if host.is_empty() { instance_name } else { host };
    (base.to_ascii_lowercase(), service_type.to_string())
}

/// Looks up the first present key from `candidates`, ignoring empty values.
pub(crate) fn first_non_empty<'a>(
    props: &'a HashMap<String, String>,
    candidates: &[&str],
) -> Option<&'a str> {
    candidates
        .iter()
        .filter_map(|k| props.get(*k))
        .map(|v| v.as_str())
        .find(|v| !v.is_empty())
}

/// Builds a [`DiscoveredDevice`] from the facts one backend gathered about a single service
/// instance. Free of any backend-specific or FFI type, so both backends share it verbatim.
pub(crate) fn build_device(
    instance_name: &str,
    hostname: &str,
    service_type: &str,
    port: Option<u16>,
    addresses: &[IpAddr],
    props: &HashMap<String, String>,
) -> DiscoveredDevice {
    // Real Apple TV advertisements carry no `DeviceClass` and no `ProductType`: manual pairing
    // reports `model=AppleTV14,1` and companion-link reports `rpMd=AppleTV14,1`. Both must map to
    // `AppleTV`, because the pairing UI hard-filters on that.
    let device_type = if let Some(class) = first_non_empty(props, &["DeviceClass", "deviceClass"]) {
        DeviceType::from_device_class(class)
    } else if let Some(model) = first_non_empty(props, &["ProductType", "model", "rpMd"]) {
        DeviceType::from_product_type(model)
    } else {
        DeviceType::Unknown
    };

    let product_type =
        first_non_empty(props, &["ProductType", "model", "rpMd"]).map(str::to_string);
    let os_version = first_non_empty(props, &["OSVersion", "osVersion"]).map(str::to_string);
    let udid =
        first_non_empty(props, &["UniqueDeviceID", "udid", "identifier"]).map(str::to_string);

    // Derived from the hostname first. The name is what the UI uses to correlate a device's
    // several service-type entries with each other, so it must be identical across them: one
    // device advertises manual pairing under a friendly instance name ("Living Room") but an
    // established pairing under a bare UUID, and only the shared hostname yields the same value
    // for both. TXT and instance name are fallbacks for advertisements carrying no hostname.
    let name = {
        let from_host = short_hostname(hostname).replace('-', " ");
        if !from_host.is_empty() {
            from_host
        } else {
            first_non_empty(props, &["name", "Name"])
                .map(str::to_string)
                .unwrap_or_else(|| instance_name.to_string())
        }
    };

    DiscoveredDevice {
        name,
        udid,
        ip_address: addresses.first().map(|a| a.to_string()),
        port,
        device_type,
        connection_type: ConnectionType::WiFi,
        is_paired: service_type.contains("mobdev2"),
        product_type,
        os_version,
        service_type: service_type.to_string(),
    }
}

#[allow(async_fn_in_trait)]
pub trait DeviceDiscovery {
    async fn discover(&self, timeout: Duration) -> crate::Result<Vec<DiscoveredDevice>>;
}

/// Discovery backend appropriate for the host platform.
///
/// On Windows the native DNS-SD resolver (`dnsapi.dll`) is tried first, because the
/// raw-multicast-socket backend does not reliably receive responses there; if it errors or
/// finds nothing, the `mdns-sd` backend runs as a fallback. Every other platform uses
/// `mdns-sd` directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformDiscovery;

impl PlatformDiscovery {
    pub fn new() -> Self {
        Self
    }
}

impl DeviceDiscovery for PlatformDiscovery {
    async fn discover(&self, timeout: Duration) -> crate::Result<Vec<DiscoveredDevice>> {
        // The fallback runs inside the caller's budget, not in addition to it: the two backends
        // together never exceed `timeout`. A native scan that finds nothing returns after its
        // browse budget, which is what leaves the fallback a usable slice.
        #[cfg(windows)]
        let timeout = {
            let started = std::time::Instant::now();

            match windows_dnssd::WindowsDnsSdDiscovery::new()
                .discover(timeout)
                .await
            {
                Ok(devices) if !devices.is_empty() => return Ok(devices),
                Ok(_) => log::warn!(
                    "Windows DNS-SD discovery returned no devices, falling back to mdns-sd"
                ),
                Err(e) => {
                    log::warn!("Windows DNS-SD discovery failed ({e}), falling back to mdns-sd")
                }
            }

            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                log::warn!("No time left for the mdns-sd fallback within the requested timeout");
                return Ok(Vec::new());
            }
            remaining
        };

        mdns::MdnsDiscovery::new().discover(timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn instance_name_strips_service_suffix() {
        assert_eq!(
            parse_instance_name(
                "Living Room._remotepairing-manual-pairing._tcp.local",
                REMOTEPAIRING_MANUAL_PAIRING_SERVICE
            ),
            "Living Room"
        );
    }

    #[test]
    fn instance_name_handles_trailing_dot_on_both_sides() {
        assert_eq!(
            parse_instance_name("Apple TV._remotepairing._tcp.local.", REMOTEPAIRING_SERVICE),
            "Apple TV"
        );
        assert_eq!(
            parse_instance_name(
                "Apple TV._remotepairing._tcp.local",
                "_remotepairing._tcp.local"
            ),
            "Apple TV"
        );
    }

    #[test]
    fn instance_name_keeps_literal_non_ascii() {
        let full = "Frankie\u{2019}s MacBook Pro._companion-link._tcp.local";
        assert_eq!(
            parse_instance_name(full, "_companion-link._tcp.local."),
            "Frankie\u{2019}s MacBook Pro"
        );
    }

    #[test]
    fn instance_name_left_alone_when_suffix_absent() {
        assert_eq!(
            parse_instance_name("Living Room._other._tcp.local", REMOTEPAIRING_SERVICE),
            "Living Room._other._tcp.local"
        );
    }

    #[test]
    fn instance_name_does_not_split_a_multibyte_character() {
        // The suffix comparison must not slice the string at `len - suffix.len()`: here that
        // offset lands inside the leading U+2019, which panics on a `&str` slice.
        let name = "\u{2019}".to_string() + &"X".repeat(24);
        assert_eq!(parse_instance_name(&name, REMOTEPAIRING_SERVICE), name);

        // Same hazard with the multi-byte character straddling the boundary from the other side.
        for pad in 0..8 {
            let name = "A".repeat(pad) + "\u{2019}\u{2019}\u{2019}";
            assert_eq!(parse_instance_name(&name, "_x._tcp.local"), name);
        }
    }

    #[test]
    fn suffix_match_is_case_insensitive() {
        assert!(ends_with_ignore_case("Living Room._TCP.LOCAL", "_tcp.local"));
        assert!(ends_with_ignore_case("abc", "ABC"));
        assert!(!ends_with_ignore_case("abc", "abd"));
        assert!(!ends_with_ignore_case("ab", "abc"));
        // DNS names are case-insensitive, so a differently-cased service label still strips.
        assert_eq!(
            parse_instance_name(
                "Living Room._RemotePairing._TCP.local",
                REMOTEPAIRING_SERVICE
            ),
            "Living Room"
        );
    }

    #[test]
    fn first_non_empty_skips_present_but_empty_values() {
        let p = props(&[("ProductType", ""), ("model", "AppleTV14,1")]);
        assert_eq!(
            first_non_empty(&p, &["ProductType", "model"]),
            Some("AppleTV14,1")
        );
        assert_eq!(first_non_empty(&p, &["ProductType"]), None);
        assert_eq!(first_non_empty(&p, &["absent"]), None);
    }

    #[test]
    fn real_apple_tv_txt_maps_to_apple_tv() {
        // Manual pairing: no DeviceClass, no ProductType, model only. The pairing UI filters on
        // `device_type == AppleTV`, so anything else drops the device before the user sees it.
        let manual = props(&[("model", "AppleTV14,1")]);
        let d = build_device(
            "Living Room",
            "Living-Room.local.",
            REMOTEPAIRING_MANUAL_PAIRING_SERVICE,
            Some(49153),
            &[],
            &manual,
        );
        assert_eq!(d.device_type, DeviceType::AppleTV);
        assert_eq!(d.product_type.as_deref(), Some("AppleTV14,1"));

        // Companion-link style: rpMd only.
        let companion = props(&[("rpMd", "AppleTV14,1"), ("udid", "deadbeef")]);
        let d = build_device(
            "Living Room",
            "Living-Room.local.",
            REMOTEPAIRING_SERVICE,
            Some(49152),
            &[],
            &companion,
        );
        assert_eq!(d.device_type, DeviceType::AppleTV);
        assert_eq!(d.product_type.as_deref(), Some("AppleTV14,1"));
        assert_eq!(d.udid.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn mapping_prefers_device_class() {
        let p = props(&[
            ("DeviceClass", "AppleTV"),
            ("ProductType", "AppleTV11,1"),
            ("UniqueDeviceID", "abc123"),
            ("OSVersion", "17.4"),
            ("name", "Ignored"),
        ]);
        let d = build_device(
            "Living Room",
            "Living-Room.local.",
            REMOTEPAIRING_SERVICE,
            Some(49152),
            &["10.0.0.5".parse::<IpAddr>().unwrap()],
            &p,
        );
        assert_eq!(d.device_type, DeviceType::AppleTV);
        assert_eq!(d.product_type.as_deref(), Some("AppleTV11,1"));
        assert_eq!(d.os_version.as_deref(), Some("17.4"));
        assert_eq!(d.udid.as_deref(), Some("abc123"));
        assert_eq!(d.ip_address.as_deref(), Some("10.0.0.5"));
        assert_eq!(d.port, Some(49152));
        assert_eq!(d.connection_type, ConnectionType::WiFi);
        assert!(!d.is_paired);
        assert_eq!(d.service_type, REMOTEPAIRING_SERVICE);
    }

    #[test]
    fn mapping_name_prefers_hostname_over_txt_and_instance() {
        // Instance label and TXT name both differ from the host name; the host name still wins.
        let d = build_device(
            "A827F07B-2D1D-4D09-8E1E-5E37EE47A96C",
            "Living-Room.local.",
            REMOTEPAIRING_SERVICE,
            Some(1),
            &[],
            &props(&[("name", "Some Other Name")]),
        );
        assert_eq!(d.name, "Living Room");
    }

    #[test]
    fn mapping_name_falls_back_when_hostname_missing() {
        // No host name: the TXT name is next.
        let d = build_device(
            "instance-label",
            "",
            REMOTEPAIRING_SERVICE,
            Some(1),
            &[],
            &props(&[("name", "Txt Name")]),
        );
        assert_eq!(d.name, "Txt Name");

        // Neither host name nor TXT name: the parsed instance label is last.
        let d = build_device(
            "instance-label",
            "",
            REMOTEPAIRING_SERVICE,
            Some(1),
            &[],
            &props(&[]),
        );
        assert_eq!(d.name, "instance-label");
    }

    #[test]
    fn same_device_yields_identical_name_across_service_types() {
        // An Apple TV advertises manual pairing under a friendly instance name carrying a TXT
        // `name`, and an established pairing under a bare UUID with no TXT `name` at all (both
        // shapes taken from a packet capture of a real pairing session). The UI correlates a
        // device's service-type entries by name, so both must resolve to the same name while
        // remaining separate entries with their own ports.
        let manual = build_device(
            "Living Room",
            "Living-Room.local.",
            REMOTEPAIRING_MANUAL_PAIRING_SERVICE,
            Some(62782),
            &[],
            &props(&[("name", "Living Room"), ("model", "AppleTV14,1")]),
        );
        let reconnect = build_device(
            "A827F07B-2D1D-4D09-8E1E-5E37EE47A96C",
            "Living-Room.local.",
            REMOTEPAIRING_SERVICE,
            Some(49152),
            &[],
            &props(&[("identifier", "73B8BE56-3881-4145-BF61-EFB7BBAEC98F")]),
        );

        assert_eq!(manual.name, "Living Room");
        assert_eq!(manual.name, reconnect.name);
        assert_ne!(manual.service_type, reconnect.service_type);
        assert_eq!(manual.port, Some(62782));
        assert_eq!(reconnect.port, Some(49152));
    }

    #[test]
    fn mapping_marks_mobdev2_as_paired() {
        let d = build_device("x", "", APPLE_MOBDEV2_SERVICE, Some(62078), &[], &props(&[]));
        assert!(d.is_paired);
        assert_eq!(d.device_type, DeviceType::Unknown);
        assert_eq!(d.product_type, None);
    }

    #[test]
    fn mapping_udid_precedence() {
        let p = props(&[("udid", "second"), ("identifier", "third")]);
        assert_eq!(
            build_device("x", "", REMOTEPAIRING_SERVICE, Some(1), &[], &p)
                .udid
                .as_deref(),
            Some("second")
        );
        let p = props(&[("identifier", "third")]);
        assert_eq!(
            build_device("x", "", REMOTEPAIRING_SERVICE, Some(1), &[], &p)
                .udid
                .as_deref(),
            Some("third")
        );
    }

    #[test]
    fn dedup_key_normalizes_case_and_falls_back_to_instance() {
        assert_eq!(
            dedup_key("Living-Room.local.", "Living Room", REMOTEPAIRING_SERVICE),
            dedup_key("living-room.local", "Living Room", REMOTEPAIRING_SERVICE)
        );
        assert_eq!(
            dedup_key("", "Living Room", REMOTEPAIRING_SERVICE),
            ("living room".to_string(), REMOTEPAIRING_SERVICE.to_string())
        );
    }

    #[test]
    fn same_device_under_two_service_types_is_not_collapsed() {
        let p = props(&[("model", "AppleTV14,1")]);
        let mut devices: HashMap<(String, String), DiscoveredDevice> = HashMap::new();

        for (service, port) in [
            (REMOTEPAIRING_SERVICE, 49152u16),
            (REMOTEPAIRING_MANUAL_PAIRING_SERVICE, 49153u16),
        ] {
            let device = build_device(
                "Living Room",
                "Living-Room.local.",
                service,
                Some(port),
                &[],
                &p,
            );
            devices.insert(
                dedup_key("Living-Room.local.", "Living Room", service),
                device,
            );
        }

        assert_eq!(devices.len(), 2);
        let mut ports: Vec<u16> = devices.values().filter_map(|d| d.port).collect();
        ports.sort_unstable();
        assert_eq!(ports, vec![49152, 49153]);
        assert!(devices.values().all(|d| d.name == "Living Room"));
    }
}
