//! Live pairing probe against a real Apple TV. Ignored by default: it needs an Apple TV on the
//! local network sitting on Settings > Remotes and Devices > Remote App and Devices, showing
//! "Waiting to Pair...".
//!
//! Run with:
//!   cargo test -p plume_utils --test live_pairing -- --ignored --nocapture
//!
//! The PIN is read from the APPLETV_PIN environment variable when set. Without it the probe
//! sends a placeholder, which still drives the exchange far enough to show how far the
//! handshake gets and whether the device asks for a PIN at all (the point at which the code
//! appears on screen).

use plume_utils::Device;
use plume_utils::discovery::{
    DeviceDiscovery, DeviceType, PlatformDiscovery, REMOTEPAIRING_MANUAL_PAIRING_SERVICE,
};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires an Apple TV in manual pairing mode on the local network"]
async fn live_pair() {
    let _ = env_logger::builder().is_test(false).try_init();

    let devices = PlatformDiscovery::new()
        .discover(Duration::from_secs(6))
        .await
        .expect("discovery failed");

    for d in &devices {
        println!(
            "found: {} type={:?} ip={:?} port={:?} service={} udid={:?}",
            d.name, d.device_type, d.ip_address, d.port, d.service_type, d.udid
        );
    }

    let target = devices
        .iter()
        .find(|d| {
            d.device_type == DeviceType::AppleTV
                && d.service_type == REMOTEPAIRING_MANUAL_PAIRING_SERVICE
        })
        .expect("no Apple TV advertising the manual-pairing service; put it on the pairing screen");

    let ip: std::net::IpAddr = target
        .ip_address
        .as_deref()
        .expect("no IP")
        .parse()
        .expect("bad IP");
    let port = target.port.expect("no port");
    let hostname = target
        .udid
        .clone()
        .unwrap_or_else(|| target.name.replace(' ', "-"));

    // When no PIN is supplied up front the probe holds the pair-setup session open and watches
    // a drop file for one. The device only shows its code while the session is open, so holding
    // here is what keeps the code on screen long enough to be read and handed back.
    let pin_file = std::env::temp_dir().join("appletv_pin.txt");
    let _ = std::fs::remove_file(&pin_file);
    let preset = std::env::var("APPLETV_PIN").ok();
    println!("pairing with {}:{}", ip, port);
    match &preset {
        Some(p) => println!("using preset PIN {p}"),
        None => println!("no preset PIN; will wait for one at {}", pin_file.display()),
    }

    // A scratch cache directory keeps the probe from reusing or clobbering the real pairing
    // file, and guarantees the cached-file early return is not taken.
    let cache_dir = std::env::temp_dir().join(format!(
        "plume_live_pair_{}",
        uuid::Uuid::new_v4().simple()
    ));

    let device = Device::new_tvos(
        target.name.clone(),
        hostname,
        ip,
        Some(port),
        None,
        cache_dir.clone(),
    );

    let provider = || {
        let preset = preset.clone();
        let pin_file = pin_file.clone();
        async move {
            if let Some(p) = preset {
                return p;
            }
            println!("=== LOOK AT THE APPLE TV NOW - its PIN is on screen ===");
            println!("=== waiting up to 180s for the code ===");
            for _ in 0..360 {
                if let Ok(s) = std::fs::read_to_string(&pin_file) {
                    let code: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
                    if code.len() == 6 {
                        println!("got PIN {code}, continuing handshake");
                        return code;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            println!("no PIN supplied in time; sending placeholder");
            "000000".to_string()
        }
    };

    match device.pair_tvos(provider, cache_dir).await {
        Ok(_) => println!("PAIRING SUCCEEDED"),
        Err(e) => println!("PAIRING FAILED: {e}"),
    }
}
