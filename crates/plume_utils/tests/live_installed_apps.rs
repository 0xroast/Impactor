//! Lists the apps installed on a paired Apple TV, over the tunnel. Ignored by default.
//!
//! Run with:
//!   cargo test -p plume_utils --test live_installed_apps -- --ignored --nocapture
//!
//! This is how an install is confirmed from the device's own point of view rather than from the
//! installer's progress bar: the app either appears in InstallationProxy's list or it does not.

use plume_utils::discovery::{
    DeviceDiscovery, PlatformDiscovery, REMOTEPAIRING_MANUAL_PAIRING_SERVICE, REMOTEPAIRING_SERVICE,
    group_network_devices,
};
use idevice::RsdService;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

fn app_cache_dir() -> PathBuf {
    let base = std::env::var("APPDATA").expect("APPDATA not set");
    PathBuf::from(base).join("PlumeImpactor")
}

#[tokio::test]
#[ignore = "requires a paired Apple TV on the local network"]
async fn live_installed_apps() {
    let _ = env_logger::builder().is_test(false).try_init();

    let cache_dir = app_cache_dir();
    let discovered = PlatformDiscovery::new()
        .discover(Duration::from_secs(6))
        .await
        .expect("discovery failed");
    for d in &discovered {
        if d.service_type == REMOTEPAIRING_SERVICE
            || d.service_type == REMOTEPAIRING_MANUAL_PAIRING_SERVICE
        {
            println!("found: {} {:?} {}", d.name, d.port, d.service_type);
        }
    }

    let device = group_network_devices(&discovered, &cache_dir)
        .into_iter()
        .next()
        .expect("no Apple TV grouped from discovery");
    println!("device: {} (id 0x{:08x})", device.name, device.device_id);

    let (mut adapter, mut handshake) = device
        .establish_tvos_tunnel(cache_dir)
        .await
        .expect("tunnel failed");
    println!("tunnel established");

    let mut inst =
        idevice::installation_proxy::InstallationProxyClient::connect_rsd(&mut adapter, &mut handshake)
            .await
            .expect("InstallationProxy connect failed");

    let apps: HashMap<String, plist::Value> = inst
        .get_apps(Some("User"), None::<Vec<String>>)
        .await
        .expect("Lookup failed");

    // Set PLUME_APP_ID to dump every key InstallationProxy reports for one app instead of the
    // one-line-per-app summary.
    if let Ok(target) = std::env::var("PLUME_APP_ID") {
        match apps.get(&target) {
            Some(plist::Value::Dictionary(d)) => {
                println!("\n=== {target} ===");
                let mut keys: Vec<_> = d.keys().cloned().collect();
                keys.sort();
                for k in keys {
                    println!("  {k} = {:?}", d.get(&k).unwrap());
                }
            }
            _ => {
                println!("\n{target} is not installed (or is not a User app).");
                println!("installed ids:");
                let mut ids: Vec<_> = apps.keys().cloned().collect();
                ids.sort();
                for i in ids {
                    println!("  {i}");
                }
            }
        }
        return;
    }

    println!("\n{} user app(s) installed:", apps.len());
    let mut names: Vec<_> = apps.keys().cloned().collect();
    names.sort();
    for id in &names {
        let ver = apps
            .get(id)
            .and_then(|v| v.as_dictionary())
            .and_then(|d| d.get("CFBundleShortVersionString"))
            .and_then(|v| v.as_string())
            .unwrap_or("?");
        println!("  {id}  v{ver}");
    }

    let target = "com.nuvio.app.tv";
    println!(
        "\n>>> {target} installed: {}",
        names.iter().any(|n| n == target)
    );
}
