use std::{
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use zed_extension_api::{
    self as zed, CodeLabel, LanguageServerId, Os, Worktree, current_platform,
    lsp::{Completion, Symbol},
    serde_json::{Value, json},
    settings::LspSettings,
};

use crate::{
    config::{
        get_cache_retention_days, get_data_sharing, get_jvm_args, get_license_key,
        get_license_key_file, get_license_server, get_region, is_eula_accepted,
        is_library_sources_enabled,
    },
    intellij::{IntellijServer as Bundle, eula_hash, get_system_path, server_home},
    labels,
    language_server::LanguageServer,
    util::{create_path_if_not_exists, path_to_string, remove_all_directories_except},
};

/// Environment variables the launcher reads. `IJ_LAUNCHER_DEBUG` is dropped
/// because its output goes to stdout, which is the LSP channel in stdio mode.
const JVM_OPTIONS_ENV: &str = "IJ_JAVA_OPTIONS";
const LAUNCHER_DEBUG_ENV: &str = "IJ_LAUNCHER_DEBUG";
const DATA_SHARING_ENV: &str = "INTELLIJ_DATA_SHARING";
const REGION_ENV: &str = "INTELLIJ_REGION";
const LICENSE_KEY_ENV: &str = "INTELLIJ_SERVER_LICENSE";
const LICENSE_KEY_FILE_ENV: &str = "INTELLIJ_SERVER_LICENSE_FILE";
const LICENSE_SERVER_ENV: &str = "JETBRAINS_LICENSE_SERVER";

const PROXY_SOURCE: &str = include_str!("../proxy/IntellijServerProxy.java");
const PROXY_DIR: &str = "intellij-server-proxy";
/// Where the proxy lived while it was named `LibrarySourcesProxy`.
const OLD_PROXY_DIR: &str = "library-sources-proxy";
const PROXY_SOURCE_FILE: &str = "IntellijServerProxy.java";
const PROXY_MAIN_CLASS: &str = "IntellijServerProxy";
/// Compiled by the proxy itself on its first run; see `IntellijServerProxy`.
const PROXY_JAR_FILE: &str = "proxy.jar";
/// Library files the proxy writes, shared by all projects, next to their
/// system paths.
const LIBRARY_SOURCES_DIR: &str = "intellij-server-library-sources";
/// System properties configuring the proxy; see `IntellijServerProxy`.
const PROJECT_PROPERTY: &str = "zed.intellij.proxy.project";
const PROJECT_CACHE_PROPERTY: &str = "zed.intellij.proxy.project.cache";
const RETENTION_PROPERTY: &str = "zed.intellij.proxy.retention.days";
const REWRITE_PROPERTY: &str = "zed.intellij.proxy.rewrite";
const AGREEMENT_HASH_PROPERTY: &str = "zed.intellij.proxy.agreement.hash";
const AGREEMENT_FILE_PROPERTY: &str = "zed.intellij.proxy.agreement.file";
const AGREEMENT_RECORD_PROPERTY: &str = "zed.intellij.proxy.agreement.record";
const EULA_FILE: &str = "EULA.txt";
/// The `--eula` hash of the agreement the user accepted in Zed, written by
/// the proxy.
const AGREEMENT_RECORD_FILE: &str = "intellij-server/eula-accepted";
/// The proxy only shuffles messages; keep its JVM small and quick to start.
const PROXY_JVM_ARGS: &[&str] = &["-XX:+UseSerialGC", "-XX:TieredStopAtLevel=1", "-Xmx128m"];

pub struct IntellijServer {
    pub bundle: Bundle,
}

impl IntellijServer {
    pub fn new() -> Self {
        Self {
            bundle: Bundle::new(),
        }
    }
}

impl LanguageServer for IntellijServer {
    const SERVER_ID: &'static str = "intellij-server";

    fn command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<zed::Command> {
        let configuration = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|lsp_settings| lsp_settings.settings);

        let launcher = self
            .bundle
            .launcher(language_server_id, &configuration, worktree)?;

        let mut args = vec!["--stdio".to_string()];

        let system_path = get_system_path(&configuration, worktree)
            .map_err(|err| format!("Failed to determine IntelliJ system path: {err}"))?;
        args.push("--system-path".to_string());
        args.push(path_to_string(&system_path)?);

        // Downloads live in the extension's work directory and are addressed
        // relative to it. A user-provided installation is outside the sandbox,
        // so its EULA can't be read here; it is accepted by running the launcher
        // with `--eula` once, which the launcher remembers.
        //
        // For a download, the agreement is accepted either in the settings or
        // in Zed when the server first starts: the proxy asks, and records the
        // answer for this version of the agreement.
        let mut agreement_prompt = None;
        if launcher.managed {
            let hash = eula_hash(&launcher.path)?;
            if is_eula_accepted(&configuration) || is_agreement_recorded(&hash) {
                args.push("--eula".to_string());
                args.push(hash);
            } else {
                let eula = server_home(&launcher.path)
                    .map(|home| home.join(EULA_FILE))
                    .ok_or_else(|| {
                        format!(
                            "Cannot locate {EULA_FILE} next to {}",
                            launcher.path.display()
                        )
                    })?;
                agreement_prompt = Some((hash, eula));
            }
        }

        let mut env: Vec<(String, String)> = worktree
            .shell_env()
            .into_iter()
            .filter(|(key, _)| {
                ![LAUNCHER_DEBUG_ENV, DATA_SHARING_ENV, REGION_ENV].contains(&key.as_str())
            })
            .collect();

        let jvm_args = get_jvm_args(&configuration);
        if !jvm_args.is_empty() {
            let extra = jvm_args
                .iter()
                .map(|arg| shell_quote_if_needed(arg))
                .collect::<Vec<_>>()
                .join(" ");
            match env.iter_mut().find(|(key, _)| key == JVM_OPTIONS_ENV) {
                Some((_, current)) if !current.is_empty() => {
                    *current = format!("{current} {extra}")
                }
                Some((_, current)) => *current = extra,
                None => env.push((JVM_OPTIONS_ENV.to_string(), extra)),
            }
        }
        if let Some(data_sharing) = get_data_sharing(&configuration) {
            env.push((DATA_SHARING_ENV.to_string(), data_sharing));
        }
        if let Some(region) = get_region(&configuration) {
            env.push((REGION_ENV.to_string(), region));
        }
        // A license from the settings replaces one from the shell.
        for (key, value) in [
            (LICENSE_KEY_ENV, get_license_key(&configuration)),
            (
                LICENSE_KEY_FILE_ENV,
                get_license_key_file(&configuration, worktree),
            ),
            (LICENSE_SERVER_ENV, get_license_server(&configuration)),
        ] {
            if let Some(value) = value {
                env.retain(|(existing, _)| existing != key);
                env.push((key.to_string(), value));
            }
        }

        // Zed spawns the server from the worktree, so a path relative to the
        // extension's work directory must be made absolute first.
        let current_dir = std::env::current_dir()
            .map_err(|err| format!("Failed to get current directory: {err}"))?;
        let launcher = if launcher.managed {
            current_dir.join(&launcher.path)
        } else {
            launcher.path
        };

        // Run the server behind the proxy, on the server's own runtime:
        // `java <properties> <proxy> <library-sources-dir> <launcher> <args...>`.
        let cache_root = system_path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", system_path.display()))?;
        let mut proxy_args: Vec<String> =
            PROXY_JVM_ARGS.iter().map(|arg| arg.to_string()).collect();
        proxy_args.extend([
            format!("-D{PROJECT_PROPERTY}={}", worktree.root_path()),
            format!(
                "-D{PROJECT_CACHE_PROPERTY}={}",
                path_to_string(&system_path)?
            ),
            format!(
                "-D{RETENTION_PROPERTY}={}",
                get_cache_retention_days(&configuration)
            ),
            format!(
                "-D{REWRITE_PROPERTY}={}",
                is_library_sources_enabled(&configuration)
            ),
        ]);
        if let Some((hash, eula)) = agreement_prompt {
            proxy_args.extend([
                format!("-D{AGREEMENT_HASH_PROPERTY}={hash}"),
                format!(
                    "-D{AGREEMENT_FILE_PROPERTY}={}",
                    path_to_string(current_dir.join(eula))?
                ),
                format!(
                    "-D{AGREEMENT_RECORD_PROPERTY}={}",
                    path_to_string(current_dir.join(AGREEMENT_RECORD_FILE))?
                ),
            ]);
        }
        proxy_args.extend(proxy_launch_args(&current_dir, &launcher)?);
        proxy_args.push(path_to_string(cache_root.join(LIBRARY_SOURCES_DIR))?);
        proxy_args.push(path_to_string(&launcher)?);
        proxy_args.extend(args);

        Ok(zed::Command {
            command: path_to_string(bundled_java(&launcher)?)?,
            args: proxy_args,
            env,
        })
    }

    fn label_for_completion(
        &self,
        _language_server_id: &LanguageServerId,
        completion: Completion,
    ) -> Option<CodeLabel> {
        labels::label_for_completion(&completion)
    }

    fn label_for_symbol(
        &self,
        _language_server_id: &LanguageServerId,
        symbol: Symbol,
    ) -> Option<CodeLabel> {
        labels::label_for_symbol(&symbol)
    }

    /// The server reads `defaultSdk`, `buildTools`, `projects` and
    /// `disableRocksDBWriteAheadLog`; they are passed through as configured.
    fn initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Option<Value>> {
        let options = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .map(|lsp_settings| lsp_settings.initialization_options)
            .map_err(|err| format!("Failed to get LSP settings for worktree: {err}"))?
            .unwrap_or_else(|| json!({}));
        if !options.is_object() {
            return Err("initialization_options is not a JSON object".to_string());
        }
        Ok(Some(options))
    }
}

/// Whether the user accepted this version of the agreement when asked in Zed.
fn is_agreement_recorded(hash: &str) -> bool {
    fs::read_to_string(AGREEMENT_RECORD_FILE).is_ok_and(|recorded| recorded.trim() == hash)
}

/// Arguments that start the proxy: from its compiled jar when an earlier
/// run has built it, from source otherwise (about 600 ms slower). The proxy
/// lives in a directory named after its source and the server it runs with,
/// so neither an extension nor a server update runs a jar built for another.
fn proxy_launch_args(current_dir: &Path, launcher: &Path) -> zed::Result<Vec<String>> {
    let mut hasher = Sha256::new();
    hasher.update(PROXY_SOURCE.as_bytes());
    hasher.update(path_to_string(launcher)?.as_bytes());
    let version = hex::encode(hasher.finalize())[..16].to_string();
    let dir = Path::new(PROXY_DIR).join(&version);
    let source = dir.join(PROXY_SOURCE_FILE);
    let jar = dir.join(PROXY_JAR_FILE);

    let mut args = Vec::new();
    if jar.is_file() {
        args.extend([
            "-cp".to_string(),
            path_to_string(current_dir.join(&jar))?,
            PROXY_MAIN_CLASS.to_string(),
        ]);
        return Ok(args);
    }

    if !fs::read_to_string(&source).is_ok_and(|current| current == PROXY_SOURCE) {
        create_path_if_not_exists(&dir)?;
        fs::write(&source, PROXY_SOURCE)
            .map_err(|err| format!("Failed to write {}: {err}", source.display()))?;
        let _ = remove_all_directories_except(PROXY_DIR, &version);
        let _ = fs::remove_dir_all(OLD_PROXY_DIR);
    }
    let source = path_to_string(current_dir.join(&source))?;
    args.extend([
        format!("-Dzed.intellij.proxy.source={source}"),
        format!(
            "-Dzed.intellij.proxy.jar={}",
            path_to_string(current_dir.join(&jar))?
        ),
        source,
    ]);
    Ok(args)
}

/// The JetBrains Runtime shipped in every server archive, next to `bin/`.
fn bundled_java(launcher: &Path) -> zed::Result<PathBuf> {
    let home = server_home(launcher)
        .ok_or_else(|| format!("Cannot locate the server home of {}", launcher.display()))?;
    Ok(match current_platform().0 {
        Os::Mac => home.join("jbr/Contents/Home/bin/java"),
        Os::Linux => home.join("jbr/bin/java"),
        Os::Windows => home.join("jbr").join("bin").join("java.exe"),
    })
}

/// Quote a JVM option the way the launcher splits `IJ_JAVA_OPTIONS`.
fn shell_quote_if_needed(arg: &str) -> String {
    let plain = arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._=:/@-".contains(c));
    if plain {
        return arg.to_string();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::shell_quote_if_needed;

    #[test]
    fn plain_jvm_options_stay_unquoted() {
        assert_eq!(shell_quote_if_needed("-Xmx4g"), "-Xmx4g");
        assert_eq!(
            shell_quote_if_needed("-Dfoo.bar=/tmp/x:y@z"),
            "-Dfoo.bar=/tmp/x:y@z"
        );
    }

    #[test]
    fn jvm_options_with_spaces_are_quoted() {
        assert_eq!(shell_quote_if_needed("-Dname=a b"), "\"-Dname=a b\"");
        assert_eq!(shell_quote_if_needed("-Dx=$HOME"), "\"-Dx=\\$HOME\"");
    }
}
