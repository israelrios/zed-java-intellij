use std::{
    fs::{self, metadata},
    path::PathBuf,
};

use zed_extension_api::{
    self as zed, DownloadedFileType, LanguageServerId, LanguageServerInstallationStatus, Worktree,
    serde_json::Value, set_language_server_installation_status,
};

use crate::{
    config::get_gradle_bridge_path,
    util::{
        NATIVE_BIN_DIR, find_native_binary_in_release, platform_asset_name, platform_exec_name,
        remove_all_directories_except,
    },
};

const BRIDGE_BINARY: &str = "gradle-lsp-bridge";
const BRIDGE_INSTALL_PATH: &str = NATIVE_BIN_DIR;
const GITHUB_REPO: &str = "zed-extensions/java";
/// This fork doesn't build the bridge; it uses the binary from the upstream
/// release it was forked from.
const UPSTREAM_RELEASE: &str = "v6.8.28";

/// Downloads and locates the `gradle-lsp-bridge` binary — the native process
/// that bridges Zed to the Gradle Language Server and drives the real
/// `gradle-server.jar` over JSON-RPC. Downloaded from the upstream
/// zed-extensions/java release this extension was forked from.
pub struct GradleBridge;

impl GradleBridge {
    pub fn new() -> Self {
        Self
    }

    pub fn binary_path(
        &mut self,
        configuration: &Option<Value>,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<String> {
        let path = self.get_or_download(language_server_id, configuration, worktree)?;
        Ok(path.to_string_lossy().to_string())
    }

    /// Find a development override or the bridge matching the extension release.
    fn find_local(&self) -> Option<PathBuf> {
        find_native_binary_in_release(&bridge_exec(), UPSTREAM_RELEASE)
    }

    /// Download and validate the bridge asset for the specified extension release.
    fn download(
        &mut self,
        version: &str,
        language_server_id: &LanguageServerId,
    ) -> zed::Result<PathBuf> {
        let (name, file_type) = asset_name()?;
        let bin_path = format!("{BRIDGE_INSTALL_PATH}/{version}/{}", bridge_exec());

        if metadata(&bin_path).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(PathBuf::from(bin_path));
        }
        if metadata(&bin_path).is_ok_and(|metadata| metadata.is_dir()) {
            fs::remove_dir_all(&bin_path)
                .map_err(|err| format!("Failed to remove invalid bridge path {bin_path}: {err}"))?;
        }

        let version_dir = format!("{BRIDGE_INSTALL_PATH}/{version}");
        let download_url =
            format!("https://github.com/{GITHUB_REPO}/releases/download/{version}/{name}");

        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Downloading,
        );

        zed::download_file(&download_url, &version_dir, file_type)
            .map_err(|err| format!("Failed to download bridge: {err}"))?;

        if !metadata(&bin_path).is_ok_and(|metadata| metadata.is_file()) {
            return Err(format!(
                "Downloaded bridge archive did not contain {bin_path}"
            ));
        }
        zed::make_file_executable(&bin_path)
            .map_err(|err| format!("Failed to make bridge executable at {bin_path}: {err}"))?;
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        let _ = remove_all_directories_except(BRIDGE_INSTALL_PATH, version);

        Ok(PathBuf::from(bin_path))
    }

    /// Resolve an explicit override, reuse the extension-bound bridge, or download it.
    fn get_or_download(
        &mut self,
        language_server_id: &LanguageServerId,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> zed::Result<PathBuf> {
        if let Some(path) = get_gradle_bridge_path(configuration, worktree) {
            return Ok(PathBuf::from(path));
        }

        if let Some(path) = self.find_local() {
            return Ok(path);
        }

        self.download(UPSTREAM_RELEASE, language_server_id)
    }
}

fn asset_name() -> zed::Result<(String, DownloadedFileType)> {
    platform_asset_name(BRIDGE_BINARY)
}

fn bridge_exec() -> String {
    platform_exec_name(BRIDGE_BINARY)
}
