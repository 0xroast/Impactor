//! Desktop notifications under the app's own identity.
//!
//! On Windows a toast takes its name and icon from the AppUserModelID it is sent under. With none
//! given, notify-rust sends under PowerShell's id, so every toast reads "Windows PowerShell" with
//! PowerShell's icon. `register` creates the id's registry entry (display name and icon file) and
//! `show` sends under it.

#[cfg(windows)]
use std::path::Path;

#[cfg(windows)]
pub(crate) const APP_USER_MODEL_ID: &str = "dev.khcrysalis.PlumeImpactor";

#[cfg(windows)]
const ICON_PNG: &[u8] = include_bytes!("./tray_colored.png");

pub(crate) fn show(body: &str) {
    let mut notification = notify_rust::Notification::new();
    notification.summary(crate::APP_NAME).body(body);

    #[cfg(windows)]
    notification.app_id(APP_USER_MODEL_ID);

    notification.show().ok();
}

/// Makes Windows show this app's name and icon on its toasts. The icon is written under
/// `icon_dir` because the registry entry can only point at a file.
#[cfg(windows)]
pub(crate) fn register(icon_dir: &Path) {
    if let Err(e) = register_as(APP_USER_MODEL_ID, icon_dir) {
        log::warn!("Could not register the notification identity: {e}");
    }
}

#[cfg(not(windows))]
pub(crate) fn register(_icon_dir: &std::path::Path) {}

#[cfg(windows)]
fn register_as(id: &str, icon_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(icon_dir).map_err(|e| format!("icon directory: {e}"))?;
    let icon_path = icon_dir.join("notification_icon.png");
    if std::fs::read(&icon_path).ok().as_deref() != Some(ICON_PNG) {
        std::fs::write(&icon_path, ICON_PNG).map_err(|e| format!("icon file: {e}"))?;
    }

    let key = windows_registry::CURRENT_USER
        .create(format!(r"Software\Classes\AppUserModelId\{id}"))
        .map_err(|e| format!("registry key: {e}"))?;
    key.set_string("DisplayName", crate::APP_NAME)
        .map_err(|e| format!("DisplayName: {e}"))?;
    key.set_string("IconUri", icon_path.to_string_lossy())
        .map_err(|e| format!("IconUri: {e}"))?;

    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn registration_names_the_app_and_points_at_a_real_icon_file() {
        let unique = uuid::Uuid::new_v4();
        let id = format!("plume.test.{unique}");
        let icon_dir = std::env::temp_dir().join(format!("plume_toast_test_{unique}"));

        register_as(&id, &icon_dir).unwrap();

        let path = format!(r"Software\Classes\AppUserModelId\{id}");
        let key = windows_registry::CURRENT_USER.open(&path).unwrap();
        let name = key.get_string("DisplayName").unwrap();
        let icon = key.get_string("IconUri").unwrap();
        windows_registry::CURRENT_USER.remove_tree(&path).ok();

        assert_eq!(name, crate::APP_NAME);
        assert_eq!(std::fs::read(&icon).unwrap(), ICON_PNG);
        std::fs::remove_dir_all(&icon_dir).ok();
    }

    /// Sends one real toast under the app's identity; whether it reads "Impactor" with the
    /// Impactor icon (and not PowerShell's) has to be checked by eye.
    #[test]
    #[ignore = "shows a desktop notification"]
    fn live_toast_shows_under_the_app_identity() {
        register(&crate::defaults::get_data_path());
        show("Toast identity check");
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    #[test]
    fn registering_twice_is_harmless() {
        let unique = uuid::Uuid::new_v4();
        let id = format!("plume.test.{unique}");
        let icon_dir = std::env::temp_dir().join(format!("plume_toast_test_{unique}"));

        register_as(&id, &icon_dir).unwrap();
        register_as(&id, &icon_dir).unwrap();

        windows_registry::CURRENT_USER
            .remove_tree(format!(r"Software\Classes\AppUserModelId\{id}"))
            .ok();
        std::fs::remove_dir_all(&icon_dir).ok();
    }
}
