//! Live TLS-PSK tunnel probe against an already-paired Apple TV. Ignored by default.
//!
//! Run with:
//!   cargo test -p plume_utils --test live_tunnel -- --ignored --nocapture
//!
//! This exercises the reconnect half of the tvOS path - pair-verify against the cached pairing
//! file, TLS-PSK tunnel, software IP stack, RSD handshake - and prints the services the device
//! offers through the tunnel. It needs no IPA: reaching a populated service list is what proves
//! the transport works, and installation is layered on top of exactly that.

use plume_utils::Device;
use plume_utils::discovery::{
    DeviceDiscovery, DeviceType, PlatformDiscovery, REMOTEPAIRING_MANUAL_PAIRING_SERVICE,
    REMOTEPAIRING_SERVICE,
};
use std::path::PathBuf;
use std::time::Duration;

/// The directory the GUI stores pairing files in, so this probe reuses a pairing made there.
fn app_cache_dir() -> PathBuf {
    let base = std::env::var("APPDATA").expect("APPDATA not set");
    PathBuf::from(base).join("PlumeImpactor")
}

#[tokio::test]
#[ignore = "requires an Apple TV already paired with this host"]
async fn live_tunnel() {
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

    // A paired Apple TV that is not on its pairing screen advertises only the reconnect
    // service, whose TXT record carries no model, so it types as Unknown. Selecting on the
    // service type rather than the device type is what finds it in that state.
    let tv_name = devices
        .iter()
        .find(|d| d.device_type == DeviceType::AppleTV)
        .or_else(|| {
            devices
                .iter()
                .find(|d| d.service_type == REMOTEPAIRING_SERVICE)
        })
        .map(|d| d.name.clone())
        .expect("no device advertising an RPPairing service found");

    let entry_for = |service: &str| {
        devices
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(&tv_name) && d.service_type == service)
    };

    let reconnect = entry_for(REMOTEPAIRING_SERVICE);
    let manual = entry_for(REMOTEPAIRING_MANUAL_PAIRING_SERVICE);
    let any = reconnect.or(manual).expect("no usable service entry");

    let ip: std::net::IpAddr = any
        .ip_address
        .as_deref()
        .expect("no IP")
        .parse()
        .expect("bad IP");
    // The advertised identifier differs between the two service types for the same device, so
    // the pairing file is keyed on the name, which does not.
    let udid = tv_name.replace(' ', "-");

    let cache_dir = app_cache_dir();
    let cache_file = cache_dir.join(format!("plume_{udid}.plist"));
    println!("\nudid: {udid}");
    println!(
        "cached pairing file: {} (exists: {})",
        cache_file.display(),
        cache_file.exists()
    );
    println!(
        "reconnect port: {:?}   manual-pairing port: {:?}",
        reconnect.and_then(|d| d.port),
        manual.and_then(|d| d.port)
    );

    let device = Device::new_tvos(
        tv_name.clone(),
        udid,
        ip,
        manual.and_then(|d| d.port),
        reconnect.and_then(|d| d.port),
        cache_dir.clone(),
    );

    println!("\nestablishing tunnel...");
    match device.establish_tvos_tunnel(cache_dir).await {
        Ok((_adapter, handshake)) => {
            println!("TUNNEL ESTABLISHED");
            println!("rsd uuid: {}", handshake.uuid);
            // Signing registers a device with Apple by its real UDID, which mDNS does not
            // advertise, so whether these properties carry one decides whether a network Apple
            // TV can go through the normal signing pipeline at all.
            println!("--- RSD properties ({}) ---", handshake.properties.len());
            let mut props: Vec<_> = handshake.properties.iter().collect();
            props.sort_by_key(|(k, _)| k.to_string());
            for (k, v) in props {
                println!("  {k} = {v:?}");
            }
            println!("--- end properties ---");
            let mut names: Vec<_> = handshake.services.keys().cloned().collect();
            names.sort();
            println!("{} RSD services discovered:", names.len());
            for n in &names {
                println!("  {n}");
            }
            for probe in [
                "com.apple.afc.shim.remote",
                "com.apple.mobile.installation_proxy.shim.remote",
            ] {
                println!(
                    "  needed by install -> {probe}: {}",
                    if handshake.services.contains_key(probe) {
                        "PRESENT"
                    } else {
                        "MISSING"
                    }
                );
            }
        }
        Err(e) => println!("TUNNEL FAILED: {e}"),
    }
}
