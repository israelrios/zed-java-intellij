# Java Extension for Zed (IntelliJ language server)

A fork of the [Zed Java extension](https://github.com/zed-extensions/java) that replaces the Eclipse JDT Language Server (JDTLS) with JetBrains' **IntelliJ language server**, the server behind the [Java and Kotlin by IntelliJ IDEA](https://open-vsx.org/extension/JetBrains/intellij-server) extension for VS Code.

It keeps everything else from upstream: the Java, `.properties`, Gradle (Groovy) and Gradle Kotlin DSL grammars, the run/test tasks, and the Gradle Language Server for build files.

> [!IMPORTANT]
> The IntelliJ language server is not open source. Since its 1.0 release it needs a JetBrains license, such as an IntelliJ IDEA Ultimate subscription or a trial; see [Licensing](#licensing). Earlier preview (EAP) builds were covered by the *JetBrains LSP Extension Public EAP Agreement* and expired 30 days after release; the extension checks Open VSX for newer builds (see `check_updates`).

## Quick Start

1. Install the extension as a dev extension (see [Developing Locally](#developing-locally)).
2. Open a Java project. On first start, the extension downloads the server, which is about 400 MB and includes its own JetBrains Runtime, so no JDK is needed to run it.
3. Zed then asks you to accept the JetBrains agreement. Choose **Read the Agreement** to open the bundled `EULA.txt` in Zed, then **Accept** to start the server.

Your answer is recorded for that version of the agreement, so Zed asks again only when a server update changes the agreement. If you choose **Decline** or dismiss the prompt, the server doesn't start; run `editor: restart language server` to be asked again. Answer within Zed's language server request timeout (2 minutes by default). If you take longer, Zed gives up on the server, and you need to restart it.

To accept without being asked, for example on a fresh machine, set `accept_eula`:

```jsonc
"lsp": {
  "intellij-server": {
    "settings": {
      "accept_eula": true
    }
  }
}
```

This accepts whichever agreement the downloaded build ships with, including later versions. Either way, the extension starts the server with `--eula <hash>`, where the hash is computed from the downloaded `EULA.txt`.

4. Without a license, Zed then asks you to activate one; see [Licensing](#licensing).

Projects that use Maven, Gradle or Bazel are imported automatically. Open the Zed project at the build root (where `pom.xml`, `settings.gradle` or `MODULE.bazel` is).

## Configuration

```jsonc
"lsp": {
  "intellij-server": {
    "settings": {
      // Accept the JetBrains EAP agreement without being asked in Zed; see
      // Quick Start.
      "accept_eula": true,

      // A JetBrains license for the server; see "Licensing" below. Each one
      // overrides the same INTELLIJ_SERVER_LICENSE / INTELLIJ_SERVER_LICENSE_FILE /
      // JETBRAINS_LICENSE_SERVER variable from your shell.
      // An activation code (license key text):
      "license_key": "ABCD1234-...",
      // ...or a file holding it; ~ is expanded:
      "license_key_file": "~/.config/jetbrains/intellij-server.key",
      // ...or a JetBrains License Server (floating licenses):
      "license_server": "https://license.example.com",

      // Extra JVM options for the server, passed through IJ_JAVA_OPTIONS.
      // The server's own default heap is -Xmx2048m.
      "jvm_args": ["-Xmx4g"],

      // Parent directory for IntelliJ caches and indexes. The extension adds an
      // intellij-server-<worktree-hash> directory for each worktree. Must be an
      // absolute path. Default: $XDG_CACHE_HOME or ~/.cache (Linux),
      // ~/Library/Caches (macOS), %LOCALAPPDATA% (Windows). See "Caches" below.
      "system_directory": "/path/to/intellij-caches",

      // Days before an unused project cache or library file is deleted; 0 keeps
      // them forever. Default: 90.
      "cache_retention_days": 90,

      // Telemetry sent to JetBrains: "full", "anonymous" or "none" (default).
      "data_sharing": "none",
      // Region for JetBrains product terms and data processing: africa,
      // americas, apac, china, europe, middle_east or oceania.
      "region": "europe",

      // Open library sources (JDK and dependencies) from go-to-definition,
      // references and symbol search. See "Library sources" below.
      "library_sources": true,

      // When to look on Open VSX for a newer server build:
      // - "always" (default): at most once every 24 hours
      // - "once": only if no local installation exists
      // - "never": only use an existing installation (errors if missing)
      "check_updates": "always",

      // Use your own server installation instead of the managed download.
      // An `intellij-server` executable on your PATH is picked up the same way.
      "server_path": "/path/to/intellij-server-263.6379.0/bin/intellij-server"
    },

    // Passed unchanged to the server as LSP initializationOptions.
    "initialization_options": {
      // JDK used to resolve symbols when the project doesn't define one.
      "defaultSdk": "/path/to/jdk",
      // Build tool per workspace folder URI: "gradle", "maven", "bazel", "jps",
      // or "" to turn project import off.
      "buildTools": { "file:///path/to/project": "maven" },
      // Projects to import in addition to the auto-detected ones.
      "projects": [
        { "type": "maven", "path": "file:///path/to/project/pom.xml", "profiles": ["dev"] }
      ]
    }
  }
}
```

A server set with `server_path` or found on `PATH` lies outside the extension's sandbox, so the extension can't read its EULA. Accept it once yourself; the launcher remembers the acceptance for your user and machine:

```sh
intellij-server --show-eula
intellij-server --eula <hash>   # the hash is printed when the server refuses to start
```

### How the server is obtained

JetBrains publishes the server only through the VS Code extension. To find the current build, this extension:

1. asks Open VSX for the latest `JetBrains.intellij-server` version for your platform;
2. downloads that VSIX (about 2 MB) and reads the server archive's URL and SHA-256 from `extension/server-bundle.json`;
3. downloads that archive (about 390 MB) from `download.jetbrains.com`, checks its SHA-256, and unpacks it into the extension's work directory under `intellij-server/<version>/`.

Updates happen the same way, when a language server starts and `check_updates` allows it:

- Servers already running keep going on the build they started with. The new build is used from the next server start, such as reopening the project or running `editor: restart language server`.
- The previous build is kept for servers that are still running on it. Older builds are deleted.
- An install appears only once it is complete, so an interrupted download leaves nothing half-installed and is cleaned up on the next attempt.
- If the download fails, the checksum doesn't match, or `server-bundle.json` has no checksum, the installed build keeps being used, and the update is retried after 24 hours. Only a first install, with no build to fall back on, goes ahead without a checksum.

## Licensing

From version 1.0, the server needs a JetBrains license. Without one it starts, but code insight (completion, navigation, diagnostics and so on) is paused until a license is activated.

When that happens, Zed asks you to activate a license, and keeps the prompt open until you answer:

- **Sign In with JetBrains Account** opens the JetBrains sign-in page in your browser. Once you sign in, the server uses a license from your account; if the account has a valid subscription that isn't in use yet, the first one is used.
- **Start Free Trial** starts an anonymous trial.
- **Other Options** opens this section, and asks again.
- **Not Now** stops asking until the language server restarts (`editor: restart language server`).

A license activated this way is kept for your user and machine, in `~/.config/JetBrains/IntelliJServer/intellij-server.key` on Linux. Zed tells you once the server reports the license as active; if code insight doesn't resume, run `editor: restart language server`.

To use an activation code, a key file or a license server instead, set `license_key`, `license_key_file` or `license_server` (see [Configuration](#configuration)) and restart the language server. These are held in memory only, never stored, and a key set this way overrides any other license; if it isn't valid, Zed says so, and you need to fix or remove it. The same values can come from the `INTELLIJ_SERVER_LICENSE`, `INTELLIJ_SERVER_LICENSE_FILE` and `JETBRAINS_LICENSE_SERVER` environment variables.

You can also manage the stored license from a terminal with the server's launcher, which lives in the extension's work directory under `intellij-server/<version>/intellij-server-*/bin/` (or is your `server_path`):

```sh
intellij-server license status
intellij-server license login --code       # sign in with a pasted code, e.g. over SSH
intellij-server license activate <code|key-file|license-server-url>
intellij-server license trial
intellij-server license reset
```

## Kotlin

The server also handles Kotlin. The "Kotlin" language itself comes from Zed's [Kotlin extension](https://github.com/zed-extensions/kotlin), so install it too, and make `intellij-server` its language server:

```jsonc
"languages": {
  "Kotlin": { "language_servers": ["intellij-server", "!kotlin-language-server", "!kotlin-lsp"] }
}
```

Kotlin files are only analyzed when the build sets them up as Kotlin sources: for Maven, the `kotlin-maven-plugin` and a `kotlin-stdlib` dependency. Otherwise they open without diagnostics.

## Library sources

The server points into libraries with `jar:` and `jrt:` URIs, for example `jar:///…/lib/src.zip!/java.base/java/util/List.java`. Zed can't open those, so the extension runs the server behind a small proxy, [`proxy/IntellijServerProxy.java`](proxy/IntellijServerProxy.java):

- When a server message names a library URI, the proxy asks the server for the file's text with its `decompile` command. It writes the text to a read-only file under `<system_directory>/intellij-server-library-sources/` and hands Zed that file instead. The file's path repeats the library URI, so Zed shows where it comes from: `jar:///usr/lib/jvm/jdk-21/lib/src.zip!/java.base/java/util/List.java` opens as `…/intellij-server-library-sources/<hash>/usr/lib/jvm/jdk-21/lib/src.zip!/java.base/java/util/List.java`.
- When Zed refers to one of those files, the proxy swaps the original URI back in, so the server keeps treating it as a library file. Hover, go-to-definition and references work inside it as well.
- A reply waits at most 5 seconds for files. URIs that aren't ready by then are left unchanged, and the files are written in the background for next time.
- The files are shared by all projects. A file from a jar is tied to the jar's size and modification time, so a rebuilt snapshot jar gets fresh text. Other files, such as JDK classes reached through `jrt:`, are kept per project.

The proxy is a single Java source file. It runs on the JetBrains Runtime that ships with the server, so it needs no build step or separate download. On its first start it runs from source and compiles itself into `proxy.jar` in the background, and later starts use the jar (about 40 ms instead of 600 ms). The proxy also cleans up the caches (see below), so it always runs. Setting `library_sources` to `false` only makes it pass library URIs through unchanged.

## Caches

The server keeps its indexes in a directory per project, a few hundred MB each, inside `system_directory`:

| Path | Contents |
| --- | --- |
| `intellij-server-<hash>/` | Caches and indexes for one project; `<hash>` is the SHA-1 of the project path. |
| `intellij-server-library-sources/` | Library files opened through go-to-definition, shared by all projects. |

`system_directory` defaults to `~/.cache` on Linux (or `$XDG_CACHE_HOME`), `~/Library/Caches` on macOS and `%LOCALAPPDATA%` on Windows.

Each time a server starts, the proxy deletes, in the background:

- caches of projects whose directory no longer exists;
- caches of projects not opened for `cache_retention_days` (90 by default);
- library files nobody opened in that time.

Each project cache records its project path and last use in a `.zed-project` file. Set `cache_retention_days` to `0` to keep everything. The **Clear default IntelliJ server cache** task removes all caches at once.

The server download itself, about 1 GB unpacked, lives in Zed's extension work directory: `~/.local/share/zed/extensions/work/java-intellij/` on Linux, `~/Library/Application Support/Zed/extensions/work/java-intellij/` on macOS and `%LOCALAPPDATA%\Zed\extensions\work\java-intellij\` on Windows.

## Differences from upstream

- **Language server:** IntelliJ instead of JDTLS. The server bundles IntelliJ's Lombok plugin, so there's no Lombok agent, `java-lsp-proxy` or Corretto download for the Java server.
- **No debugger.** Upstream's debugger is a JDTLS plugin. The IntelliJ server has its own debug adapter, but its port is only available through the server's `start_debug_server` LSP command, and Zed extensions can't send LSP requests.

## Gradle Build Files

For **Groovy** build scripts (`.gradle`) the extension runs Microsoft's [Gradle Language Server](https://github.com/microsoft/vscode-gradle), giving you completions for Gradle DSL closures, plugin-contributed blocks (e.g. `java {}`, `application {}`), Maven Central dependency coordinates, and syntax diagnostics.

To resolve the *plugin-aware* parts of the model (which plugins are applied, the closures/methods they contribute, and the script classpath), the language server needs the resolved build model. The extension obtains this exactly the way vscode-gradle 3.18 does: a small native binary, `gradle-lsp-bridge`, drives the bundled `gradle-server` over JSON-RPC using a named pipe on Windows or a Unix-domain socket on Unix. Build-model messages are protobuf-encoded and carried as base64 payloads inside the JSON-RPC messages. The bridge keeps a single `gradle-server` process (and its Gradle daemon) warm for the lifetime of the session, so re-syncs after a build-file save are fast. Both the language server and the bridge are downloaded and managed automatically — no configuration is required. This fork does not build the bridge itself; it downloads the binary published with the upstream [zed-extensions/java](https://github.com/zed-extensions/java) release it was forked from (`v6.8.28`).

### Kotlin DSL (`.gradle.kts`)

The Gradle Language Server itself is Groovy-only, so it does **not** provide completions or semantic tokens for Kotlin-DSL build scripts. For `.gradle.kts` files the extension instead provides:

- **Syntax highlighting** via the bundled Kotlin grammar.
- **Build-evaluation diagnostics** — when `gradle-server` configures the project, Gradle's own Kotlin-DSL compiler reports errors (unresolved references, invalid dependency notations, etc.). The bridge surfaces these as squiggles on the build file with the correct line.

(The Groovy language server's own — invalid — diagnostics for these files are suppressed, since it cannot parse Kotlin.)

Configuration, when you need it, goes under the `gradle-language-server` language server in your `settings.json` (note: this is a **different** block from `intellij-server`):

```jsonc
"lsp": {
  "gradle-language-server": {
    "settings": {
      // All optional — sensible defaults are used when omitted.

      // JDK used to run gradle-server and the Gradle daemon. Falls back to the
      // $JAVA_HOME environment variable. Modern Gradle requires JVM 17+, so set
      // this if your default Java is older. Also accepts the legacy key
      // "java.home".
      "java_home": "/path/to/your/JDK17+",

      // Gradle distribution (mirrors the Gradle Language Server's own schema).
      // By default the project's Gradle wrapper is used.
      "gradleWrapperEnabled": true,
      "gradleVersion": null,        // pin a version when the wrapper is disabled
      "gradleHome": null,           // a local Gradle installation directory
      "gradleUserHome": null,       // overrides GRADLE_USER_HOME
      "gradle_jvm_arguments": null, // e.g. "-Xmx2G" for the Gradle build

      // Uses the same always/once/never policy as `intellij-server`. In "always" mode,
      // successful version checks are cached for 24 hours.
      "check_updates": "always",

      // Path to a locally built gradle-lsp-bridge binary, overriding the
      // managed download. Primarily for development (see Developing Locally).
      "gradle_bridge_path": "/path/to/your/gradle-lsp-bridge"
    }
  }
}
```

> **Note:** The bridge launches `gradle-server` with the JDK resolved from this server's own settings — the `java_home` value under `gradle-language-server` (falling back to the `$JAVA_HOME` environment variable, then an auto-downloaded JDK). Modern Gradle requires JVM 17+, so ensure that JDK satisfies your Gradle distribution.

## Launch Scripts (aka Tasks) in Windows

This extension provides tasks for running your application and tests from within Zed via little play buttons next to tests/entry points. However, due to current limitiations of Zed's extension interface, we can not provide scripts that will work across Maven and Gradle on both Windows and Unix-compatible systems, so out of the box the launch scripts only work on Mac and Linux.

There is a fairly straightforward fix that you can apply to make it work on Windows by supplying your own task scripts. Please see [this Issue](https://github.com/zed-extensions/java/issues/94) for information on how to do that and read the [Tasks section in Zeds documentation](https://zed.dev/docs/tasks) for more information.


## External Formatters

If you prefer to use an external formatting tool like `google-java-format` or `palantir-java-format` instead of the IntelliJ formatter, you can configure Zed to run these tools on save or format commands.

### Configuring google-java-format

To use `google-java-format` as your external formatter:

1. **Installation**: Download the pre-built native binary for your platform from the [GitHub releases page](https://github.com/google/google-java-format/releases). Place it in a directory that is in your system's `PATH` (e.g., `/usr/local/bin` or `~/.local/bin`), rename it to `google-java-format` (or `google-java-format.exe` on Windows), and ensure it is marked as executable.
   - Alternatively, on macOS, you can install it via Homebrew: `brew install google-java-format`
2. Add the configuration to your Zed `settings.json`:

```json
"languages": {
  "Java": {
    "formatter": {
      "external": {
        "command": "google-java-format",
        "arguments": ["-"]
      }
    }
  }
}
```

If you prefer to format using the AOSP style (4-space indentation), you can pass the `--aosp` flag:

```json
"languages": {
  "Java": {
    "formatter": {
      "external": {
        "command": "google-java-format",
        "arguments": ["--aosp", "-"]
      }
    }
  }
}
```

If you installed it to a custom location not on your `PATH`, specify the absolute path for the `command`:

```json
"languages": {
  "Java": {
    "formatter": {
      "external": {
        "command": "/path/to/google-java-format",
        "arguments": ["-"]
      }
    }
  }
}
```

### Configuring palantir-java-format

To use `palantir-java-format` as your external formatter:

1. **Installation**:
   - **macOS / Linux**: Download the native binary for your architecture from the [Maven Central repository page](https://repo1.maven.org/maven2/com/palantir/javaformat/palantir-java-format-native/). Rename it to `palantir-java-format`, mark it as executable, and place it in a directory in your system's `PATH`.
   - **Windows**: Palantir does not distribute official pre-built native binaries for Windows. You must compile the native executable yourself from [palantir-java-format repository](https://github.com/palantir/palantir-java-format).
2. Add the configuration to your Zed `settings.json` (note that the `--palantir` option is required):

```json
"languages": {
  "Java": {
    "formatter": {
      "external": {
        "command": "palantir-java-format",
        "arguments": ["--palantir", "-"]
      }
    }
  }
}
```

### Running Formatters via JAR File (Alternative)

If a native executable is not available for your platform (or you prefer not to compile/install one), you can run the formatters using their `.jar` files (e.g. the all-deps/shadow JARs) by configuring Zed to launch `java` directly:

#### Using google-java-format JAR

```json
"languages": {
  "Java": {
    "formatter": {
      "external": {
        "command": "java",
        "arguments": ["-jar", "/path/to/google-java-format-all-deps.jar", "-"]
      }
    }
  }
}
```

#### Using palantir-java-format JAR

```json
"languages": {
  "Java": {
    "formatter": {
      "external": {
        "command": "java",
        "arguments": [
          "-jar",
          "/path/to/palantir-java-format-all-deps.jar",
          "--palantir",
          "-"
        ]
      }
    }
  }
}
```

## Developing Locally

See the [Zed documentation on developing extensions](https://zed.dev/docs/extensions/developing-extensions).

### Prerequisites

- [Rust](https://rustup.rs/) toolchain, with the `wasm32-wasip2` target: `rustup target add wasm32-wasip2`
- [just](https://github.com/casey/just) command runner (optional)

### Installing as a Dev Extension

1. Clone this repository.
2. In Zed, open the extensions panel (`zed: extensions` in the command palette), click **Install Dev Extension**, and select the cloned folder.
3. If the upstream **Java** extension is installed, uninstall it, or turn off its server for Java, so the two don't both run.

   ```jsonc
   "languages": {
     "Java": { "language_servers": ["intellij-server", "!jdtls", "..."] }
   }
   ```

   Zed may still offer to install the upstream `java` extension when it restores `.java` files at startup, before extensions have loaded. Click **No, don't install it** once and Zed stops asking.

After changing the source, run **Rebuild Dev Extension** from the command palette.

### Using the `justfile`

| Recipe | Description |
|--------|-------------|
| `just ext-build` | Build the WASM extension in release mode |
| `just fmt` | Format all code (Rust + tree-sitter queries) |
| `just clippy` | Run clippy |
| `just lint` | Format and lint all code |
| `just all` | Lint and build |

## License

The extension code is Apache-2.0, like upstream. The IntelliJ language server it downloads is proprietary JetBrains software under its own agreement.
