pub mod mdns;

use std::time::Duration;

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

#[allow(async_fn_in_trait)]
pub trait DeviceDiscovery {
    async fn discover(&self, timeout: Duration) -> crate::Result<Vec<DiscoveredDevice>>;
}
