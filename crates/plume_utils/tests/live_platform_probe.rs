//! Read-only probe of how Apple's developer API scopes devices by platform. Ignored by default.
//!
//! Run with:
//!   cargo test -p plume_core --test live_platform_probe -- --ignored --nocapture
//!
//! Two reference implementations disagree about how a tvOS device is addressed. Sideloader
//! switches the URL segment to `tvos/`; Sideloadly keeps `ios/` and instead sends
//! `DTDK_Platform`/`subPlatform` request parameters. This probe asks Apple directly.
//!
//! Every call here is `listDevices`, which only reads. Nothing is registered, no certificate is
//! created or revoked, and no developer-account quota is consumed.

use plist::{Dictionary, Value};
use plume_core::developer::DeveloperSession;
use plume_core::AnisetteConfiguration;
use std::path::PathBuf;

fn app_data_dir() -> PathBuf {
    let base = std::env::var("APPDATA").expect("APPDATA not set");
    PathBuf::from(base).join("PlumeImpactor")
}

fn summarize(label: &str, result: Result<Dictionary, plume_core::Error>) {
    match result {
        Ok(dict) => {
            let code = dict.get("resultCode").and_then(|v| v.as_signed_integer());
            let msg = dict
                .get("userString")
                .and_then(|v| v.as_string())
                .unwrap_or("");
            let devices = dict.get("devices").and_then(|v| v.as_array());
            println!("  [{label}] resultCode={code:?} userString={msg:?}");
            match devices {
                Some(list) => {
                    println!("  [{label}] devices returned: {}", list.len());
                    for d in list {
                        if let Some(d) = d.as_dictionary() {
                            let name = d.get("name").and_then(|v| v.as_string()).unwrap_or("?");
                            let num = d
                                .get("deviceNumber")
                                .and_then(|v| v.as_string())
                                .unwrap_or("?");
                            let class = d
                                .get("deviceClass")
                                .and_then(|v| v.as_string())
                                .unwrap_or("(none)");
                            let plat = d
                                .get("devicePlatform")
                                .and_then(|v| v.as_string())
                                .unwrap_or("(none)");
                            println!("      - {name} | {num} | class={class} platform={plat}");
                        }
                    }
                }
                None => println!("  [{label}] no 'devices' key in response"),
            }
        }
        Err(e) => println!("  [{label}] ERROR: {e}"),
    }
}

#[tokio::test]
#[ignore = "requires a signed-in Apple account in the app's data directory"]
async fn live_platform_probe() {
    let _ = env_logger::builder().is_test(false).try_init();

    // The store is the accounts file itself, not the directory holding it.
    let store = plume_store::AccountStore::load(&Some(app_data_dir().join("accounts.json")))
        .await
        .expect("could not load account store");
    let (email, account) = store
        .accounts()
        .iter()
        .next()
        .expect("no account signed in; sign in through the app first");
    println!("using account: {email}");

    let session = DeveloperSession::new(
        account.adsid().clone(),
        account.xcode_gs_token().clone(),
        AnisetteConfiguration::default().set_configuration_path(app_data_dir()),
    )
    .await
    .expect("could not establish developer session");

    let teams = session.qh_list_teams().await.expect("listTeams failed");
    let team_id = if account.team_id().is_empty() {
        teams.teams.first().expect("no teams").team_id.clone()
    } else {
        account.team_id().clone()
    };
    println!("team: {team_id}\n");

    let ios_url =
        "https://developerservices2.apple.com/services/QH65B2/ios/listDevices.action".to_string();
    let tvos_url =
        "https://developerservices2.apple.com/services/QH65B2/tvos/listDevices.action".to_string();

    let base_body = || {
        let mut b = Dictionary::new();
        b.insert("teamId".into(), Value::String(team_id.clone()));
        b
    };
    let platform_body = || {
        let mut b = base_body();
        b.insert("DTDK_Platform".into(), Value::String("tvos".into()));
        b.insert("subPlatform".into(), Value::String("tvOS".into()));
        b
    };

    println!("A) ios/ URL, no platform params  (what we send today)");
    summarize("A", session.qh_send_request(&ios_url, Some(base_body())).await);

    println!("\nB) ios/ URL + DTDK_Platform/subPlatform  (Sideloadly's mechanism)");
    summarize(
        "B",
        session
            .qh_send_request(&ios_url, Some(platform_body()))
            .await,
    );

    println!("\nC) tvos/ URL, no platform params  (Sideloader's mechanism)");
    summarize(
        "C",
        session.qh_send_request(&tvos_url, Some(base_body())).await,
    );

    println!("\nD) tvos/ URL + platform params  (both)");
    summarize(
        "D",
        session
            .qh_send_request(&tvos_url, Some(platform_body()))
            .await,
    );

    // listDevices returns the Apple TV regardless of the platform params, so it cannot
    // discriminate between the two mechanisms. The call that actually failed is the profile
    // download, so probe that - reusing an App ID that already exists, creating nothing.
    let list_appids =
        "https://developerservices2.apple.com/services/QH65B2/ios/listAppIds.action".to_string();
    println!("\nE) existing App IDs (read-only)");
    let app_id = match session.qh_send_request(&list_appids, Some(base_body())).await {
        Ok(dict) => {
            let ids = dict.get("appIds").and_then(|v| v.as_array());
            let n = ids.map(|l| l.len()).unwrap_or(0);
            println!("  [E] appIds returned: {n}");
            ids.and_then(|l| l.first())
                .and_then(|v| v.as_dictionary())
                .and_then(|d| {
                    let id = d.get("appIdId")?.as_string()?.to_string();
                    let ident = d
                        .get("identifier")
                        .and_then(|v| v.as_string())
                        .unwrap_or("?");
                    println!("  [E] using appIdId={id} identifier={ident}");
                    Some(id)
                })
        }
        Err(e) => {
            println!("  [E] ERROR: {e}");
            None
        }
    };

    let Some(app_id) = app_id else {
        println!(
            "\nNo App ID exists yet, so the profile download cannot be probed without creating \
             one (which would consume one of the account's ten). Stopping here."
        );
        return;
    };

    let profile_url =
        "https://developerservices2.apple.com/services/QH65B2/ios/downloadTeamProvisioningProfile.action"
            .to_string();
    let profile_body = |with_platform: bool| {
        let mut b = Dictionary::new();
        b.insert("teamId".into(), Value::String(team_id.clone()));
        b.insert("appIdId".into(), Value::String(app_id.clone()));
        if with_platform {
            b.insert("DTDK_Platform".into(), Value::String("tvos".into()));
            b.insert("subPlatform".into(), Value::String("tvOS".into()));
        }
        b
    };
    let show = |label: &str, r: Result<Dictionary, plume_core::Error>| match r {
        Ok(dict) => {
            let code = dict.get("resultCode").and_then(|v| v.as_signed_integer());
            let msg = dict
                .get("userString")
                .and_then(|v| v.as_string())
                .unwrap_or("");
            let got = dict.get("provisioningProfile").is_some();
            println!("  [{label}] resultCode={code:?} profile_returned={got} msg={msg:?}");
        }
        Err(e) => println!("  [{label}] ERROR: {e}"),
    };

    println!("\nF) downloadTeamProvisioningProfile WITHOUT platform params (reproduces the failure)");
    show(
        "F",
        session
            .qh_send_request(&profile_url, Some(profile_body(false)))
            .await,
    );

    println!("\nG) downloadTeamProvisioningProfile WITH DTDK_Platform/subPlatform");
    show(
        "G",
        session
            .qh_send_request(&profile_url, Some(profile_body(true)))
            .await,
    );
}
