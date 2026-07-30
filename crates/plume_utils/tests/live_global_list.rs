//! Live probe of the pipeline the global device list is fed from. Ignored by default.
//!
//! Run with:
//!   cargo test -p plume_utils --test live_global_list -- --ignored --nocapture
//!
//! Mirrors what `subscriptions::network_device_listener` does each scan: discover, group into
//! `Device` values, then adopt the real UDID over a tunnel for any device that already has a
//! cached pairing file. A device reaching the list without a real UDID cannot be signed for, so
//! the UDID is the thing this probe exists to check.

use plume_utils::discovery::{DeviceDiscovery, PlatformDiscovery, group_network_devices};
use std::path::PathBuf;
use std::time::Duration;

fn app_cache_dir() -> PathBuf {
    let base = std::env::var("APPDATA").expect("APPDATA not set");
    PathBuf::from(base).join("PlumeImpactor")
}

#[tokio::test]
#[ignore = "requires an Apple TV on the local network"]
async fn live_global_list() {
    let _ = env_logger::builder().is_test(false).try_init();

    let cache_dir = app_cache_dir();

    let discovered = PlatformDiscovery::new()
        .discover(Duration::from_secs(6))
        .await
        .expect("discovery failed");
    println!("discovered {} raw entries:", discovered.len());
    for d in &discovered {
        println!(
            "  {} type={:?} port={:?} service={}",
            d.name, d.device_type, d.port, d.service_type
        );
    }

    let mut devices = group_network_devices(&discovered, &cache_dir);
    println!("\ngrouped into {} device(s):", devices.len());

    for device in &mut devices {
        println!(
            "  name={} device_id=0x{:08x} pairing={:?} reconnect={:?}",
            device.name, device.device_id, device.pairing_address, device.reconnect_address
        );
        println!("    pairing_identity={:?}", device.pairing_identity);
        println!(
            "    cached pairing file present: {}",
            device.has_cached_pairing_file(&cache_dir)
        );
        println!("    udid before enrichment: {:?}", device.udid);

        if device.has_cached_pairing_file(&cache_dir) {
            match device.fetch_tvos_info(cache_dir.clone()).await {
                Ok(info) => {
                    device.apply_tvos_info(&info);
                    println!("    enriched: {info:?}");
                }
                Err(e) => println!("    ENRICHMENT FAILED: {e}"),
            }
        } else {
            println!("    skipped enrichment: no cached pairing file");
        }

        println!("    udid after enrichment: {:?}", device.udid);
        // An empty udid is what the install guard rejects, so this is the pass/fail line.
        println!(
            "    INSTALLABLE (udid non-empty): {}",
            !device.udid.is_empty()
        );
    }

    assert!(
        !devices.is_empty(),
        "no Apple TV grouped from discovery; is one on the network?"
    );
}
