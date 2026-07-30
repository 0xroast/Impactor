mod bundle;
mod cgbi;
mod device;
pub mod discovery;
mod options;
mod package;
mod signer;
mod tweak;

use std::path::Path;

pub use bundle::{Bundle, BundleType}; // Bundle helper
pub use device::{Device, TvosDeviceInfo, get_device_for_id, install_app_mac, synthetic_device_id}; // Device helper
pub use idevice::utils::installation::InstallProgress; // Progress reported by `Device::install_app`
pub use options::{
    SignerApp, // Supported app types
    SignerAppReal,
    SignerEmbedding,   // Embedding options
    SignerFeatures,    // Feature support options
    SignerInstallMode, // Installation mode
    SignerMode,        // Signing mode
    SignerOptions,     // Main
};
pub use package::Package; // Package helper
pub use signer::Signer; // Signer
pub use tweak::Tweak; // Tweak helper

pub type Result<T> = std::result::Result<T, Error>;

use thiserror::Error as ThisError;
#[derive(Debug, ThisError)]
pub enum Error {
    #[error("Info.plist not found")]
    BundleInfoPlistMissing,
    // Device
    #[error("Bundle failed to rename, make sure its available: {0}")]
    BundleFailedToCopy(String),
    // Tweak
    #[error("Invalid tweak file path")]
    TweakInvalidPath,
    #[error("Tweak extraction failed: {0}")]
    TweakExtractionFailed(String),
    #[error("Unsupported file type: {0}")]
    UnsupportedFileType(String),

    #[error("Zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Info.plist not found")]
    PackageInfoPlistMissing,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Plist error: {0}")]
    Plist(#[from] plist::Error),
    #[error("Core error: {0}")]
    Core(#[from] plume_core::Error),
    #[error("Idevice error: {0}")]
    Idevice(#[from] idevice::IdeviceError),
    #[error("Codesign error: {0}")]
    Codesign(#[from] plume_core::AppleCodesignError),
    #[error("Other error: {0}")]
    Other(String),
    #[error("Image error: {0}")]
    Image(#[from] image::ImageError),
}

pub trait PlistInfoTrait {
    fn get_name(&self) -> Option<String>;
    fn get_executable(&self) -> Option<String>;
    fn get_bundle_identifier(&self) -> Option<String>;
    fn get_bundle_name(&self) -> Option<String>;
    fn get_version(&self) -> Option<String>;
    fn get_build_version(&self) -> Option<String>;
}

pub async fn copy_dir_recursively(src: &Path, dst: &Path) -> Result<()> {
    use tokio::fs;

    fs::create_dir_all(dst).await?;
    let mut entries = fs::read_dir(src).await?;

    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_symlink() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                let target = fs::read_link(&src_path).await?;
                symlink(&target, &dst_path)?;
            }
        } else if file_type.is_dir() {
            Box::pin(copy_dir_recursively(&src_path, &dst_path)).await?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).await?;
        }
    }

    Ok(())
}

/// Sums the byte length of every regular file under `dir`, recursing into subdirectories.
/// Symlinks are skipped rather than followed, so a symlink to a directory cannot lead the
/// walk outside `dir` and a symlink to a file is not double-counted through its target.
pub async fn dir_total_bytes(dir: &Path) -> Result<u64> {
    use std::collections::VecDeque;
    use tokio::fs;

    let mut total: u64 = 0;
    let mut queue = VecDeque::new();
    queue.push_back(dir.to_path_buf());

    while let Some(current) = queue.pop_front() {
        let mut entries = fs::read_dir(&current).await?;

        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;

            if file_type.is_symlink() {
                continue;
            } else if file_type.is_dir() {
                queue.push_back(entry.path());
            } else if file_type.is_file() {
                total += entry.metadata().await?.len();
            }
        }
    }

    Ok(total)
}

/// Renders a byte count for display, in decimal units to match how Apple's own tools report
/// file sizes. Sub-megabyte values keep whole units because a decimal place there is noise.
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000 * KB;
    const GB: u64 = 1_000 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "plume_utils_test_{tag}_{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[tokio::test]
    async fn dir_total_bytes_empty_dir_is_zero() {
        let dir = unique_temp_dir("dir_total_bytes_empty");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let total = dir_total_bytes(&dir).await.unwrap();
        assert_eq!(total, 0);

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn dir_total_bytes_sums_files_in_dir() {
        let dir = unique_temp_dir("dir_total_bytes_sums");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.bin"), vec![0u8; 100])
            .await
            .unwrap();
        tokio::fs::write(dir.join("b.bin"), vec![0u8; 250])
            .await
            .unwrap();

        let total = dir_total_bytes(&dir).await.unwrap();
        assert_eq!(total, 350);

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn dir_total_bytes_includes_nested_subdirectories() {
        let dir = unique_temp_dir("dir_total_bytes_nested");
        let nested = dir.join("nested").join("deeper");
        tokio::fs::create_dir_all(&nested).await.unwrap();
        tokio::fs::write(dir.join("top.bin"), vec![0u8; 10])
            .await
            .unwrap();
        tokio::fs::write(dir.join("nested").join("mid.bin"), vec![0u8; 20])
            .await
            .unwrap();
        tokio::fs::write(nested.join("deep.bin"), vec![0u8; 30])
            .await
            .unwrap();

        let total = dir_total_bytes(&dir).await.unwrap();
        assert_eq!(total, 60);

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[test]
    fn format_bytes_picks_a_unit_per_magnitude() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1 KB");
        assert_eq!(format_bytes(999_999), "999 KB");
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
        assert_eq!(format_bytes(1_000_000_000), "1.0 GB");
    }

    #[test]
    fn format_bytes_renders_the_test_bundle_size() {
        assert_eq!(format_bytes(54_741_568), "54.7 MB");
    }
}
