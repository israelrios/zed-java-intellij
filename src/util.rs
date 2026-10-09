use serde::{Deserialize, Serialize};
use std::{
    env::current_dir,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use zed_extension_api::{
    self as zed, Architecture, DownloadedFileType, LanguageServerId, Os, Worktree,
    current_platform, serde_json::Value,
};

use crate::{
    config::{CheckUpdates, get_check_updates, get_java_home, is_java_autodownload},
    jdk::Jdk,
};

// Errors
const EXPAND_ERROR: &str = "Failed to expand ~";
const CURR_DIR_ERROR: &str = "Could not get current dir";
const DIR_ENTRY_LOAD_ERROR: &str = "Failed to load directory entry";
const DIR_ENTRY_RM_ERROR: &str = "Failed to remove directory entry";
const ENTRY_TYPE_ERROR: &str = "Could not determine entry type";
const FILE_ENTRY_RM_ERROR: &str = "Failed to remove file entry";
const PATH_TO_STR_ERROR: &str = "Failed to convert path to string";
const JAVA_EXEC_NOT_FOUND_ERROR: &str = "Could not find Java executable in JAVA_HOME or on PATH";
const PATH_IS_NOT_DIR: &str = "File exists but is not a path";
const NO_LOCAL_INSTALL_NEVER_ERROR: &str =
    "Update checks disabled (never) and no local installation found";
const NO_LOCAL_INSTALL_ONCE_ERROR: &str =
    "Update check already performed once and no local installation found";

pub const UPDATE_CHECK_MARKER: &str = ".update_checked";
const UPDATE_CHECK_TTL_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Deserialize, Serialize)]
struct UpdateCheckRecord {
    version: String,
    checked_at_unix_seconds: u64,
}

/// Create a Path if it does not exist
///
/// **Errors** if a file that is not a path exists at the location or read/write access failed for the location
///
///# Arguments
/// * [`path`] the path to create
///
///# Returns
///
/// Ok(()) if the path exists or was created successfully
pub fn create_path_if_not_exists<P: AsRef<Path>>(path: P) -> zed::Result<()> {
    let path_ref = path.as_ref();
    match fs::metadata(path_ref) {
        Ok(metadata) => {
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(format!("{PATH_IS_NOT_DIR}: {path_ref:?}"))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path_ref).map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Return the default update-check record path for a component install directory.
pub fn update_check_path(component_name: &str) -> PathBuf {
    PathBuf::from(component_name).join(UPDATE_CHECK_MARKER)
}

/// Return whether an update-check marker exists, regardless of its format or contents.
pub fn has_checked_once(update_check_path: &Path) -> bool {
    update_check_path.exists()
}

/// Return the recorded version when its successful remote check is less than 24 hours old.
pub fn fresh_cached_version(update_check_path: &Path) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    fresh_cached_version_at(update_check_path, now)
}

/// Return a cached version relative to an explicit time, primarily to make TTL
/// boundary behavior deterministic in tests.
fn fresh_cached_version_at(update_check_path: &Path, now: u64) -> Option<String> {
    let record =
        serde_json::from_slice::<UpdateCheckRecord>(&fs::read(update_check_path).ok()?).ok()?;
    let age = now.checked_sub(record.checked_at_unix_seconds)?;

    if !record.version.is_empty() && age < UPDATE_CHECK_TTL_SECONDS {
        Some(record.version)
    } else {
        None
    }
}

/// Atomically record the version and current time of a successful remote update check.
pub fn record_successful_update_check(update_check_path: &Path, version: &str) -> zed::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("System clock is before the Unix epoch: {err}"))?
        .as_secs();
    record_successful_update_check_at(update_check_path, version, now)
}

/// Serialize an update-check record using an explicit timestamp and replace the
/// previous record through a temporary sibling file.
fn record_successful_update_check_at(
    update_check_path: &Path,
    version: &str,
    now: u64,
) -> zed::Result<()> {
    if let Some(parent) = update_check_path.parent() {
        create_path_if_not_exists(parent)
            .map_err(|err| format!("Failed to create update-check directory {parent:?}: {err}"))?;
    }

    let record = UpdateCheckRecord {
        version: version.to_string(),
        checked_at_unix_seconds: now,
    };
    let contents = serde_json::to_vec(&record)
        .map_err(|err| format!("Failed to serialize update-check record: {err}"))?;
    replace_update_check_record(update_check_path, &contents, now)
}

/// Write and sync a uniquely named temporary record before renaming it over the
/// destination, preventing interrupted writes from leaving partial JSON.
fn replace_update_check_record(
    update_check_path: &Path,
    contents: &[u8],
    nonce: u64,
) -> zed::Result<()> {
    let file_name = update_check_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(UPDATE_CHECK_MARKER);

    for attempt in 0..16 {
        let temporary_path =
            update_check_path.with_file_name(format!("{file_name}.{nonce}.{attempt}.tmp"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path);
        let mut file = match file {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "Failed to create temporary update-check record {temporary_path:?}: {err}"
                ));
            }
        };

        let result = file.write_all(contents).and_then(|_| file.sync_all());
        drop(file);
        let result = result.and_then(|_| fs::rename(&temporary_path, update_check_path));
        if let Err(err) = result {
            let _ = fs::remove_file(&temporary_path);
            return Err(format!(
                "Failed to replace update-check record {update_check_path:?}: {err}"
            ));
        }
        return Ok(());
    }

    Err(format!(
        "Failed to allocate a temporary update-check record for {update_check_path:?}"
    ))
}

/// Expand ~ on Unix-like systems
///
/// # Arguments
///
/// * [`worktree`] Zed extension worktree with access to ENV
/// * [`path`] path to expand
///
/// # Returns
///
/// On Unix-like systems ~ is replaced with the value stored in HOME
///
/// On Windows systems [`path`] is returned untouched
pub fn expand_home_path(worktree: &Worktree, path: String) -> zed::Result<String> {
    match zed::current_platform() {
        (Os::Windows, _) => Ok(path),
        (_, _) => worktree
            .shell_env()
            .iter()
            .find(|&(key, _)| key == "HOME")
            .map_or_else(
                || Err(EXPAND_ERROR.to_string()),
                |(_, value)| Ok(path.replace("~", value)),
            ),
    }
}

/// Get the extension current directory
///
/// # Returns
///
/// The [`PathBuf`] of the extension directory
///
/// # Errors
///
/// This functoin will return an error if it was not possible to retrieve the current directory
pub fn get_curr_dir() -> zed::Result<PathBuf> {
    current_dir().map_err(|_| CURR_DIR_ERROR.to_string())
}

/// Retrieve the path to a java exec either:
/// - defined by the user in `settings.json` under option `java_home`
/// - from PATH
/// - from JAVA_HOME
/// - from the bundled OpenJDK if option `jdk_auto_download` is true
///
/// # Arguments
///
/// * [`configuration`] a JSON object representing the user configuration
/// * [`worktree`] Zed extension worktree
///
/// # Returns
///
/// Returns the path to the java exec file
///
/// # Errors
///
/// This function will return an error if neither PATH or JAVA_HOME led
/// to a java exec file
pub fn get_java_executable(
    configuration: &Option<Value>,
    worktree: &Worktree,
    language_server_id: &LanguageServerId,
) -> zed::Result<PathBuf> {
    let java_executable_filename = get_java_exec_name();

    // Get executable from $JAVA_HOME
    if let Some(java_home) = get_java_home(configuration, worktree) {
        let java_executable = PathBuf::from(java_home)
            .join("bin")
            .join(java_executable_filename);
        return Ok(java_executable);
    }
    // If we can't, try to get it from $PATH
    if let Some(java_home) = worktree.which(java_executable_filename.as_str()) {
        return Ok(PathBuf::from(java_home));
    }

    // If the user has set the option, retrieve the latest version of Corretto (OpenJDK)
    if is_java_autodownload(configuration) {
        let mut jdk = Jdk::new();
        return Ok(jdk
            .get_bin_path(language_server_id, configuration, worktree)
            .map_err(|err| format!("Failed to auto-download JDK: {err}"))?
            .join(java_executable_filename));
    }

    Err(JAVA_EXEC_NOT_FOUND_ERROR.to_string())
}

/// Retrieve the executable name for Java on this platform
///
/// # Returns
///
/// Returns the executable java name
pub fn get_java_exec_name() -> String {
    platform_exec_name("java")
}

/// The single install directory shared by every native binary the extension
/// downloads. Managed binaries co-locate under the extension release tag in
/// `bin/<version>/`; root-level binaries are local development overrides.
pub const NATIVE_BIN_DIR: &str = "bin";

/// Find a native development override or the binary from the given release.
pub fn find_native_binary_in_release(executable: &str, release: &str) -> Option<PathBuf> {
    find_native_binary_in(Path::new(NATIVE_BIN_DIR), executable, release)
}

/// Resolve a native binary within an explicit install directory for filesystem tests.
fn find_native_binary_in(install_dir: &Path, executable: &str, release: &str) -> Option<PathBuf> {
    let preferred_paths = [
        install_dir.join(executable),
        install_dir.join(release).join(executable),
    ];
    if let Some(path) = preferred_paths
        .into_iter()
        .find(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
    {
        return Some(path);
    }

    None
}

/// The platform-specific executable file name for a binary
/// (appends `.exe` on Windows).
pub fn platform_exec_name(binary: &str) -> String {
    match current_platform().0 {
        Os::Windows => format!("{binary}.exe"),
        _ => binary.to_string(),
    }
}

/// The release-asset name and archive type for a downloaded native binary on the
/// current platform, e.g. `gradle-lsp-bridge-darwin-aarch64.tar.gz`. Upstream ships
/// per-platform bridge assets under each release with this naming;
/// only `binary` differs.
///
/// # Errors
///
/// Returns an error on an unsupported CPU architecture.
pub fn platform_asset_name(binary: &str) -> zed::Result<(String, DownloadedFileType)> {
    let (os, arch) = current_platform();
    let (os_str, file_type) = match os {
        Os::Mac => ("darwin", DownloadedFileType::GzipTar),
        Os::Linux => ("linux", DownloadedFileType::GzipTar),
        Os::Windows => ("windows", DownloadedFileType::Zip),
    };
    let arch_str = match arch {
        Architecture::Aarch64 => "aarch64",
        Architecture::X8664 => "x86_64",
        _ => return Err("Unsupported architecture".into()),
    };
    let ext = if matches!(file_type, DownloadedFileType::Zip) {
        "zip"
    } else {
        "tar.gz"
    };
    Ok((format!("{binary}-{os_str}-{arch_str}.{ext}"), file_type))
}

/// Converts a [`Path`] into a [`String`].
///
/// # Arguments
///
/// * `path` - The path of type `AsRef<Path>` to be converted.
///
/// # Returns
///
/// Returns a `String` representing the path.
///
/// # Errors
///
/// This function will return an error when the string conversion fails.
pub fn path_to_string<P: AsRef<Path>>(path: P) -> zed::Result<String> {
    path.as_ref()
        .to_path_buf()
        .into_os_string()
        .into_string()
        .map_err(|_| PATH_TO_STR_ERROR.to_string())
}

/// Remove all files or directories that aren't equal to [`filename`].
///
/// This function scans the directory given by [`prefix`] and removes any
/// file or directory whose name does not exactly match [`filename`].
///
/// # Arguments
///
/// * [`prefix`] - The path to the directory to clean. See [`AsRef<Path>`] for supported types.
/// * [`filename`] - The name of the file to keep.
///
/// # Returns
///
/// Returns `Ok(())` on success, even if some removals fail (errors are printed to stdout).
pub fn remove_all_files_except<P: AsRef<Path>>(prefix: P, filename: &str) -> zed::Result<()> {
    let entries: Vec<_> = match fs::read_dir(prefix) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(err) => {
            println!("{DIR_ENTRY_LOAD_ERROR}: {err}");
            return Err(format!("{DIR_ENTRY_LOAD_ERROR}: {err}"));
        }
    };

    for entry in entries {
        if entry.file_name().to_str() != Some(filename) {
            match entry.file_type() {
                Ok(t) => {
                    if t.is_dir()
                        && let Err(err) = fs::remove_dir_all(entry.path())
                    {
                        println!("{DIR_ENTRY_RM_ERROR}: {err}");
                    } else if t.is_file()
                        && let Err(err) = fs::remove_file(entry.path())
                    {
                        println!("{FILE_ENTRY_RM_ERROR}: {err}");
                    }
                }
                Err(type_err) => println!("{ENTRY_TYPE_ERROR}: {type_err}"),
            }
        }
    }

    Ok(())
}

/// Remove all subdirectories except the named one, preserving root-level files.
pub fn remove_all_directories_except<P: AsRef<Path>>(
    prefix: P,
    directory_name: &str,
) -> zed::Result<()> {
    let entries = fs::read_dir(prefix).map_err(|err| format!("{DIR_ENTRY_LOAD_ERROR}: {err}"))?;

    for entry in entries.filter_map(Result::ok) {
        if entry.file_name().to_str() != Some(directory_name)
            && entry.file_type().is_ok_and(|file_type| file_type.is_dir())
            && let Err(err) = fs::remove_dir_all(entry.path())
        {
            println!("{DIR_ENTRY_RM_ERROR}: {err}");
        }
    }

    Ok(())
}

/// Determine whether to use local component or download based on update mode
///
/// This function handles the common update policy for managed components:
/// 1. Apply update check mode (Never/Once/Always)
/// 2. Find local installation if applicable
///
/// # Arguments
/// * `configuration` - User configuration JSON
/// * `local` - Optional path to local installation
/// * `component_name` - Component name for error messages (e.g., "intellij-server", "gradle-ls")
/// * `update_check_path` - Component-specific update-check record
///
/// # Returns
/// * `Ok(Some(PathBuf))` - Local installation should be used
/// * `Ok(None)` - Should download
/// * `Err(String)` - Error message if resolution failed
///
/// # Errors
/// - Update mode is Never but no local installation found
/// - Update mode is Once and already checked but no local installation found
pub fn should_use_local_or_download(
    configuration: &Option<Value>,
    local: Option<PathBuf>,
    component_name: &str,
    update_check_path: &Path,
) -> zed::Result<Option<PathBuf>> {
    match get_check_updates(configuration) {
        CheckUpdates::Never => match local {
            Some(path) => Ok(Some(path)),
            None => Err(format!(
                "{NO_LOCAL_INSTALL_NEVER_ERROR} for {component_name}"
            )),
        },
        CheckUpdates::Once => {
            // If we have a local installation, use it
            if let Some(path) = local {
                return Ok(Some(path));
            }

            // If we've already checked once, don't check again
            if has_checked_once(update_check_path) {
                return Err(format!(
                    "{NO_LOCAL_INSTALL_ONCE_ERROR} for {component_name}"
                ));
            }

            // First time checking - allow download
            Ok(None)
        }
        CheckUpdates::Always => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    };

    use serde_json::json;

    use super::*;

    static NEXT_TEMP_PATH: AtomicU64 = AtomicU64::new(0);

    fn temporary_update_check_path(test_name: &str) -> PathBuf {
        let id = NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "zed-java-update-check-{}-{id}-{test_name}",
                std::process::id()
            ))
            .join(UPDATE_CHECK_MARKER)
    }

    fn remove_temporary_update_check(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn update_check_is_fresh_for_less_than_24_hours() {
        let path = temporary_update_check_path("fresh");
        record_successful_update_check_at(&path, "1.2.3", 1_000).unwrap();

        assert_eq!(
            fresh_cached_version_at(&path, 1_000 + UPDATE_CHECK_TTL_SECONDS - 1),
            Some("1.2.3".to_string())
        );
        remove_temporary_update_check(&path);
    }

    #[test]
    fn update_check_expires_at_24_hours() {
        let path = temporary_update_check_path("expired");
        record_successful_update_check_at(&path, "1.2.3", 1_000).unwrap();

        assert_eq!(
            fresh_cached_version_at(&path, 1_000 + UPDATE_CHECK_TTL_SECONDS),
            None
        );
        remove_temporary_update_check(&path);
    }

    #[test]
    fn future_update_check_timestamp_is_stale() {
        let path = temporary_update_check_path("future");
        record_successful_update_check_at(&path, "1.2.3", 2_000).unwrap();

        assert_eq!(fresh_cached_version_at(&path, 1_000), None);
        remove_temporary_update_check(&path);
    }

    #[test]
    fn legacy_marker_is_stale_but_still_counts_for_once_mode() {
        let path = temporary_update_check_path("legacy");
        create_path_if_not_exists(path.parent().unwrap()).unwrap();
        fs::write(&path, "1.2.3").unwrap();

        assert_eq!(fresh_cached_version_at(&path, 1_000), None);
        assert!(has_checked_once(&path));

        let configuration = Some(json!({ "check_updates": "once" }));
        assert!(should_use_local_or_download(&configuration, None, "test", &path).is_err());
        remove_temporary_update_check(&path);
    }

    #[test]
    fn once_mode_ignores_update_check_record_freshness() {
        let path = temporary_update_check_path("once");
        record_successful_update_check_at(&path, "1.2.3", 1_000).unwrap();
        let configuration = Some(json!({ "check_updates": "once" }));

        assert!(should_use_local_or_download(&configuration, None, "test", &path).is_err());
        remove_temporary_update_check(&path);
    }

    #[test]
    fn successful_recheck_refreshes_unchanged_version_timestamp() {
        let path = temporary_update_check_path("refresh");
        record_successful_update_check_at(&path, "1.2.3", 1_000).unwrap();
        record_successful_update_check_at(&path, "1.2.3", 2_000).unwrap();

        let record =
            serde_json::from_slice::<UpdateCheckRecord>(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.version, "1.2.3");
        assert_eq!(record.checked_at_unix_seconds, 2_000);
        remove_temporary_update_check(&path);
    }

    #[test]
    fn once_mode_uses_marker_presence_regardless_of_contents() {
        let path = temporary_update_check_path("once-marker-contents");
        create_path_if_not_exists(path.parent().unwrap()).unwrap();

        fs::write(&path, r#"{"version":"1.2.3""#).unwrap();
        assert!(has_checked_once(&path));
        assert_eq!(fresh_cached_version_at(&path, 1_000), None);

        fs::write(&path, "").unwrap();
        assert!(has_checked_once(&path));
        assert_eq!(fresh_cached_version_at(&path, 1_000), None);
        remove_temporary_update_check(&path);
    }

    #[test]
    fn update_check_record_replacement_leaves_no_temporary_file() {
        let path = temporary_update_check_path("atomic");
        record_successful_update_check_at(&path, "1.2.3", 1_000).unwrap();
        record_successful_update_check_at(&path, "1.2.4", 2_000).unwrap();

        let record =
            serde_json::from_slice::<UpdateCheckRecord>(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.version, "1.2.4");
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "only the completed update-check record should remain"
        );
        remove_temporary_update_check(&path);
    }

    #[test]
    fn concurrent_update_check_writers_leave_a_valid_record() {
        let path = temporary_update_check_path("concurrent");
        let barrier = Arc::new(Barrier::new(8));
        let writers = (0..8)
            .map(|writer| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    record_successful_update_check_at(
                        &path,
                        &format!("1.2.{writer}"),
                        1_000 + writer,
                    )
                })
            })
            .collect::<Vec<_>>();

        for writer in writers {
            writer.join().unwrap().unwrap();
        }

        let record =
            serde_json::from_slice::<UpdateCheckRecord>(&fs::read(&path).unwrap()).unwrap();
        assert!(record.version.starts_with("1.2."));
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "concurrent writers must not leave temporary files"
        );
        remove_temporary_update_check(&path);
    }

    #[test]
    fn native_binary_discovery_ignores_other_releases() {
        let root = temporary_update_check_path("native-binary")
            .parent()
            .unwrap()
            .to_path_buf();
        let old_binary = root.join("v1.0.0").join("test-binary");
        create_path_if_not_exists(old_binary.parent().unwrap()).unwrap();
        fs::write(&old_binary, "").unwrap();

        assert_eq!(find_native_binary_in(&root, "test-binary", "v2.0.0"), None);

        let current_binary = root.join("v2.0.0").join("test-binary");
        create_path_if_not_exists(&current_binary).unwrap();
        assert_eq!(
            find_native_binary_in(&root, "test-binary", "v2.0.0"),
            None,
            "a directory at the binary path must not be accepted"
        );
        fs::remove_dir_all(&current_binary).unwrap();
        create_path_if_not_exists(current_binary.parent().unwrap()).unwrap();
        fs::write(&current_binary, "").unwrap();
        assert_eq!(
            find_native_binary_in(&root, "test-binary", "v2.0.0"),
            Some(current_binary)
        );

        let development_binary = root.join("test-binary");
        fs::write(&development_binary, "").unwrap();
        assert_eq!(
            find_native_binary_in(&root, "test-binary", "v2.0.0"),
            Some(development_binary)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn native_cleanup_removes_old_versions_and_preserves_root_files() {
        let root = temporary_update_check_path("native-cleanup")
            .parent()
            .unwrap()
            .to_path_buf();
        let current_dir = root.join("v2.0.0");
        let old_dir = root.join("v1.0.0");
        let development_binary = root.join("test-binary");
        create_path_if_not_exists(&current_dir).unwrap();
        create_path_if_not_exists(&old_dir).unwrap();
        fs::write(&development_binary, "").unwrap();

        remove_all_directories_except(&root, "v2.0.0").unwrap();

        assert!(current_dir.is_dir());
        assert!(!old_dir.exists());
        assert!(development_binary.is_file());
        let _ = fs::remove_dir_all(root);
    }
}
