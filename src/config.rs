use std::path::Path;

use zed_extension_api::{Os, Worktree, current_platform, serde_json::Value};

use crate::util::expand_home_path;

const DEFAULT_CACHE_RETENTION_DAYS: u64 = 90;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum CheckUpdates {
    #[default]
    Always,
    Once,
    Never,
}

pub fn get_java_home(configuration: &Option<Value>, worktree: &Worktree) -> Option<String> {
    // try to read the value from settings
    if let Some(configuration) = configuration
        && let Some(java_home) = configuration
            .pointer("/java_home")
            .or_else(|| configuration.pointer("/java/home")) // legacy support
            .and_then(|x| x.as_str())
    {
        match expand_home_path(worktree, java_home.to_string()) {
            Ok(home_path) => return Some(home_path),
            Err(err) => {
                println!("{err}");
            }
        };
    }

    // try to read the value from env
    match worktree
        .shell_env()
        .into_iter()
        .find(|(k, _)| k == "JAVA_HOME")
    {
        Some((_, value)) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn configured_system_directory(configuration: &Option<Value>) -> Result<Option<&str>, String> {
    let Some(value) = configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/system_directory"))
    else {
        return Ok(None);
    };

    let path = value
        .as_str()
        .ok_or_else(|| "system_directory must be a string".to_string())?;
    if path.trim().is_empty() {
        return Err("system_directory must not be empty".to_string());
    }

    Ok(Some(path))
}

/// macOS and Linux rely on [`Path::is_absolute`].
///
/// Windows requires custom logic to recognize:
/// - Drive paths: C:\... or C:/...
/// - UNC paths: \\server\share\... or //server/share/..
fn is_absolute_directory(path: &str, os: Os) -> bool {
    match os {
        Os::Windows => {
            let bytes = path.as_bytes();
            path.starts_with(r"\\")
                || path.starts_with("//")
                || (bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && matches!(bytes[2], b'/' | b'\\'))
        }
        Os::Mac | Os::Linux => Path::new(path).is_absolute(),
    }
}

fn validate_system_directory(path: String, os: Os) -> Result<String, String> {
    if is_absolute_directory(&path, os) {
        Ok(path)
    } else {
        Err(format!("system_directory must be an absolute path: {path}"))
    }
}

/// Returns the parent directory where per-worktree IntelliJ caches and indexes are stored.
pub fn get_system_directory(
    configuration: &Option<Value>,
    worktree: &Worktree,
) -> Result<Option<String>, String> {
    let Some(data_directory) = configured_system_directory(configuration)? else {
        return Ok(None);
    };
    let path = expand_home_path(worktree, data_directory.to_string())
        .map_err(|err| format!("Failed to expand system_directory: {err}"))?;

    validate_system_directory(path, current_platform().0).map(Some)
}

pub fn is_java_autodownload(configuration: &Option<Value>) -> bool {
    configuration
        .as_ref()
        .and_then(|configuration| {
            configuration
                .pointer("/jdk_auto_download")
                .and_then(|enabled| enabled.as_bool())
        })
        .unwrap_or(false)
}

pub fn get_check_updates(configuration: &Option<Value>) -> CheckUpdates {
    if let Some(configuration) = configuration
        && let Some(mode_str) = configuration
            .pointer("/check_updates")
            .and_then(|x| x.as_str())
            .map(|s| s.to_lowercase())
    {
        return match mode_str.as_str() {
            "once" => CheckUpdates::Once,
            "never" => CheckUpdates::Never,
            "always" => CheckUpdates::Always,
            _ => CheckUpdates::default(),
        };
    }
    CheckUpdates::default()
}

/// Path to a user-provided `intellij-server` launcher (`<server>/bin/intellij-server`).
pub fn get_server_launcher(configuration: &Option<Value>, worktree: &Worktree) -> Option<String> {
    configured_path(configuration, worktree, "/server_path")
}

/// Whether the user accepted the JetBrains EAP agreement, letting the extension
/// pass the bundled EULA hash to the launcher.
pub fn is_eula_accepted(configuration: &Option<Value>) -> bool {
    configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/accept_eula"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether to turn `jar:` and `jrt:` library locations into files Zed can
/// open. On by default.
pub fn is_library_sources_enabled(configuration: &Option<Value>) -> bool {
    configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/library_sources"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Days a project cache or library file may go unused before it is deleted;
/// `0` keeps them forever. Defaults to 90.
pub fn get_cache_retention_days(configuration: &Option<Value>) -> u64 {
    configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/cache_retention_days"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_CACHE_RETENTION_DAYS)
}

/// Extra JVM options for the server, passed through `IJ_JAVA_OPTIONS`.
pub fn get_jvm_args(configuration: &Option<Value>) -> Vec<String> {
    configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/jvm_args"))
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Telemetry level, `full` or `anonymous`; anything else sends nothing.
pub fn get_data_sharing(configuration: &Option<Value>) -> Option<String> {
    configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/data_sharing"))
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "full" | "anonymous"))
        .map(str::to_string)
}

/// Region for JetBrains product terms and data processing.
pub fn get_region(configuration: &Option<Value>) -> Option<String> {
    configuration
        .as_ref()
        .and_then(|configuration| configuration.pointer("/region"))
        .and_then(Value::as_str)
        .filter(|value| {
            matches!(
                *value,
                "africa" | "americas" | "apac" | "china" | "europe" | "middle_east" | "oceania"
            )
        })
        .map(str::to_string)
}

fn configured_path(
    configuration: &Option<Value>,
    worktree: &Worktree,
    pointer: &str,
) -> Option<String> {
    let path = configuration.as_ref()?.pointer(pointer)?.as_str()?;
    match expand_home_path(worktree, path.to_string()) {
        Ok(path) => Some(path),
        Err(err) => {
            println!("{err}");
            None
        }
    }
}

/// Path to a local `gradle-lsp-bridge` binary, overriding the downloaded one.
/// Primarily used to test a locally-built bridge.
pub fn get_gradle_bridge_path(
    configuration: &Option<Value>,
    worktree: &Worktree,
) -> Option<String> {
    configured_path(configuration, worktree, "/gradle_bridge_path")
}

#[cfg(test)]
mod tests {
    use zed_extension_api::serde_json::json;

    use zed_extension_api::Os;

    use super::{configured_system_directory, is_absolute_directory, validate_system_directory};

    #[test]
    fn configured_data_directory_distinguishes_absent_configuration() {
        assert_eq!(configured_system_directory(&None), Ok(None));
        assert_eq!(configured_system_directory(&Some(json!({}))), Ok(None));
    }

    #[test]
    fn configured_data_directory_accepts_non_empty_strings() {
        let configuration = Some(json!({ "system_directory": "/tmp/intellij" }));

        assert_eq!(
            configured_system_directory(&configuration),
            Ok(Some("/tmp/intellij"))
        );
    }

    #[test]
    fn configured_data_directory_rejects_empty_values() {
        let empty_error =
            configured_system_directory(&Some(json!({ "system_directory": "" }))).unwrap_err();
        let whitespace_error =
            configured_system_directory(&Some(json!({ "system_directory": "  " }))).unwrap_err();

        assert_eq!(empty_error, "system_directory must not be empty");
        assert_eq!(whitespace_error, "system_directory must not be empty");
    }

    #[test]
    fn configured_data_directory_rejects_non_string_values() {
        let error =
            configured_system_directory(&Some(json!({ "system_directory": true }))).unwrap_err();

        assert_eq!(error, "system_directory must be a string");
    }

    #[test]
    fn data_directory_validation_rejects_relative_paths() {
        assert_eq!(
            validate_system_directory("tmp/intellij".to_string(), Os::Linux),
            Err("system_directory must be an absolute path: tmp/intellij".to_string())
        );
        assert_eq!(
            validate_system_directory(r"Opt\zed-intellij".to_string(), Os::Windows),
            Err(r"system_directory must be an absolute path: Opt\zed-intellij".to_string())
        );
    }

    #[test]
    fn data_directory_requires_platform_absolute_paths() {
        assert!(is_absolute_directory("/tmp/intellij", Os::Linux));
        assert!(!is_absolute_directory("tmp/intellij", Os::Linux));
        assert!(is_absolute_directory(r"C:\Opt\zed-intellij", Os::Windows));
        assert!(is_absolute_directory("C:/Opt/zed-intellij", Os::Windows));
        assert!(is_absolute_directory(
            r"\\server\share\zed-intellij",
            Os::Windows
        ));
        assert!(!is_absolute_directory(r"Opt\zed-intellij", Os::Windows));
    }
}
