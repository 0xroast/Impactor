use super::{ConnectionType, DeviceDiscovery, DiscoveredDevice, DeviceType};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::time::Duration;

/// Service for devices with no existing pairing, actively showing a pairing PIN on screen.
/// This is the only service that supports first-time SRP pair-setup.
pub const REMOTEPAIRING_MANUAL_PAIRING_SERVICE: &str = "_remotepairing-manual-pairing._tcp.local.";
/// Service for devices with an existing pairing. Reconnect (pair-verify) only - does not
/// support first-time pairing.
pub const REMOTEPAIRING_SERVICE: &str = "_remotepairing._tcp.local.";
const APPLE_MOBDEV2_SERVICE: &str = "_apple-mobdev2._tcp.local.";
const APPLE_PAIRABLE_SERVICE: &str = "_apple-pairable._tcp.local.";

pub struct MdnsDiscovery {
    service_types: Vec<String>,
}

impl MdnsDiscovery {
    pub fn new() -> Self {
        Self {
            service_types: vec![
                APPLE_MOBDEV2_SERVICE.to_string(),
                APPLE_PAIRABLE_SERVICE.to_string(),
                REMOTEPAIRING_SERVICE.to_string(),
                REMOTEPAIRING_MANUAL_PAIRING_SERVICE.to_string(),
            ],
        }
    }
}

impl Default for MdnsDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceDiscovery for MdnsDiscovery {
    async fn discover(&self, timeout: Duration) -> crate::Result<Vec<DiscoveredDevice>> {
        let mdns = ServiceDaemon::new().map_err(|e| {
            crate::Error::Other(format!("Failed to create mDNS daemon: {e}"))
        })?;

        // Browse all service types simultaneously
        let mut receivers = Vec::new();
        for service_type in &self.service_types {
            match mdns.browse(service_type) {
                Ok(receiver) => receivers.push((service_type.clone(), receiver)),
                Err(e) => {
                    log::warn!("Failed to browse {service_type}: {e}");
                }
            }
        }

        let service_types = self.service_types.clone();
        let discovered = tokio::task::spawn_blocking(move || {
            // Keyed by (hostname, service_type): the same physical device can advertise
            // multiple RPPairing service types at once with different ports (e.g. manual
            // pairing vs. an already-established pairing), and those are not interchangeable.
            let mut discovered_devices: HashMap<(String, String), DiscoveredDevice> =
                HashMap::new();
            let deadline = std::time::Instant::now() + timeout;

            // Poll all receivers until timeout
            while std::time::Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                // Short poll interval so we check all receivers fairly
                let poll_time = remaining.min(Duration::from_millis(200));
                let mut got_event = false;

                for (service_type, receiver) in &receivers {
                    match receiver.recv_timeout(poll_time) {
                        Ok(ServiceEvent::ServiceResolved(info)) => {
                            got_event = true;
                            let properties: HashMap<String, String> = info
                                .get_properties()
                                .iter()
                                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                                .collect();

                            let device_class = properties
                                .get("DeviceClass")
                                .or_else(|| properties.get("deviceClass"))
                                .map(|s| s.as_str())
                                .unwrap_or("");

                            let device_type = if !device_class.is_empty() {
                                DeviceType::from_device_class(device_class)
                            } else if let Some(pt) = properties.get("ProductType") {
                                DeviceType::from_product_type(pt)
                            } else {
                                DeviceType::Unknown
                            };

                            let product_type = properties
                                .get("ProductType")
                                .or_else(|| properties.get("model"))
                                .cloned();
                            let os_version = properties
                                .get("OSVersion")
                                .or_else(|| properties.get("osVersion"))
                                .cloned();

                            let hostname = info.get_hostname();
                            let name = {
                                let friendly = hostname
                                    .trim_end_matches('.')
                                    .trim_end_matches(".local")
                                    .replace('-', " ");
                                if friendly.is_empty() {
                                    properties
                                        .get("Name")
                                        .or_else(|| properties.get("name"))
                                        .cloned()
                                        .unwrap_or_else(|| info.get_fullname().to_string())
                                } else {
                                    friendly
                                }
                            };

                            let udid = properties
                                .get("UniqueDeviceID")
                                .or_else(|| properties.get("udid"))
                                .or_else(|| properties.get("identifier"))
                                .cloned();

                            let addresses = info.get_addresses();
                            let ip_address =
                                addresses.iter().next().map(|addr| addr.to_string());
                            let port = Some(info.get_port());
                            let is_paired = service_type.contains("mobdev2");

                            log::debug!(
                                "mDNS resolved: hostname={} service={} ip={:?} port={:?}",
                                hostname,
                                service_type,
                                ip_address,
                                port
                            );

                            let key = (
                                hostname
                                    .trim_end_matches('.')
                                    .trim_end_matches(".local")
                                    .to_string(),
                                service_type.clone(),
                            );

                            discovered_devices.insert(
                                key,
                                DiscoveredDevice {
                                    name,
                                    udid,
                                    ip_address,
                                    port,
                                    device_type,
                                    connection_type: ConnectionType::WiFi,
                                    is_paired,
                                    product_type,
                                    os_version,
                                    service_type: service_type.clone(),
                                },
                            );
                        }
                        Ok(_) => {
                            got_event = true;
                        }
                        Err(_) => {} // timeout on this receiver, try next
                    }
                }

                if !got_event && poll_time == remaining {
                    break; // final poll expired with no events
                }
            }

            // Stop all browses before dropping to avoid "closed channel" errors
            for stype in &service_types {
                let _ = mdns.stop_browse(stype);
            }
            let _ = mdns.shutdown();

            discovered_devices
        })
        .await
        .map_err(|e| crate::Error::Other(format!("mDNS scan task failed: {e}")))?;

        Ok(discovered.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_type_from_class() {
        assert_eq!(DeviceType::from_device_class("AppleTV"), DeviceType::AppleTV);
        assert_eq!(DeviceType::from_device_class("iPhone"), DeviceType::IPhone);
    }

    #[test]
    fn test_device_type_from_product() {
        assert_eq!(
            DeviceType::from_product_type("AppleTV11,1"),
            DeviceType::AppleTV
        );
        assert_eq!(
            DeviceType::from_product_type("iPhone15,2"),
            DeviceType::IPhone
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_mdns_discovery() {
        let discovery = MdnsDiscovery::new();
        let devices = discovery.discover(Duration::from_secs(5)).await.unwrap();
        println!("Discovered {} devices:", devices.len());
        for device in &devices {
            println!("  - {} ({:?})", device.name, device.device_type);
        }
    }
}
