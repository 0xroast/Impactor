use super::{ConnectionType, DeviceDiscovery, DiscoveredDevice, DeviceType};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::time::Duration;

pub struct MdnsDiscovery {
    service_types: Vec<String>,
}

impl MdnsDiscovery {
    pub fn new() -> Self {
        Self {
            service_types: vec![
                "_apple-mobdev2._tcp.local.".to_string(),
                "_apple-pairable._tcp.local.".to_string(),
                "_remotepairing._tcp.local.".to_string(),
                "_remotepairing-manual-pairing._tcp.local.".to_string(),
            ],
        }
    }

    fn parse_service_properties(
        &self,
        properties: &HashMap<String, String>,
        _service_type: &str,
    ) -> (DeviceType, Option<String>, Option<String>) {
        let device_class = properties
            .get("DeviceClass")
            .or_else(|| properties.get("deviceClass"))
            .map(|s| s.as_str())
            .unwrap_or("");

        let device_type = if !device_class.is_empty() {
            DeviceType::from_device_class(device_class)
        } else if let Some(product_type) = properties.get("ProductType") {
            DeviceType::from_product_type(product_type)
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

        (device_type, product_type, os_version)
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

        let mut discovered_devices: HashMap<String, DiscoveredDevice> = HashMap::new();

        for service_type in &self.service_types {
            let receiver = mdns.browse(service_type).map_err(|e| {
                crate::Error::Other(format!("Failed to browse service {service_type}: {e}"))
            })?;

            let deadline = std::time::Instant::now() + timeout;

            while std::time::Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());

                let recv_result = tokio::task::spawn_blocking({
                    let receiver = receiver.clone();
                    move || receiver.recv_timeout(remaining)
                })
                .await;

                match recv_result {
                    Ok(Ok(event)) => match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let properties: HashMap<String, String> = info
                                .get_properties()
                                .iter()
                                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                                .collect();

                            let (device_type, product_type, os_version) =
                                self.parse_service_properties(&properties, service_type);

                            let hostname = info.get_hostname();
                            let name = {
                                let friendly_name = hostname
                                    .trim_end_matches('.')
                                    .trim_end_matches(".local")
                                    .replace('-', " ");

                                if friendly_name.is_empty() {
                                    properties
                                        .get("Name")
                                        .or_else(|| properties.get("name"))
                                        .cloned()
                                        .unwrap_or_else(|| info.get_fullname().to_string())
                                } else {
                                    friendly_name
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

                            let key = hostname
                                .trim_end_matches('.')
                                .trim_end_matches(".local")
                                .to_string();

                            let device = DiscoveredDevice {
                                name,
                                udid,
                                ip_address,
                                port,
                                device_type,
                                connection_type: ConnectionType::WiFi,
                                is_paired,
                                product_type,
                                os_version,
                            };

                            discovered_devices.insert(key, device);
                        }
                        ServiceEvent::SearchStopped(_) => break,
                        _ => {}
                    },
                    Ok(Err(_)) => break,
                    Err(_) => break,
                }
            }
        }

        Ok(discovered_devices.into_values().collect())
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
