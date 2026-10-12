use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use plume_core::{
    AnisetteConfiguration, CertificateIdentity, MobileProvision,
    developer::{DeveloperPlatform, DeveloperSession},
};
use plume_store::{AccountStore, RefreshDevice};
use plume_utils::{Bundle, Device, Signer, SignerMode, SignerOptions};

use crate::defaults::get_data_path;

pub type ConnectedDevices = Arc<Mutex<HashMap<String, Device>>>;

const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 3);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60 * 20);
/// How long a manual refresh waits for the user to answer a certificate reset prompt. The wait
/// blocks its thread, so the refresh timeout cannot end it, and the device stays locked meanwhile.
const MANUAL_RESET_PROMPT_TIMEOUT: Duration = Duration::from_secs(60 * 5);
/// Transfer archives older than this belong to a refresh that was abandoned.
const STALE_ARCHIVE_AGE: Duration = Duration::from_secs(60 * 60);

/// How long before a profile expires its app is scheduled for a refresh.
const REFRESH_LEAD_DAYS: i64 = 3;
/// Once an app is this close to expiring, a failed refresh is escalated to the user.
const URGENT_WINDOW_HOURS: i64 = 24;
const RETRY_BASE_MINUTES: i64 = 5;
const RETRY_MAX_MINUTES: i64 = 360;

/// Devices with a refresh in flight. Shared by every `RefreshDaemon` so the background loop and a
/// tray-initiated refresh cannot work on the same device at once.
static ACTIVE_TASKS: LazyLock<Arc<Mutex<HashSet<String>>>> = LazyLock::new(Default::default);
/// Consecutive failures per app, keyed by `app_key`.
static FAILURES: LazyLock<Mutex<HashMap<String, FailureState>>> = LazyLock::new(Default::default);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTrigger {
    /// The background loop. Unattended, so it never asks the user anything.
    Scheduled,
    /// The user asked for it from the tray; prompts are allowed and every failure is reported.
    Manual,
}

#[derive(Debug, Clone, Default)]
struct FailureState {
    count: u32,
    retry_at: Option<DateTime<Utc>>,
    urgent_sent: bool,
}

impl FailureState {
    fn record(&mut self, now: DateTime<Utc>) {
        self.count += 1;
        self.retry_at = Some(now + retry_delay(self.count));
    }

    fn ready(&self, now: DateTime<Utc>) -> bool {
        self.retry_at.is_none_or(|retry_at| retry_at <= now)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureNotice {
    First,
    Urgent,
}

/// 5 minutes after the first failure, doubling per failure up to 6 hours.
fn retry_delay(failures: u32) -> chrono::Duration {
    let doublings = failures.saturating_sub(1).min(16);
    let minutes = RETRY_BASE_MINUTES
        .saturating_mul(1i64 << doublings)
        .min(RETRY_MAX_MINUTES);
    chrono::Duration::minutes(minutes)
}

/// What to tell the user about a failed scheduled refresh: nothing for repeats of an already
/// reported failure, and one escalation once the app is about to expire.
fn failure_notice(state: &FailureState, remaining: chrono::Duration) -> Option<FailureNotice> {
    if remaining <= chrono::Duration::hours(URGENT_WINDOW_HOURS) && !state.urgent_sent {
        Some(FailureNotice::Urgent)
    } else if state.count == 1 {
        Some(FailureNotice::First)
    } else {
        None
    }
}

/// Counts a failed attempt against `state` and returns what, if anything, the user should be told.
fn register_failure(
    state: &mut FailureState,
    now: DateTime<Utc>,
    remaining: chrono::Duration,
) -> Option<FailureNotice> {
    state.record(now);
    let notice = failure_notice(state, remaining);
    if notice == Some(FailureNotice::Urgent) {
        state.urgent_sent = true;
    }
    notice
}

/// Whether an app on a device that cannot be reached is due its one escalation: it is inside the
/// urgent window and has not been escalated yet. The retry schedule is left alone, since nothing
/// was attempted.
fn register_unreachable(state: &mut FailureState, remaining: chrono::Duration) -> bool {
    if remaining <= chrono::Duration::hours(URGENT_WINDOW_HOURS) && !state.urgent_sent {
        state.urgent_sent = true;
        true
    } else {
        false
    }
}

fn urgent_text(label: &str, device: &str, remaining: chrono::Duration, problem: &str) -> String {
    if remaining.num_seconds() > 0 {
        format!(
            "{label} on {device} expires in {} hours and could not be refreshed: {problem}",
            remaining.num_hours().max(1)
        )
    } else {
        format!("{label} on {device} has expired and could not be refreshed: {problem}")
    }
}

/// A profile that is already due as soon as it is installed would be refreshed again on the next
/// check, with a full upload each time. Reporting it as a failure applies the retry delay.
fn check_new_schedule(scheduled: DateTime<Utc>, now: DateTime<Utc>) -> Result<(), String> {
    if scheduled <= now {
        Err(format!(
            "Apple returned a provisioning profile that is already due for renewal \
             (next refresh {scheduled})"
        ))
    } else {
        Ok(())
    }
}

fn expires_at(app: &plume_store::RefreshApp) -> DateTime<Utc> {
    app.scheduled_refresh + chrono::Duration::days(REFRESH_LEAD_DAYS)
}

fn app_key(udid: &str, path: &Path) -> String {
    format!("{udid}|{}", path.display())
}

fn failure_ready(key: &str, now: DateTime<Utc>) -> bool {
    FAILURES
        .lock()
        .map(|failures| failures.get(key).is_none_or(|state| state.ready(now)))
        .unwrap_or(true)
}

/// When an app signed with `provision` should next be refreshed.
pub(crate) fn scheduled_refresh_for(provision: &MobileProvision) -> DateTime<Utc> {
    let expires = provision
        .expiration_date()
        .clone()
        .to_xml_format()
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now() + chrono::Duration::days(REFRESH_LEAD_DAYS + 1));
    expires - chrono::Duration::days(REFRESH_LEAD_DAYS)
}

pub(crate) fn notify(body: &str) {
    crate::toast::show(body);
}

fn reset_callback(trigger: RefreshTrigger) -> impl FnMut() -> bool {
    move || match trigger {
        RefreshTrigger::Manual => {
            crate::certificate_reset::confirm_within(MANUAL_RESET_PROMPT_TIMEOUT)
        }
        RefreshTrigger::Scheduled => {
            log::warn!("A certificate reset is needed; not confirming it unattended");
            false
        }
    }
}

fn identity_error(error: &str, trigger: RefreshTrigger) -> String {
    if trigger == RefreshTrigger::Scheduled && error.contains("Certificate reset cancelled") {
        "The developer certificate has to be reset, which needs your confirmation; \
         open Impactor and use Refresh Now"
            .to_string()
    } else {
        format!("Failed to create identity: {error}")
    }
}

struct RefreshGuard {
    udid: String,
    tasks: Arc<Mutex<HashSet<String>>>,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.remove(&self.udid);
            log::debug!("Released lock for device {}", self.udid);
        }
    }
}

pub struct RefreshDaemon {
    store_path: std::path::PathBuf,
    connected_devices: ConnectedDevices,
    active_tasks: Arc<Mutex<HashSet<String>>>,
}

impl RefreshDaemon {
    pub fn new() -> Self {
        Self {
            store_path: get_data_path().join("accounts.json"),
            connected_devices: Arc::new(Mutex::new(HashMap::new())),
            active_tasks: ACTIVE_TASKS.clone(),
        }
    }

    pub fn connected_devices(&self) -> ConnectedDevices {
        self.connected_devices.clone()
    }

    pub fn spawn(self) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            loop {
                if let Err(e) = rt.block_on(self.check_and_refresh()) {
                    log::error!("Refresh daemon error: {}", e);
                    notify(&format!("Failed to refresh: {}", e));
                }

                thread::sleep(CHECK_INTERVAL);
            }
        })
    }

    async fn check_and_refresh(&self) -> Result<(), String> {
        remove_stale_archives_in(&std::env::temp_dir(), STALE_ARCHIVE_AGE);

        let store = AccountStore::load(&Some(self.store_path.clone()))
            .await
            .map_err(|e| format!("Failed to load account store: {}", e))?;

        for (udid, refresh_device) in store.refreshes() {
            for app in &refresh_device.apps {
                let now = Utc::now();
                if app.scheduled_refresh > now {
                    continue;
                }

                if !failure_ready(&app_key(udid, &app.path), now) {
                    log::debug!(
                        "App at {:?} failed recently; waiting before the next attempt",
                        app.path
                    );
                    continue;
                }

                // We check for active tasks here to prevent the background loop
                // from even starting a wait if a manual refresh is already running.
                if self.is_busy(udid) {
                    log::info!(
                        "Device {} is already being processed. Skipping this app for now.",
                        udid
                    );
                    continue;
                }

                log::info!("App at {:?} needs refresh for device {}", app.path, udid);

                let device = self
                    .connected_devices
                    .lock()
                    .ok()
                    .and_then(|devices| devices.get(udid).cloned());

                let Some(device) = device else {
                    log::debug!(
                        "App at {:?} is due for refresh on {}, but no matching connected device was found. Retrying in {} seconds.",
                        app.path,
                        udid,
                        CHECK_INTERVAL.as_secs()
                    );
                    self.note_unreachable(refresh_device, app);
                    continue;
                };

                if let Err(e) = self
                    .refresh_app(
                        &store,
                        refresh_device,
                        app,
                        &device,
                        RefreshTrigger::Scheduled,
                    )
                    .await
                {
                    log::error!("Error refreshing app: {}", e);
                }
            }
        }

        Ok(())
    }

    fn is_busy(&self, udid: &str) -> bool {
        self.active_tasks
            .lock()
            .map(|t| t.contains(udid))
            .unwrap_or(false)
    }

    /// An app that is due on a device the app cannot see is never attempted, so it never fails
    /// either; without this the user would hear nothing as it ran out.
    fn note_unreachable(&self, refresh_device: &RefreshDevice, app: &plume_store::RefreshApp) {
        let remaining = expires_at(app) - Utc::now();
        let announce = FAILURES
            .lock()
            .map(|mut failures| {
                let state = failures
                    .entry(app_key(&refresh_device.udid, &app.path))
                    .or_default();
                register_unreachable(state, remaining)
            })
            .unwrap_or(false);

        if announce {
            notify(&urgent_text(
                app.name.as_deref().unwrap_or("???"),
                &refresh_device.name,
                remaining,
                "the device could not be reached",
            ));
        }
    }

    fn acquire(&self, udid: &str) -> Result<Option<RefreshGuard>, String> {
        let mut tasks = self
            .active_tasks
            .lock()
            .map_err(|_| "Failed to lock task registry")?;
        if !tasks.insert(udid.to_string()) {
            return Ok(None);
        }
        Ok(Some(RefreshGuard {
            udid: udid.to_string(),
            tasks: self.active_tasks.clone(),
        }))
    }

    /// Refreshes one app on one device and reports the outcome to the user: always for a manual
    /// refresh, and for a scheduled one when the failure is new or the app is about to expire.
    pub async fn refresh_app(
        &self,
        store: &AccountStore,
        refresh_device: &RefreshDevice,
        app: &plume_store::RefreshApp,
        device: &Device,
        trigger: RefreshTrigger,
    ) -> Result<(), String> {
        let Some(_guard) = self.acquire(&device.udid)? else {
            log::warn!(
                "Refresh already in progress for {}. Aborting duplicate.",
                device.udid
            );
            if trigger == RefreshTrigger::Manual {
                notify(&format!(
                    "A refresh for {} is already in progress",
                    refresh_device.name
                ));
            }
            return Ok(());
        };

        let result = match tokio::time::timeout(
            REFRESH_TIMEOUT,
            self.run_refresh(store, refresh_device, app, device, trigger),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "Timed out after {} minutes",
                REFRESH_TIMEOUT.as_secs() / 60
            )),
        };

        self.record_outcome(refresh_device, app, trigger, &result);
        result
    }

    fn record_outcome(
        &self,
        refresh_device: &RefreshDevice,
        app: &plume_store::RefreshApp,
        trigger: RefreshTrigger,
        result: &Result<(), String>,
    ) {
        let key = app_key(&refresh_device.udid, &app.path);
        let label = app.name.as_deref().unwrap_or("???");

        let error = match result {
            Ok(()) => {
                if let Ok(mut failures) = FAILURES.lock() {
                    failures.remove(&key);
                }
                return;
            }
            Err(error) => error,
        };

        let now = Utc::now();
        let remaining = expires_at(app) - now;
        let notice = {
            let Ok(mut failures) = FAILURES.lock() else {
                return;
            };
            register_failure(failures.entry(key).or_default(), now, remaining)
        };

        let body = match (trigger, notice) {
            (RefreshTrigger::Manual, _) => Some(format!(
                "Failed to refresh {label} for {}: {error}",
                refresh_device.name
            )),
            (RefreshTrigger::Scheduled, Some(FailureNotice::Urgent)) => {
                Some(urgent_text(label, &refresh_device.name, remaining, error))
            }
            (RefreshTrigger::Scheduled, Some(FailureNotice::First)) => Some(format!(
                "Failed to refresh {label} for {}: {error}. Retrying automatically.",
                refresh_device.name
            )),
            (RefreshTrigger::Scheduled, None) => None,
        };

        if let Some(body) = body {
            notify(&body);
        }
    }

    async fn run_refresh(
        &self,
        store: &AccountStore,
        refresh_device: &RefreshDevice,
        app: &plume_store::RefreshApp,
        device: &Device,
        trigger: RefreshTrigger,
    ) -> Result<(), String> {
        log::info!("Starting refresh for app at {:?}", app.path);

        if trigger == RefreshTrigger::Manual {
            notify(&format!(
                "Started refreshing {} for {}",
                app.name.as_deref().unwrap_or("???"),
                &refresh_device.name
            ));
        }

        let account = store
            .get_account(&refresh_device.account)
            .ok_or_else(|| format!("Account {} not found", refresh_device.account))?;

        let session = DeveloperSession::new(
            account.adsid().clone(),
            account.xcode_gs_token().clone(),
            AnisetteConfiguration::default().set_configuration_path(get_data_path()),
        )
        .await
        .map_err(|e| format!("Failed to create session: {}", e))?;

        let teams_response = session
            .qh_list_teams()
            .await
            .map_err(|e| format!("Failed to list teams: {}", e))?;

        if teams_response.teams.is_empty() {
            return Err("No teams available for this account".to_string());
        }

        let team_id = if account.team_id().is_empty() {
            &teams_response.teams[0].team_id
        } else {
            account.team_id()
        };

        let identity = {
            let mut on_certificate_reset = reset_callback(trigger);
            CertificateIdentity::new_with_session(
                &session,
                get_data_path(),
                None,
                team_id,
                false,
                Some(&mut on_certificate_reset),
            )
            .await
            .map_err(|e| identity_error(&e.to_string(), trigger))?
        };
        let identity_is_new = identity.new;

        // Determine if we need to reinstall:
        // - Mac devices always need reinstalling
        // - A network Apple TV always does: the installed-app query and the profile-only update
        //   below are both USB-only
        // - If the identity is new, we need to reinstall
        // - If the app is not installed, we need to reinstall
        // - If the app is installed and identity is not new, we can just update profiles
        let needs_reinstall = if device.is_mac || device.is_tvos() || identity_is_new {
            true
        } else {
            match app.bundle_id.as_deref() {
                Some(bundle_id) => !device
                    .is_app_installed(bundle_id)
                    .await
                    .map_err(|e| format!("Failed to check if app is installed: {}", e))?,
                None => true,
            }
        };

        if needs_reinstall {
            self.resign_and_reinstall(app, device, &session, team_id, identity)
                .await?;
        } else {
            log::info!(
                "Certificate exists and app is installed, updating provisioning profiles..."
            );
            self.update_provisioning_profiles(app, device, &session, team_id)
                .await?;
        }

        self.update_refresh_schedule(refresh_device, app).await?;

        log::info!("Successfully refreshed app at {:?}", app.path);

        notify(&format!(
            "Successfully refreshed {} for {}",
            app.name.as_deref().unwrap_or("???"),
            &refresh_device.name
        ));

        Ok(())
    }

    async fn resign_and_reinstall(
        &self,
        app: &plume_store::RefreshApp,
        device: &Device,
        session: &DeveloperSession,
        team_id: &str,
        signing_identity: CertificateIdentity,
    ) -> Result<(), String> {
        if device.udid.is_empty() {
            return Err("Device UDID is unknown; cannot register it with Apple".to_string());
        }

        let platform = if device.is_tvos() {
            DeveloperPlatform::TvOs
        } else {
            DeveloperPlatform::IOs
        };

        let team_id_string = team_id.to_string();
        session
            .qh_ensure_device(&team_id_string, &device.name, &device.udid, platform)
            .await
            .map_err(|e| format!("Failed to ensure device: {}", e))?;

        let bundle =
            Bundle::new(app.path.clone()).map_err(|e| format!("Failed to create bundle: {}", e))?;

        let options = SignerOptions {
            mode: SignerMode::Pem,
            ..Default::default()
        };

        let mut signer = Signer::new(Some(signing_identity), options);

        signer
            .register_bundle(&bundle, session, &team_id.to_string(), true, platform)
            .await
            .map_err(|e| format!("Failed to register bundle: {}", e))?;

        signer
            .sign_bundle(&bundle)
            .await
            .map_err(|e| format!("Failed to sign bundle: {}", e))?;

        if device.is_mac {
            plume_utils::install_app_mac(&app.path)
                .await
                .map_err(|e| format!("Failed to install app on Mac: {}", e))?;
        } else if device.is_network() {
            // One archive instead of the bundle directory: every file in a directory upload
            // costs several tunnelled round trips.
            let archive = archive_for_transfer(&app.path).await?;
            device
                .install_app(&archive.0, |_| async {})
                .await
                .map_err(|e| format!("Failed to install app: {}", e))?;
        } else {
            device
                .install_app(&app.path, |_| async {})
                .await
                .map_err(|e| format!("Failed to install app: {}", e))?;
        }

        Ok(())
    }

    async fn update_provisioning_profiles(
        &self,
        app: &plume_store::RefreshApp,
        device: &Device,
        session: &DeveloperSession,
        team_id: &str,
    ) -> Result<(), String> {
        let bundle =
            Bundle::new(app.path.clone()).map_err(|e| format!("Failed to create bundle: {}", e))?;

        let options = SignerOptions {
            mode: SignerMode::Pem,
            ..Default::default()
        };

        let mut signer = Signer::new(None, options);

        let platform = if device.is_tvos() {
            DeveloperPlatform::TvOs
        } else {
            DeveloperPlatform::IOs
        };

        signer
            .register_bundle(&bundle, session, &team_id.to_string(), true, platform)
            .await
            .map_err(|e| format!("Failed to register bundle: {}", e))?;

        for provision in &signer.provisioning_files {
            device
                .install_profile(provision)
                .await
                .map_err(|e| format!("Failed to install profile: {}", e))?;
        }

        Ok(())
    }

    /// Records the new refresh time for `app`. The store is re-read first so that anything the
    /// GUI saved since this cycle began is kept, and an app forgotten in the meantime is not
    /// resurrected.
    async fn update_refresh_schedule(
        &self,
        refresh_device: &RefreshDevice,
        app: &plume_store::RefreshApp,
    ) -> Result<(), String> {
        let embedded_prov_path = app.path.join("embedded.mobileprovision");
        if !embedded_prov_path.exists() {
            return Err("embedded.mobileprovision not found".to_string());
        }

        let provision = MobileProvision::load_with_path(&embedded_prov_path)
            .map_err(|e| format!("Failed to load mobile provision: {}", e))?;
        let scheduled_refresh = scheduled_refresh_for(&provision);
        check_new_schedule(scheduled_refresh, Utc::now())?;

        let mut store = AccountStore::load(&Some(self.store_path.clone()))
            .await
            .map_err(|e| format!("Failed to load account store: {}", e))?;

        let Some(mut updated_device) = store.get_refresh_device(&refresh_device.udid).cloned()
        else {
            return Ok(());
        };
        let Some(existing_app) = updated_device.apps.iter_mut().find(|a| a.path == app.path) else {
            return Ok(());
        };
        existing_app.scheduled_refresh = scheduled_refresh;

        store
            .add_or_update_refresh_device(updated_device)
            .await
            .map_err(|e| format!("Failed to update refresh schedule: {}", e))?;

        log::info!("Next refresh scheduled for: {}", scheduled_refresh);

        Ok(())
    }
}

/// A transfer archive in the temp directory, removed when dropped so that an abandoned refresh
/// does not leave it behind.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

async fn archive_for_transfer(bundle_dir: &Path) -> Result<TempFile, String> {
    let archive =
        TempFile(std::env::temp_dir().join(format!("plume_refresh_{}.ipa", uuid::Uuid::new_v4())));
    let source = bundle_dir.to_path_buf();
    let out = archive.0.clone();

    tokio::task::spawn_blocking(move || plume_utils::archive_bundle_dir(&source, &out))
        .await
        .map_err(|e| format!("Packaging task failed: {e}"))?
        .map_err(|e| format!("Failed to package for transfer: {e}"))?;

    Ok(archive)
}

/// Removes transfer archives older than `max_age`. A packaging task that outlives a timed-out
/// refresh writes its archive after the guard has already run, so this catches what the guard
/// cannot.
fn remove_stale_archives_in(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("plume_refresh_") && name.ends_with(".ipa")) {
            continue;
        }

        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

pub fn spawn_refresh_daemon() -> (thread::JoinHandle<()>, ConnectedDevices) {
    let daemon = RefreshDaemon::new();
    let devices = daemon.connected_devices();
    let handle = daemon.spawn();
    (handle, devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::minutes(minutes)
    }

    #[test]
    fn an_unreachable_device_is_escalated_once_and_only_near_expiry() {
        let mut state = FailureState::default();
        assert!(!register_unreachable(&mut state, chrono::Duration::days(2)));

        let close = chrono::Duration::hours(URGENT_WINDOW_HOURS - 1);
        assert!(register_unreachable(&mut state, close));
        assert!(!register_unreachable(&mut state, close));

        assert_eq!(state.count, 0);
        assert!(state.ready(at(0)));
    }

    #[test]
    fn a_profile_that_is_already_due_is_a_failure() {
        assert!(check_new_schedule(at(0), at(0)).is_err());
        assert!(check_new_schedule(at(-5), at(0)).is_err());
        assert!(check_new_schedule(at(1), at(0)).is_ok());
    }

    #[test]
    fn urgent_text_says_expired_once_the_time_has_passed() {
        let soon = urgent_text("App", "TV", chrono::Duration::hours(5), "boom");
        assert!(soon.contains("expires in 5 hours"));
        assert!(soon.contains("boom"));

        let gone = urgent_text("App", "TV", chrono::Duration::hours(-1), "boom");
        assert!(gone.contains("has expired"));
    }

    #[test]
    fn a_transfer_archive_is_removed_when_its_guard_drops() {
        let path = std::env::temp_dir().join(format!("plume_refresh_{}.ipa", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"zip").unwrap();

        drop(TempFile(path.clone()));

        assert!(!path.exists());
    }

    #[test]
    fn only_old_transfer_archives_are_swept() {
        let dir = std::env::temp_dir().join(format!("plume_sweep_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("plume_refresh_old.ipa");
        let fresh = dir.join("plume_refresh_fresh.ipa");
        let unrelated = dir.join("other_old.ipa");
        for path in [&old, &fresh, &unrelated] {
            std::fs::write(path, b"zip").unwrap();
        }
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        for path in [&old, &unrelated] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }

        remove_stale_archives_in(&dir, STALE_ARCHIVE_AGE);

        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(unrelated.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retry_delay_doubles_from_five_minutes_up_to_the_cap() {
        let minutes: Vec<i64> = (1..=9).map(|n| retry_delay(n).num_minutes()).collect();
        assert_eq!(minutes, vec![5, 10, 20, 40, 80, 160, 320, 360, 360]);
    }

    #[test]
    fn retry_delay_survives_absurd_failure_counts() {
        assert_eq!(retry_delay(u32::MAX).num_minutes(), RETRY_MAX_MINUTES);
        assert_eq!(retry_delay(0).num_minutes(), RETRY_BASE_MINUTES);
    }

    #[test]
    fn a_failed_app_is_not_ready_until_its_retry_time() {
        let mut state = FailureState::default();
        assert!(state.ready(at(0)));

        state.record(at(0));
        assert!(!state.ready(at(4)));
        assert!(state.ready(at(5)));
        assert!(state.ready(at(60)));
    }

    #[test]
    fn only_the_first_failure_of_a_streak_is_reported_when_far_from_expiry() {
        let far = chrono::Duration::days(2);
        let mut state = FailureState::default();

        state.record(at(0));
        assert_eq!(failure_notice(&state, far), Some(FailureNotice::First));

        state.record(at(5));
        assert_eq!(failure_notice(&state, far), None);
    }

    #[test]
    fn nearing_expiry_escalates_once() {
        let close = chrono::Duration::hours(URGENT_WINDOW_HOURS - 1);
        let mut state = FailureState::default();

        state.record(at(0));
        assert_eq!(failure_notice(&state, close), Some(FailureNotice::Urgent));

        let mut tracked = FailureState::default();
        assert_eq!(
            register_failure(&mut tracked, at(0), close),
            Some(FailureNotice::Urgent)
        );
        assert_eq!(register_failure(&mut tracked, at(5), close), None);
    }

    #[test]
    fn an_already_expired_app_is_still_urgent() {
        let mut state = FailureState::default();
        state.record(at(0));
        assert_eq!(
            failure_notice(&state, chrono::Duration::hours(-5)),
            Some(FailureNotice::Urgent)
        );
    }

    #[test]
    fn expiry_is_the_refresh_lead_after_the_scheduled_time() {
        let app = plume_store::RefreshApp {
            path: PathBuf::from("app"),
            name: None,
            bundle_id: None,
            scheduled_refresh: at(0),
        };
        assert_eq!(
            expires_at(&app) - app.scheduled_refresh,
            chrono::Duration::days(REFRESH_LEAD_DAYS)
        );
    }

    #[test]
    fn app_keys_differ_by_device_and_by_path() {
        let a = app_key("udid-1", Path::new("one.app"));
        assert_ne!(a, app_key("udid-2", Path::new("one.app")));
        assert_ne!(a, app_key("udid-1", Path::new("two.app")));
    }

    #[test]
    fn an_unattended_certificate_reset_names_the_action_to_take() {
        let message = identity_error("Certificate reset cancelled", RefreshTrigger::Scheduled);
        assert!(message.contains("Refresh Now"));

        let manual = identity_error("Certificate reset cancelled", RefreshTrigger::Manual);
        assert!(manual.starts_with("Failed to create identity"));
    }

    #[test]
    fn the_reset_callback_declines_unattended() {
        let mut callback = reset_callback(RefreshTrigger::Scheduled);
        assert!(!callback());
    }

    /// Runs the scheduled-refresh path on the IPA at `PLUME_REFRESH_IPA` against the first
    /// paired Apple TV found, using the selected stored account. The bundle is prepared the way
    /// the install flow prepares it (team id appended to the bundle id) and the resulting id
    /// must equal `PLUME_REFRESH_BUNDLE_ID` before anything is sent, so a mismatch cannot
    /// install a second copy of the app.
    #[test]
    #[ignore = "reinstalls an app on a paired Apple TV using the stored Apple ID"]
    fn live_refresh_installed_tvos_app() {
        use plume_utils::discovery::{DeviceDiscovery, PlatformDiscovery, group_network_devices};
        use plume_utils::{Package, PlistInfoTrait};

        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let _ = env_logger::builder().is_test(false).try_init();

        let ipa = PathBuf::from(std::env::var("PLUME_REFRESH_IPA").expect("set PLUME_REFRESH_IPA"));
        let expected_id =
            std::env::var("PLUME_REFRESH_BUNDLE_ID").expect("set PLUME_REFRESH_BUNDLE_ID");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let store = AccountStore::load(&Some(get_data_path().join("accounts.json")))
                .await
                .unwrap();
            let account = store.selected_account().expect("no selected account");
            let team_id = account.team_id().to_string();
            assert!(!team_id.is_empty(), "the selected account has no team id");

            let cache_dir = get_data_path();
            let discovered = PlatformDiscovery::new()
                .discover(Duration::from_secs(6))
                .await
                .unwrap();
            let mut device = group_network_devices(&discovered, &cache_dir)
                .into_iter()
                .next()
                .expect("no Apple TV found");
            let info = device
                .fetch_tvos_info(cache_dir.clone())
                .await
                .expect("tunnel to the Apple TV failed");
            device.apply_tvos_info(&info);
            assert!(!device.udid.is_empty(), "the Apple TV reported no UDID");
            println!("device: {} ({})", device.name, device.udid);

            let package = Package::new(ipa).unwrap();
            let bundle = package.get_package_bundle().unwrap();
            let mut signer = Signer::new(
                None,
                SignerOptions {
                    mode: SignerMode::Pem,
                    ..Default::default()
                },
            );
            signer.modify_bundle(&bundle, &Some(team_id)).await.unwrap();
            let bundle_id = bundle.get_bundle_identifier().unwrap();
            println!("bundle id to install: {bundle_id}");
            assert_eq!(bundle_id, expected_id, "refusing to install under a new id");

            let app = plume_store::RefreshApp {
                path: bundle.bundle_dir().clone(),
                name: bundle.get_name(),
                bundle_id: Some(bundle_id),
                scheduled_refresh: Utc::now() - chrono::Duration::minutes(1),
            };
            let refresh_device = RefreshDevice {
                udid: device.udid.clone(),
                name: device.name.clone(),
                account: account.email().clone(),
                apps: vec![app.clone()],
                is_mac: false,
            };

            let started = std::time::Instant::now();
            let result = RefreshDaemon::new()
                .refresh_app(
                    &store,
                    &refresh_device,
                    &app,
                    &device,
                    RefreshTrigger::Scheduled,
                )
                .await;
            println!("refresh finished in {:?}: {:?}", started.elapsed(), result);

            if let Ok(provision) =
                MobileProvision::load_with_path(&app.path.join("embedded.mobileprovision"))
            {
                println!(
                    "next refresh would be {}",
                    scheduled_refresh_for(&provision)
                );
            }

            package.remove_package_stage();
            result.expect("refresh failed");
        });
    }
}
