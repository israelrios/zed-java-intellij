use std::{
    fs::{self, metadata, read_dir},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use zed_extension_api::{
    self as zed, Architecture, DownloadedFileType, LanguageServerId,
    LanguageServerInstallationStatus, Os, Worktree, current_platform,
    http_client::{HttpMethod, HttpRequest, fetch},
    serde_json::{self, Value},
    set_language_server_installation_status,
};

use crate::{
    archive::{self, ArchiveKind},
    config::{get_server_launcher, get_system_directory},
    downloadable::Downloadable,
    util::{create_path_if_not_exists, path_to_string},
};

const INSTALL_PATH: &str = "intellij-server";
const VSIX_DIR: &str = "vsix";
const OPEN_VSX_EXTENSION: &str = "https://open-vsx.org/api/JetBrains/intellij-server";
const SERVER_BUNDLE_FILE: &str = "extension/server-bundle.json";
const LAUNCHER_NAME: &str = "intellij-server";
const EULA_FILE: &str = "EULA.txt";
/// The archive as downloaded, before it is checked and unpacked.
const DOWNLOAD_SUFFIX: &str = ".download";
/// An install being unpacked; renamed to the version once complete.
const PARTIAL_SUFFIX: &str = ".partial";
/// `<version>@<unix seconds>` of the last failed install.
const FAILED_UPDATE_FILE: &str = "failed-update";
const RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;
/// Default parent of the per-worktree caches, relative to the work directory.
const DEFAULT_CACHE_DIR: &str = "caches";
const CACHEDIR_TAG_FILE: &str = "CACHEDIR.TAG";
/// See <https://bford.info/cachedir/>.
const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by the Zed Java (IntelliJ) extension.\n\
# For information about cache directory tags see https://bford.info/cachedir/\n";

/// The `server-bundle.json` shipped inside the JetBrains VS Code extension. It
/// pins the platform-specific language server archive the extension runs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerBundle {
    url: String,
    version: String,
    archive_name: String,
    /// SHA-256 of the archive at `url`.
    sha256: Option<String>,
}

/// The server launcher to run.
pub struct Launcher {
    pub path: PathBuf,
    /// Downloaded by the extension, so `path` is relative to its work
    /// directory; otherwise the user's own installation, outside the sandbox.
    pub managed: bool,
}

/// A path on the host, in the form the sandbox can take apart. Its paths are
/// Unix-style, so a Windows path splits only on `/`, which Windows accepts too.
fn host_path(path: &str) -> PathBuf {
    match current_platform().0 {
        Os::Windows => PathBuf::from(path.replace('\\', "/")),
        _ => PathBuf::from(path),
    }
}

/// The JetBrains "Java and Kotlin by IntelliJ IDEA" language server.
///
/// JetBrains publishes the server as a standalone archive, but only references
/// it from the VS Code extension on Open VSX. So the extension version on Open
/// VSX is what gets tracked for updates: each one names the server build it
/// runs. Installs live in `intellij-server/<extension-version>/`.
pub struct IntellijServer;

impl IntellijServer {
    pub fn new() -> Self {
        Self
    }

    /// Return the launcher to run, preferring the user's own installation.
    pub fn launcher(
        &mut self,
        language_server_id: &LanguageServerId,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> zed::Result<Launcher> {
        if let Some(path) = self.user_configured_path(configuration, worktree) {
            return Ok(Launcher {
                path: host_path(&path),
                managed: false,
            });
        }
        self.get_or_download(language_server_id, configuration, worktree)
            .map(|path| Launcher {
                path,
                managed: true,
            })
    }
}

impl Downloadable for IntellijServer {
    const INSTALL_PATH: &'static str = INSTALL_PATH;

    fn find_local(&self) -> Option<PathBuf> {
        let mut installs: Vec<(String, PathBuf)> = read_dir(INSTALL_PATH)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir() && entry.file_name() != VSIX_DIR)
            .filter(|entry| {
                !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(PARTIAL_SUFFIX)
            })
            .filter_map(|entry| {
                let launcher = find_launcher(&entry.path())?;
                Some((entry.file_name().to_string_lossy().into_owned(), launcher))
            })
            .collect();
        installs.sort_by(|(left, _), (right, _)| compare_versions(left, right));
        installs.pop().map(|(_, launcher)| launcher)
    }

    fn fetch_latest_version(&self, _worktree: &Worktree) -> zed::Result<String> {
        let url = format!("{OPEN_VSX_EXTENSION}/{}/latest", open_vsx_target()?);
        let response = fetch(
            &HttpRequest::builder()
                .method(HttpMethod::Get)
                .url(url)
                .build()?,
        )
        .map_err(|err| format!("Failed to query Open VSX for the latest server: {err}"))?;
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|err| format!("Malformed Open VSX response: {err}"))?;
        body.get("version")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "Open VSX response has no version".to_string())
    }

    fn download(
        &mut self,
        version: &str,
        language_server_id: &LanguageServerId,
        _worktree: &Worktree,
    ) -> zed::Result<PathBuf> {
        let install_path = Path::new(INSTALL_PATH).join(version);
        if let Some(launcher) = find_launcher(&install_path) {
            return Ok(launcher);
        }
        // Each attempt fetches ~400 MB; while there is a server to fall back on,
        // a failed update waits a day instead of repeating on every start.
        let failure_path = Path::new(INSTALL_PATH).join(FAILED_UPDATE_FILE);
        if failed_recently(&failure_path, version) && self.find_local().is_some() {
            return Err(format!(
                "The update to IntelliJ server {version} failed less than a day ago; not retrying yet"
            ));
        }

        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Downloading,
        );
        let has_local = self.find_local().is_some();
        let installed = install(version, &install_path, has_local);
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        match installed {
            Ok(launcher) => {
                let _ = fs::remove_file(&failure_path);
                remove_old_installs(version);
                Ok(launcher)
            }
            Err(err) => {
                record_failure(&failure_path, version);
                Err(err)
            }
        }
    }

    fn user_configured_path(
        &self,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> Option<String> {
        get_server_launcher(configuration, worktree).or_else(|| worktree.which(&launcher_exec()))
    }
}

/// Download, verify and unpack a server version, which appears at
/// `install_path` only once complete.
fn install(version: &str, install_path: &Path, has_local: bool) -> zed::Result<PathBuf> {
    let bundle = read_server_bundle(version)?;
    let kind = archive_type(&bundle.archive_name)?;
    let download_path = Path::new(INSTALL_PATH).join(format!("{version}{DOWNLOAD_SUFFIX}"));
    let partial_path = Path::new(INSTALL_PATH).join(format!("{version}{PARTIAL_SUFFIX}"));
    remove_leftovers()?;
    if install_path.exists() {
        fs::remove_dir_all(install_path)
            .map_err(|err| format!("Failed to remove incomplete install: {err}"))?;
    }

    zed::download_file(
        &bundle.url,
        &path_to_string(&download_path)?,
        DownloadedFileType::Uncompressed,
    )
    .map_err(|err| {
        format!(
            "Failed to download IntelliJ server {} from {}: {err}",
            bundle.version, bundle.url
        )
    })?;
    let unpacked = verify_archive(&download_path, &bundle, has_local).and_then(|()| {
        archive::extract(&download_path, kind, &partial_path)
            .map_err(|err| format!("Failed to unpack {}: {err}", bundle.archive_name))
    });
    let _ = fs::remove_file(&download_path);
    let prepared = unpacked.and_then(|executables| prepare(&partial_path, executables, &bundle));
    if let Err(err) = prepared {
        let _ = fs::remove_dir_all(&partial_path);
        return Err(err);
    }
    // Only a complete, runnable install gets the version's name.
    fs::rename(&partial_path, install_path)
        .map_err(|err| format!("Failed to install IntelliJ server: {err}"))?;
    find_launcher(install_path)
        .ok_or_else(|| format!("IntelliJ server {version} lost its launcher while installing"))
}

/// Make an unpacked server runnable: restore the executable bits the archive
/// records, and check that the launcher is there.
fn prepare(dir: &Path, executables: Vec<PathBuf>, bundle: &ServerBundle) -> zed::Result<()> {
    if current_platform().0 != Os::Windows {
        for path in executables {
            zed::make_file_executable(&path_to_string(dir.join(path))?)?;
        }
    }
    let launcher = find_launcher(dir).ok_or_else(|| {
        format!(
            "Downloaded IntelliJ server archive {} has no bin/{}",
            bundle.archive_name,
            launcher_exec()
        )
    })?;
    // Archives have carried their modes so far; cover the essentials anyway.
    make_executables(&launcher)
}

/// Keep the new version and the one before it, which servers started before
/// the update may still be running from.
fn remove_old_installs(current: &str) {
    let mut versions: Vec<String> = read_dir(INSTALL_PATH)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir() && entry.file_name() != VSIX_DIR)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != current && !name.ends_with(PARTIAL_SUFFIX))
        .collect();
    versions.sort_by(|left, right| compare_versions(right, left));
    let previous = versions
        .iter()
        .position(|name| compare_versions(name, current).is_lt());
    for (index, name) in versions.iter().enumerate() {
        if Some(index) != previous
            && let Err(err) = fs::remove_dir_all(Path::new(INSTALL_PATH).join(name))
        {
            println!("Failed to remove IntelliJ server {name}: {err}");
        }
    }
}

/// Whether installing `version` failed within the last day.
fn failed_recently(failure_path: &Path, version: &str) -> bool {
    let Ok(record) = fs::read_to_string(failure_path) else {
        return false;
    };
    let Some((failed_version, failed_at)) = record.trim().split_once('@') else {
        return false;
    };
    failed_version == version
        && failed_at
            .parse::<u64>()
            .is_ok_and(|failed_at| unix_now().saturating_sub(failed_at) < RETRY_AFTER_SECONDS)
}

fn record_failure(failure_path: &Path, version: &str) {
    let _ = fs::write(failure_path, format!("{version}@{}", unix_now()));
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Download the VS Code extension (a ~2 MB zip) only to read which server
/// archive it pins, then drop it again.
fn read_server_bundle(version: &str) -> zed::Result<ServerBundle> {
    let target = open_vsx_target()?;
    let vsix_path = Path::new(INSTALL_PATH).join(VSIX_DIR).join(version);
    let url = format!(
        "{OPEN_VSX_EXTENSION}/{target}/{version}/file/JetBrains.intellij-server-{version}@{target}.vsix"
    );
    if vsix_path.exists() {
        fs::remove_dir_all(&vsix_path)
            .map_err(|err| format!("Failed to remove stale VSIX directory: {err}"))?;
    }
    create_path_if_not_exists(vsix_path.parent().unwrap_or(Path::new(INSTALL_PATH)))?;
    zed::download_file(&url, &path_to_string(&vsix_path)?, DownloadedFileType::Zip)
        .map_err(|err| format!("Failed to download {url}: {err}"))?;

    let bundle = fs::read_to_string(vsix_path.join(SERVER_BUNDLE_FILE))
        .map_err(|err| format!("VSIX {version} has no {SERVER_BUNDLE_FILE}: {err}"))
        .and_then(|json| {
            serde_json::from_str::<ServerBundle>(&json)
                .map_err(|err| format!("Malformed {SERVER_BUNDLE_FILE}: {err}"))
        });
    let _ = fs::remove_dir_all(&vsix_path);
    bundle
}

/// Check the archive against the checksum `server-bundle.json` pins. Bundles
/// have always had one. Should a future one not, an update is refused, as the
/// installed server keeps working; a first install goes ahead unverified
/// rather than leaving no server at all.
fn verify_archive(path: &Path, bundle: &ServerBundle, has_local: bool) -> zed::Result<()> {
    let Some(expected) = bundle.sha256.as_deref() else {
        if has_local {
            return Err(format!(
                "{SERVER_BUNDLE_FILE} has no sha256; not updating to unverified {}",
                bundle.archive_name
            ));
        }
        println!(
            "{SERVER_BUNDLE_FILE} has no sha256; installing {} unverified",
            bundle.archive_name
        );
        return Ok(());
    };
    let actual = archive::sha256_file(path)
        .map_err(|err| format!("Failed to read the downloaded server archive: {err}"))?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        return Err(format!(
            "Downloaded IntelliJ server archive {} is corrupt: SHA-256 {actual}, expected {expected}",
            bundle.archive_name
        ));
    }
    Ok(())
}

/// Drop what an interrupted install left behind: downloads and partial unpacks.
fn remove_leftovers() -> zed::Result<()> {
    create_path_if_not_exists(INSTALL_PATH)?;
    for entry in read_dir(INSTALL_PATH)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.ends_with(DOWNLOAD_SUFFIX) {
            let _ = fs::remove_file(&path);
        } else if name.ends_with(PARTIAL_SUFFIX) {
            let _ = fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

/// Open VSX target platform of the JetBrains extension for the current host.
fn open_vsx_target() -> zed::Result<&'static str> {
    Ok(match current_platform() {
        (Os::Linux, Architecture::X8664) => "linux-x64",
        (Os::Linux, Architecture::Aarch64) => "linux-arm64",
        (Os::Mac, Architecture::X8664) => "darwin-x64",
        (Os::Mac, Architecture::Aarch64) => "darwin-arm64",
        (Os::Windows, Architecture::X8664) => "win32-x64",
        (Os::Windows, Architecture::Aarch64) => "win32-arm64",
        (os, arch) => {
            return Err(format!(
                "IntelliJ server is not available for {os:?} {arch:?}"
            ));
        }
    })
}

/// JetBrains ships `.tar.gz` for Linux, `.win.zip` for Windows and `.sit`
/// (a plain zip) for macOS.
fn archive_type(archive_name: &str) -> zed::Result<ArchiveKind> {
    if archive_name.ends_with(".tar.gz") {
        Ok(ArchiveKind::TarGz)
    } else if archive_name.ends_with(".zip") || archive_name.ends_with(".sit") {
        Ok(ArchiveKind::Zip)
    } else {
        Err(format!(
            "Unsupported IntelliJ server archive: {archive_name}"
        ))
    }
}

fn launcher_exec() -> String {
    match current_platform().0 {
        Os::Windows => format!("{LAUNCHER_NAME}.exe"),
        _ => LAUNCHER_NAME.to_string(),
    }
}

/// Archives unpack to `intellij-server-<build>/`, so accept the launcher at
/// the install root or one directory below it.
fn find_launcher(install_path: &Path) -> Option<PathBuf> {
    let launcher = Path::new("bin").join(launcher_exec());
    let is_file = |path: &Path| metadata(path).is_ok_and(|metadata| metadata.is_file());

    let direct = install_path.join(&launcher);
    if is_file(&direct) {
        return Some(direct);
    }
    read_dir(install_path)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&launcher))
        .find(|path| is_file(path))
}

/// Return the server root (the parent of `bin/`) for a launcher path.
pub fn server_home(launcher: &Path) -> Option<&Path> {
    launcher.parent()?.parent()
}

/// Restore the executable bits archive extraction may have dropped: the
/// launcher, and the bundled JetBrains Runtime it starts.
fn make_executables(launcher: &Path) -> zed::Result<()> {
    if current_platform().0 == Os::Windows {
        return Ok(());
    }
    zed::make_file_executable(&path_to_string(launcher)?)?;

    let Some(home) = server_home(launcher) else {
        return Ok(());
    };
    for jbr_home in [home.join("jbr"), home.join("jbr/Contents/Home")] {
        let mut executables: Vec<PathBuf> = read_dir(jbr_home.join("bin"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        executables.push(jbr_home.join("lib/jspawnhelper"));
        for path in executables.iter().filter(|path| path.is_file()) {
            zed::make_file_executable(&path_to_string(path)?)?;
        }
    }
    Ok(())
}

/// The value the launcher expects for `--eula`: the first 16 hex digits of the
/// SHA-256 of the bundled `EULA.txt`.
pub fn eula_hash(launcher: &Path) -> zed::Result<String> {
    let eula_path = server_home(launcher)
        .map(|home| home.join(EULA_FILE))
        .ok_or_else(|| format!("Cannot locate {EULA_FILE} next to {}", launcher.display()))?;
    let eula = fs::read(&eula_path)
        .map_err(|err| format!("Failed to read {}: {err}", eula_path.display()))?;
    Ok(hex::encode(Sha256::digest(eula))[..16].to_string())
}

/// IntelliJ caches and indexes for a worktree, kept apart per project like the
/// VS Code extension's workspace storage.
pub fn get_system_path(configuration: &Option<Value>, worktree: &Worktree) -> zed::Result<PathBuf> {
    let base_directory = match get_system_directory(configuration, worktree)? {
        Some(directory) => PathBuf::from(directory),
        None => default_cache_directory()?,
    };
    Ok(build_system_path(&base_directory, &worktree.root_path()))
}

/// The cache parent used when `system_directory` is not set: a directory in
/// the extension's work directory, the only place Zed lets an extension write.
/// It is tagged as a cache so backup tools that honour `CACHEDIR.TAG` skip it.
fn default_cache_directory() -> zed::Result<PathBuf> {
    create_path_if_not_exists(DEFAULT_CACHE_DIR)?;
    let tag = Path::new(DEFAULT_CACHE_DIR).join(CACHEDIR_TAG_FILE);
    if !tag.is_file() {
        // Only a hint for backup tools; the cache works without it.
        let _ = fs::write(&tag, CACHEDIR_TAG);
    }
    // The server runs outside the sandbox, so it needs the absolute path.
    std::env::current_dir()
        .map(|path| path.join(DEFAULT_CACHE_DIR))
        .map_err(|err| format!("Failed to get current directory: {err}"))
}

fn build_system_path(base_directory: &Path, cache_key: &str) -> PathBuf {
    let digest = hex::encode(Sha1::digest(cache_key.as_bytes()));
    base_directory.join(format!("intellij-server-{digest}"))
}

/// Order Open VSX versions numerically (`0.0.10` after `0.0.9`).
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let parts = |version: &str| -> Vec<u64> {
        version
            .split(['.', '-'])
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    parts(left).cmp(&parts(right))
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    #[test]
    fn archive_types_follow_jetbrains_naming() {
        assert!(matches!(
            archive_type("intellij-server-263.6379.0.tar.gz"),
            Ok(ArchiveKind::TarGz)
        ));
        assert!(matches!(
            archive_type("intellij-server-263.6379.0-aarch64.sit"),
            Ok(ArchiveKind::Zip)
        ));
        assert!(matches!(
            archive_type("intellij-server-263.6379.0.win.zip"),
            Ok(ArchiveKind::Zip)
        ));
        assert!(archive_type("intellij-server.dmg").is_err());
    }

    #[test]
    fn server_bundle_parses_vsix_metadata() {
        let bundle: ServerBundle = serde_json::from_str(
            r#"{
              "url": "https://download.jetbrains.com/language-server/intellij-server/263.6379.0/intellij-server-263.6379.0.tar.gz",
              "version": "263.6379.0",
              "archiveName": "intellij-server-263.6379.0.tar.gz",
              "sha256": "7529cc733d4a020e1c1369183b2f60e135c85b8f074bbf27d1476a5a7b3839c3"
            }"#,
        )
        .unwrap();
        assert_eq!(bundle.version, "263.6379.0");
        assert_eq!(bundle.archive_name, "intellij-server-263.6379.0.tar.gz");
        assert_eq!(
            bundle.sha256.as_deref(),
            Some("7529cc733d4a020e1c1369183b2f60e135c85b8f074bbf27d1476a5a7b3839c3")
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(compare_versions("0.0.10", "0.0.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.0", "0.0.13"), Ordering::Greater);
        assert_eq!(compare_versions("0.0.13", "0.0.13"), Ordering::Equal);
    }

    #[test]
    fn system_path_is_unique_per_worktree() {
        let base = Path::new("/cache");
        let first = build_system_path(base, "/work/a");
        let second = build_system_path(base, "/work/b");
        assert_ne!(first, second);
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("intellij-server-")
        );
    }

    #[test]
    fn server_home_is_parent_of_bin() {
        let launcher = Path::new("/x/intellij-server-1/bin/intellij-server");
        assert_eq!(
            server_home(launcher),
            Some(Path::new("/x/intellij-server-1"))
        );
    }
}
