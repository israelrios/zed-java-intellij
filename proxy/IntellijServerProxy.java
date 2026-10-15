import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystemNotFoundException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.attribute.FileTime;
import java.security.MessageDigest;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.regex.Pattern;
import java.util.stream.Stream;
import javax.tools.ToolProvider;

/**
 * Sits between Zed and the IntelliJ language server, adding what Zed lacks: asking for the
 * JetBrains agreement and for a license, opening library sources, and cleaning up caches.
 *
 * <p>The server points into libraries with {@code jar:} and {@code jrt:} URIs, which Zed can't
 * open. For every such URI in a server message, the proxy fetches the text with the server's
 * {@code decompile} command, writes it to a read-only file under the cache directory, and
 * replaces the URI with that file's {@code file:} URI. Messages from Zed that mention one of
 * these files get the original URI back, so the server keeps treating them as library documents
 * and navigation inside them works.
 *
 * <p>The cache directory is shared by all projects. A file is keyed by its URI and by the size and
 * modification time of the archive behind it, so a rebuilt snapshot jar gets fresh text; anything
 * else is kept apart per project.
 *
 * <p>The proxy also looks after the caches, which nothing else would clean up: it marks the
 * project's IntelliJ system directory as in use, and deletes the system directories of projects
 * that no longer exist or went unused for the retention period, along with library files nobody
 * opened in that time.
 *
 * <p>It also stands in for the licensing UI of JetBrains' VS Code extension. It tells the server
 * that the client has one, so the server reports its licensing state instead of a passing message,
 * and while code insight is paused for want of a license, it keeps asking in Zed until the user
 * signs in with a JetBrains Account, starts a trial, or turns the prompt down.
 *
 * <p>Runs with the Java runtime bundled with the server as a single-file source program:
 * {@code java IntellijServerProxy.java <cache-dir> <server-command> [server-args...]}, configured
 * through system properties:
 *
 * <ul>
 *   <li>{@code zed.intellij.proxy.project}: the project directory;
 *   <li>{@code zed.intellij.proxy.project.cache}: its IntelliJ system directory, which sits
 *       next to those of other projects;
 *   <li>{@code zed.intellij.proxy.retention.days}: days before unused caches go, 0 to keep
 *       them;
 *   <li>{@code zed.intellij.proxy.rewrite}: {@code false} to pass library URIs through;
 *   <li>{@code zed.intellij.proxy.agreement.hash}, {@code .file} and {@code .record}: when set,
 *       the server only starts once the user accepts the JetBrains EAP agreement in {@code .file},
 *       and the proxy then records {@code .hash}, the launcher's {@code --eula} value, in
 *       {@code .record};
 *   <li>{@code zed.intellij.proxy.source} and {@code zed.intellij.proxy.jar}: compiling the
 *       source costs about a second on each start, so when both are set, the first run also
 *       compiles itself into that jar, and later runs start from the jar instead.
 * </ul>
 */
public final class IntellijServerProxy {
    private static final String REQUEST_ID_PREFIX = "zed-intellij-proxy-";
    private static final String URI_FILE = ".uri";
    /** Part of every library file's key; bumped when the files' layout changes. */
    private static final String LAYOUT = "2";
    private static final long EXIT_GRACE_SECONDS = 5;
    private static final long HOOK_GRACE_SECONDS = 2;
    private static final String SOURCE_PROPERTY = "zed.intellij.proxy.source";
    private static final String JAR_PROPERTY = "zed.intellij.proxy.jar";
    private static final long MAX_WAIT_SECONDS = 5;
    private static final String PROJECT_PROPERTY = "zed.intellij.proxy.project";
    private static final String PROJECT_CACHE_PROPERTY = "zed.intellij.proxy.project.cache";
    private static final String RETENTION_PROPERTY = "zed.intellij.proxy.retention.days";
    private static final String REWRITE_PROPERTY = "zed.intellij.proxy.rewrite";
    private static final String AGREEMENT_HASH_PROPERTY = "zed.intellij.proxy.agreement.hash";
    private static final String AGREEMENT_FILE_PROPERTY = "zed.intellij.proxy.agreement.file";
    private static final String AGREEMENT_RECORD_PROPERTY = "zed.intellij.proxy.agreement.record";
    private static final String AGREEMENT_MESSAGE = "The IntelliJ language server is a JetBrains preview (EAP),"
            + " covered by the JetBrains LSP Extension Public EAP Agreement. It starts once you accept the"
            + " agreement.";
    private static final String DECLINED_MESSAGE = "The JetBrains EAP agreement was not accepted. Restart the"
            + " language server to be asked again.";
    private static final String ACCEPT = "Accept";
    private static final String READ = "Read the Agreement";
    private static final String DECLINE = "Decline";
    private static final String LICENSING_STATE = "jetbrains/licensing/state";
    private static final String LICENSING_DOCS = "https://github.com/israelrios/zed-java-intellij#licensing";
    /** Set when the license comes from the settings or the shell; it overrides any other. */
    private static final List<String> LICENSE_ENV = List.of("INTELLIJ_SERVER_LICENSE", "INTELLIJ_SERVER_LICENSE_FILE");
    private static final String SIGN_IN = "Sign In with JetBrains Account";
    private static final String TRIAL = "Start Free Trial";
    private static final String OTHER_OPTIONS = "Other Options";
    private static final String NOT_NOW = "Not Now";
    /** The server gives a browser sign-in five minutes. */
    private static final long SIGN_IN_TIMEOUT_MINUTES = 6;
    private static final long LICENSING_TIMEOUT_MINUTES = 1;
    /** Names the project a system directory belongs to; its modification time is the last use. */
    private static final String PROJECT_FILE = ".zed-project";
    private static final Pattern PROJECT_CACHE = Pattern.compile("intellij-server-[0-9a-f]{40}");
    /** A cache renamed for deletion, so no server picks it up half deleted. */
    private static final String DELETING_SUFFIX = ".deleting";
    /** Where library files were kept, per project, before they were shared. */
    private static final String OLD_LIBRARY_SOURCES = "library-sources";
    private static final long MARK_IN_USE_HOURS = 12;
    private static final long TOUCH_INTERVAL_MILLIS = TimeUnit.HOURS.toMillis(1);
    /** A failed decompile is tried again after this long, for example once the import is done. */
    private static final long RETRY_FAILED_MILLIS = TimeUnit.MINUTES.toMillis(1);

    private final Path cacheDir;
    /** Keeps library files apart per project when they can't be keyed by their archive. */
    private final String projectCache;
    private final boolean rewrite;
    private final OutputStream toClient;
    private final OutputStream toServer;
    private final AtomicLong nextRequestId = new AtomicLong();
    private volatile boolean exiting;
    private final Map<String, CompletableFuture<Object>> pendingRequests = new ConcurrentHashMap<>();
    /** Library URI to the file holding its text, once written. */
    private final Map<String, LibraryFile> libraryFiles = new ConcurrentHashMap<>();

    /** When each {@code .uri} sidecar was last marked as used, so it happens at most hourly. */
    private final Map<Path, Long> usedSidecars = new ConcurrentHashMap<>();

    /** The file for a library URI, under the directory named {@code key}, requested at {@code started}. */
    private record LibraryFile(String key, CompletableFuture<String> uri, long started) {
        /**
         * Still right for its URI: not deleted by another proxy's cleanup, and not a failure old
         * enough to try again (the project may not have been imported yet).
         */
        boolean isCurrent(String expectedKey) {
            if (!key.equals(expectedKey)) return false;
            if (!uri.isDone()) return true;
            if (uri.isCompletedExceptionally()) return System.currentTimeMillis() - started < RETRY_FAILED_MILLIS;
            return Files.exists(Path.of(URI.create(uri.join())));
        }
    }
    /**
     * Sends the proxy's own requests. The thread reading the server must never block writing to
     * it: if the server in turn blocked writing its output, both would wait forever.
     */
    private final ExecutorService serverWriter = Executors.newSingleThreadExecutor(runnable -> {
        Thread thread = new Thread(runnable, "to-server");
        thread.setDaemon(true);
        return thread;
    });
    /** Asks for a license, which waits on the user, away from both pumps. */
    private final ExecutorService licensing = Executors.newSingleThreadExecutor(runnable -> {
        Thread thread = new Thread(runnable, "licensing");
        thread.setDaemon(true);
        return thread;
    });
    /** The server's last licensing state, and how many it has sent. */
    private volatile Object licensingState;
    private final AtomicLong licensingStates = new AtomicLong();
    private final AtomicBoolean askingForLicense = new AtomicBoolean();
    /** The user chose not to be asked again until the server restarts. */
    private volatile boolean licensePromptDismissed;
    /** Code insight was paused, so a license that becomes valid is worth announcing. */
    private volatile boolean licenseMissing;

    private IntellijServerProxy(
            Path cacheDir, String projectCache, boolean rewrite, OutputStream toClient, OutputStream toServer) {
        this.cacheDir = cacheDir;
        this.projectCache = projectCache == null ? "" : projectCache;
        this.rewrite = rewrite;
        this.toClient = toClient;
        this.toServer = toServer;
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: IntellijServerProxy <cache-dir> <server-command> [args...]");
            System.exit(2);
        }
        Path cacheDir = Path.of(args[0]).toAbsolutePath().normalize();
        Files.createDirectories(cacheDir);
        // Claim the project's cache before anything uses it, so another proxy's cleanup leaves it.
        String projectCache = System.getProperty(PROJECT_CACHE_PROPERTY);
        Path systemDir = projectCache == null ? null : Path.of(projectCache).toAbsolutePath().normalize();
        String project = System.getProperty(PROJECT_PROPERTY, "");
        if (systemDir != null) markInUse(systemDir, project);

        List<String> command = new ArrayList<>(List.of(args).subList(1, args.length));
        List<byte[]> heldMessages = List.of();
        String agreementHash = System.getProperty(AGREEMENT_HASH_PROPERTY);
        if (agreementHash != null) {
            heldMessages = askForAgreement(System.in, System.out, agreementHash,
                    Path.of(System.getProperty(AGREEMENT_FILE_PROPERTY)),
                    Path.of(System.getProperty(AGREEMENT_RECORD_PROPERTY)));
            if (heldMessages == null) System.exit(0);
            command.addAll(List.of("--eula", agreementHash));
        }

        Process server = new ProcessBuilder(command)
                .redirectError(ProcessBuilder.Redirect.INHERIT)
                .start();
        for (byte[] message : heldMessages) {
            write(server.getOutputStream(), withLicensingUi(new String(message, StandardCharsets.UTF_8)));
        }
        // On SIGTERM, close the server's input (which it answers by exiting) before going away,
        // rather than leaving it to notice the closed pipe on its own.
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            if (!server.isAlive()) return;
            try {
                server.getOutputStream().close();
                if (!server.waitFor(HOOK_GRACE_SECONDS, TimeUnit.SECONDS)) server.destroy();
            } catch (IOException | InterruptedException e) {
                server.destroy();
            }
        }, "stop-server"));
        compileSelfInBackground();
        if (systemDir != null) startHousekeeping(cacheDir, systemDir, project);

        boolean rewrite = !"false".equals(System.getProperty(REWRITE_PROPERTY));
        var proxy = new IntellijServerProxy(cacheDir, projectCache, rewrite, System.out, server.getOutputStream());
        Thread fromServer = new Thread(() -> proxy.pumpFromServer(server.getInputStream()), "from-server");
        fromServer.setDaemon(true);
        fromServer.start();
        boolean exitSent = proxy.pumpFromClient(System.in);

        // The server ignores `exit` but stops once its input is closed, which the pump has done.
        if (!server.waitFor(EXIT_GRACE_SECONDS, TimeUnit.SECONDS)) {
            server.destroy();
            if (!server.waitFor(EXIT_GRACE_SECONDS, TimeUnit.SECONDS)) server.destroyForcibly();
        }
        System.exit(exitSent ? 0 : 1);
    }

    /**
     * Asks in Zed for the JetBrains EAP agreement to be accepted before the server starts. Zed's
     * {@code initialize} is held back meanwhile; a server may prompt before answering it. Returns
     * the messages held back once the agreement is accepted, which is recorded for the extension,
     * or {@code null} once it is declined, after failing {@code initialize}.
     */
    static List<byte[]> askForAgreement(InputStream in, OutputStream out, String hash, Path agreement,
            Path record) throws IOException {
        List<byte[]> held = new ArrayList<>();
        byte[] initialize = readMessage(in);
        if (initialize == null) return null;
        held.add(initialize);
        String promptText = AGREEMENT_MESSAGE;
        for (int round = 1; ; round++) {
            String promptId = REQUEST_ID_PREFIX + "agreement-" + round;
            var prompt = new LinkedHashMap<String, Object>();
            prompt.put("type", new Json.Number("3"));
            prompt.put("message", promptText);
            prompt.put("actions", List.of(Map.of("title", ACCEPT), Map.of("title", READ), Map.of("title", DECLINE)));
            write(out, Json.write(request(promptId, "window/showMessageRequest", prompt)));
            Object choice = field(awaitResponse(in, promptId, held), "title");
            if (READ.equals(choice)) {
                String showId = REQUEST_ID_PREFIX + "agreement-show-" + round;
                var show = new LinkedHashMap<String, Object>();
                show.put("uri", agreement.toUri().toString());
                show.put("takeFocus", Boolean.TRUE);
                write(out, Json.write(request(showId, "window/showDocument", show)));
                // An error response reads as null, like a failure.
                if (!Boolean.TRUE.equals(field(awaitResponse(in, showId, held), "success"))) {
                    promptText = AGREEMENT_MESSAGE + " Zed couldn't open the agreement; it is at " + agreement + ".";
                }
                continue;
            }
            if (ACCEPT.equals(choice)) {
                Files.createDirectories(record.getParent());
                Files.writeString(record, hash);
                return held;
            }
            // Declined, or the prompt was dismissed.
            Object initializeId = field(Json.parse(new String(initialize, StandardCharsets.UTF_8)), "id");
            write(out, Json.write(errorResponse(initializeId, DECLINED_MESSAGE)));
            byte[] body;
            while ((body = readMessage(in)) != null) {
                Object message = Json.parse(new String(body, StandardCharsets.UTF_8));
                if ("exit".equals(field(message, "method"))) break;
                if (field(message, "method") != null && field(message, "id") != null) {
                    write(out, Json.write(errorResponse(field(message, "id"), "The language server did not start.")));
                }
            }
            return null;
        }
    }

    /** Reads Zed's messages until the response to {@code id}, holding back anything else. */
    private static Object awaitResponse(InputStream in, String id, List<byte[]> held) throws IOException {
        byte[] body;
        while ((body = readMessage(in)) != null) {
            Object message = Json.parse(new String(body, StandardCharsets.UTF_8));
            if (id.equals(field(message, "id")) && field(message, "method") == null) {
                return field(message, "result");
            }
            held.add(body);
        }
        throw new java.io.EOFException("Zed closed the connection");
    }

    private static Map<String, Object> request(String id, String method, Object params) {
        var message = new LinkedHashMap<String, Object>();
        message.put("jsonrpc", "2.0");
        message.put("id", id);
        message.put("method", method);
        if (params != null) message.put("params", params);
        return message;
    }

    private static Map<String, Object> errorResponse(Object id, String text) {
        var error = new LinkedHashMap<String, Object>();
        error.put("code", new Json.Number("-32603"));
        error.put("message", text);
        var message = new LinkedHashMap<String, Object>();
        message.put("jsonrpc", "2.0");
        message.put("id", id);
        message.put("error", error);
        return message;
    }

    /** Forwards Zed's messages until the `exit` notification or end of input. */
    private boolean pumpFromClient(InputStream in) {
        try {
            byte[] body;
            while ((body = readMessage(in)) != null) {
                String text = new String(body, StandardCharsets.UTF_8);
                if (text.contains(REQUEST_ID_PREFIX) && completeOwnRequest(Json.parse(text))) {
                    continue;
                }
                text = withLicensingUi(text);
                // Set before forwarding, so a server that stops right away isn't taken for a crash.
                boolean isExit = text.contains("\"exit\"") && "exit".equals(field(Json.parse(text), "method"));
                if (isExit) exiting = true;
                // Only messages naming a cached file need rewriting; pass the rest through untouched.
                if (text.contains(cacheDir.getFileName().toString())) {
                    Object message = Json.parse(text);
                    write(toServer, Json.write(toLibraryUris(message)));
                } else {
                    write(toServer, text);
                }
                if (isExit) break;
            }
        } catch (IOException | RuntimeException e) {
            System.err.println("[proxy] client stream failed: " + e);
        }
        try {
            toServer.close();
        } catch (IOException e) {
            // The server is already gone.
        }
        return exiting;
    }

    private void pumpFromServer(InputStream in) {
        try {
            byte[] body;
            while ((body = readMessage(in)) != null) {
                String text = new String(body, StandardCharsets.UTF_8);
                if (text.contains(REQUEST_ID_PREFIX) && completeOwnRequest(Json.parse(text))) {
                    continue;
                }
                if (text.contains(LICENSING_STATE)) {
                    Object message = Json.parse(text);
                    if (LICENSING_STATE.equals(field(message, "method"))) {
                        onLicensingState(field(message, "params"));
                        continue;
                    }
                }
                if (!rewrite || (!text.contains("\"jar:") && !text.contains("\"jrt:"))) {
                    write(toClient, text);
                    continue;
                }
                Object message = Json.parse(text);
                List<String> uris = new ArrayList<>();
                collectLibraryUris(message, uris);
                List<CompletableFuture<String>> files = uris.stream().map(this::libraryFile).toList();
                // Don't hold a reply for long (a `references` result can name many
                // library files); whatever isn't ready keeps its library URI and
                // finishes in the background for next time.
                CompletableFuture.allOf(files.toArray(CompletableFuture[]::new))
                        .completeOnTimeout(null, MAX_WAIT_SECONDS, TimeUnit.SECONDS)
                        .whenComplete((ignored, error) -> {
                            try {
                                write(toClient, Json.write(toFileUris(message)));
                            } catch (IOException e) {
                                System.err.println("[proxy] failed to write to client: " + e);
                            }
                        });
            }
        } catch (IOException | RuntimeException e) {
            System.err.println("[proxy] server stream failed: " + e);
        }
        // After `exit`, the main thread stops the server and reports success.
        if (!exiting) System.exit(1);
    }

    private boolean completeOwnRequest(Object message) {
        if (!(field(message, "id") instanceof String id) || !id.startsWith(REQUEST_ID_PREFIX)) return false;
        if (field(message, "method") != null) return false;
        CompletableFuture<Object> pending = pendingRequests.remove(id);
        if (pending == null) return false;
        Object error = field(message, "error");
        if (error != null) {
            pending.completeExceptionally(new IOException(Json.write(error)));
        } else {
            pending.complete(field(message, "result"));
        }
        return true;
    }

    private CompletableFuture<Object> requestServer(String method, Object params) {
        String id = REQUEST_ID_PREFIX + nextRequestId.incrementAndGet();
        var future = new CompletableFuture<Object>();
        pendingRequests.put(id, future);
        String body = Json.write(request(id, method, params));
        serverWriter.execute(() -> {
            try {
                write(toServer, body);
            } catch (IOException e) {
                pendingRequests.remove(id);
                future.completeExceptionally(e);
            }
        });
        return future;
    }

    /** Sends a request to Zed; only called on the licensing thread. */
    private CompletableFuture<Object> requestClient(String method, Object params) {
        String id = REQUEST_ID_PREFIX + nextRequestId.incrementAndGet();
        var future = new CompletableFuture<Object>();
        pendingRequests.put(id, future);
        try {
            write(toClient, Json.write(request(id, method, params)));
        } catch (IOException e) {
            pendingRequests.remove(id);
            future.completeExceptionally(e);
        }
        return future;
    }

    /**
     * Adds the {@code licensingUi} client capability to Zed's {@code initialize}, so the server
     * reports its licensing state to the proxy rather than showing a message that soon goes away.
     */
    @SuppressWarnings("unchecked")
    static String withLicensingUi(String text) {
        if (!text.contains("\"initialize\"")) return text;
        Object message = Json.parse(text);
        if (!"initialize".equals(field(message, "method"))
                || !(field(message, "params") instanceof Map<?, ?> params)) {
            return text;
        }
        var capabilities = (Map<String, Object>) ((Map<String, Object>) params)
                .computeIfAbsent("capabilities", key -> new LinkedHashMap<String, Object>());
        var experimental = (Map<String, Object>) capabilities
                .computeIfAbsent("experimental", key -> new LinkedHashMap<String, Object>());
        experimental.put("licensingUi", Boolean.TRUE);
        return Json.write(message);
    }

    private void onLicensingState(Object state) {
        licensingState = state;
        licensingStates.incrementAndGet();
        if (!Boolean.TRUE.equals(field(state, "featuresDeclined"))) {
            if (licenseMissing && "Valid".equals(field(field(state, "activeLicense"), "status"))) {
                licenseMissing = false;
                showMessage(3, "JetBrains license activated (" + describeLicense(field(state, "activeLicense"))
                        + "). Code insight is back.");
            }
            return;
        }
        licenseMissing = true;
        startAskingForLicense();
    }

    private void startAskingForLicense() {
        if (licensePromptDismissed || !askingForLicense.compareAndSet(false, true)) return;
        licensing.execute(() -> {
            long handled = licensingStates.get();
            try {
                handled = askForLicense();
            } catch (RuntimeException e) {
                System.err.println("[licensing] failed to ask for a license: " + e);
            } finally {
                askingForLicense.set(false);
            }
            // A state that came while finishing found the prompt still busy and started none.
            if (licensingStates.get() != handled && Boolean.TRUE.equals(field(licensingState, "featuresDeclined"))) {
                startAskingForLicense();
            }
        });
    }

    /**
     * Asks in Zed for a license until the user picks a way to get one or turns the prompt down.
     * The server's next state tells whether the license took; this only reports what went wrong.
     * Returns the count of licensing states handled, as states that come while the server works
     * on the user's choice are superseded by it.
     */
    private long askForLicense() {
        String problem = null;
        while (true) {
            long handled = licensingStates.get();
            if (!Boolean.TRUE.equals(field(licensingState, "featuresDeclined"))) return handled;
            boolean fromEnvironment = LICENSE_ENV.stream().map(System::getenv)
                    .anyMatch(value -> value != null && !value.isBlank());
            String text = licensePrompt(licensingState, fromEnvironment) + (problem == null ? "" : " " + problem);
            // Signing in or starting a trial would not override a license from the environment.
            List<String> actions = fromEnvironment
                    ? List.of(OTHER_OPTIONS, NOT_NOW)
                    : List.of(SIGN_IN, TRIAL, OTHER_OPTIONS, NOT_NOW);
            var prompt = new LinkedHashMap<String, Object>();
            prompt.put("type", new Json.Number("2"));
            prompt.put("message", text);
            prompt.put("actions", actions.stream().map(title -> Map.of("title", title)).toList());
            Object choice;
            try {
                choice = field(requestClient("window/showMessageRequest", prompt).join(), "title");
            } catch (CompletionException e) {
                System.err.println("[licensing] Zed couldn't show the license prompt: " + e.getCause());
                return licensingStates.get();
            }
            problem = null;
            try {
                if (SIGN_IN.equals(choice)) {
                    problem = signIn();
                } else if (TRIAL.equals(choice)) {
                    Object result = requestServer("jetbrains/licensing/trial/startAnonymous", null)
                            .get(LICENSING_TIMEOUT_MINUTES, TimeUnit.MINUTES);
                    if (!Boolean.TRUE.equals(field(result, "isSuccess"))) {
                        problem = withDetail("Couldn't start the trial.", field(result, "errorMessage"));
                    }
                } else if (OTHER_OPTIONS.equals(choice)) {
                    var show = new LinkedHashMap<String, Object>();
                    show.put("uri", LICENSING_DOCS);
                    show.put("external", Boolean.TRUE);
                    try {
                        requestClient("window/showDocument", show).join();
                    } catch (CompletionException e) {
                        problem = "Zed couldn't open " + LICENSING_DOCS + ".";
                    }
                    continue;
                } else {
                    // Not now, or the prompt was dismissed.
                    licensePromptDismissed = true;
                    return licensingStates.get();
                }
            } catch (ExecutionException | TimeoutException e) {
                problem = withDetail("The language server didn't respond.", e.getMessage());
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return licensingStates.get();
            }
            // Done: the server announces the license; ask again only if something went wrong.
            if (problem == null) return licensingStates.get();
        }
    }

    /** Signs in with a JetBrains Account in the browser; returns what went wrong, if anything. */
    private String signIn() throws ExecutionException, TimeoutException, InterruptedException {
        Object result = requestServer("jetbrains/licensing/jba/login", null)
                .get(SIGN_IN_TIMEOUT_MINUTES, TimeUnit.MINUTES);
        Object status = field(result, "status");
        if ("Licensed".equals(status)) return null;
        if ("Canceled".equals(status)) return "The sign-in was canceled.";
        if ("NoSuitableLicense".equals(status)) {
            // Signed in, but no license is in use yet: use the first subscription that is valid. The
            // state is fetched, as its notification may not have come yet.
            Object state = requestServer("jetbrains/licensing/state/get", null)
                    .get(LICENSING_TIMEOUT_MINUTES, TimeUnit.MINUTES);
            Object subscriptions = field(field(state, "jbaLicensingState"), "availableSubscriptions");
            if (subscriptions instanceof List<?> list) {
                for (Object subscription : list) {
                    if ("Valid".equals(field(subscription, "status"))
                            && field(subscription, "assetId") instanceof String assetId) {
                        requestServer("jetbrains/licensing/subscriptions/use", Map.of("assetId", assetId))
                                .get(LICENSING_TIMEOUT_MINUTES, TimeUnit.MINUTES);
                        return null;
                    }
                }
            }
            return "Your JetBrains Account has no license that covers the IntelliJ language server.";
        }
        if ("NoLicenses".equals(status)) {
            return "No licenses available for activation were found in your JetBrains Account.";
        }
        return withDetail("Couldn't sign in with your JetBrains Account.", field(result, "errorMessage"));
    }

    static String licensePrompt(Object state, boolean fromEnvironment) {
        String reason = switch (String.valueOf(field(state, "parkedReason"))) {
            case "GraceExpired" -> "The JetBrains license grace period has ended.";
            case "LicenseRemoved" -> "The JetBrains license was removed.";
            case "LicenseRevoked" -> "The JetBrains license was revoked.";
            default -> fromEnvironment ? "The configured JetBrains license isn't valid." : "No JetBrains license is active.";
        };
        String text = reason + " Code insight is paused until a license is activated.";
        if (fromEnvironment) {
            text += " The license comes from the license_key or license_key_file setting (or the"
                    + " INTELLIJ_SERVER_LICENSE variables), which overrides any other; fix or remove it and"
                    + " restart the language server.";
        }
        return text;
    }

    private static String describeLicense(Object license) {
        String product = field(license, "product") instanceof String s ? s : "IntelliJ";
        return switch (String.valueOf(field(license, "source"))) {
            case "trial" -> product + " trial";
            case "licenseServer" -> product + " from " + field(license, "serverAddress");
            default -> field(license, "licensedTo") instanceof String to ? product + ", licensed to " + to : product;
        };
    }

    private static String withDetail(String text, Object detail) {
        return detail instanceof String s && !s.isBlank() ? text + " " + s.strip() : text;
    }

    private void showMessage(int type, String text) {
        var params = new LinkedHashMap<String, Object>();
        params.put("type", new Json.Number(String.valueOf(type)));
        params.put("message", text);
        var message = new LinkedHashMap<String, Object>();
        message.put("jsonrpc", "2.0");
        message.put("method", "window/showMessage");
        message.put("params", params);
        try {
            write(toClient, Json.write(message));
        } catch (IOException e) {
            System.err.println("[licensing] failed to write to client: " + e);
        }
    }

    /**
     * The `file:` URI holding the text of a library URI, written on first use, and again when the
     * archive behind it changes.
     */
    private CompletableFuture<String> libraryFile(String libraryUri) {
        String key = hash(LAYOUT + "\n" + libraryUri + "\n" + sourceVersion(libraryUri));
        LibraryFile file = libraryFiles.compute(libraryUri, (uri, known) -> known != null && known.isCurrent(key)
                ? known
                : new LibraryFile(key, writeLibraryFile(uri, cacheDir.resolve(key)), System.currentTimeMillis()));
        markUsed(cacheDir.resolve(key).resolve(URI_FILE));
        return file.uri();
    }

    /** Keeps a library file in use from looking unused to the cleanup; touches it at most hourly. */
    private void markUsed(Path sidecar) {
        long now = System.currentTimeMillis();
        Long last = usedSidecars.get(sidecar);
        if (last != null && now - last < TOUCH_INTERVAL_MILLIS) return;
        usedSidecars.put(sidecar, now);
        touch(sidecar);
    }

    /** Reuses the file an earlier session wrote in {@code dir}, or writes it. */
    private CompletableFuture<String> writeLibraryFile(String uri, Path dir) {
        Path existing = writtenFile(dir);
        if (existing != null) {
            touch(dir.resolve(URI_FILE));
            return CompletableFuture.completedFuture(existing.toUri().toString());
        }
        return requestServer("workspace/executeCommand",
                Map.of("command", "decompile", "arguments", List.of(uri))).thenApply(result -> {
            if (!(field(result, "code") instanceof String code)) {
                throw new IllegalStateException("no source for " + uri);
            }
            Path file = dir.resolve(libraryPath(uri, field(result, "language")));
            try {
                writeLibraryFile(dir, file, uri, code);
            } catch (IOException e) {
                throw new IllegalStateException(e);
            }
            return file.toUri().toString();
        });
    }

    /**
     * What the text behind a library URI depends on besides the URI: the size and modification
     * time of the archive it points into, or else the project, as another project may see other
     * contents behind the same URI.
     */
    private String sourceVersion(String uri) {
        Path archive = archivePath(uri);
        if (archive != null) {
            try {
                return Files.size(archive) + "@" + Files.getLastModifiedTime(archive).toMillis();
            } catch (IOException e) {
                // Not a local file after all.
            }
        }
        return projectCache;
    }

    /** The archive a {@code jar:} URI points into, such as {@code jar:///x/a.jar!/A.java}. */
    static Path archivePath(String uri) {
        int separator = uri.indexOf("!/");
        if (!uri.startsWith("jar:") || separator < 0) return null;
        String location = uri.substring("jar:".length(), separator);
        if (!location.startsWith("file:")) location = "file:" + location;
        try {
            return Path.of(URI.create(location));
        } catch (IllegalArgumentException | FileSystemNotFoundException e) {
            return null;
        }
    }

    /** The file written for a library URI in an earlier session, if any. */
    private static Path writtenFile(Path dir) {
        if (!Files.isRegularFile(dir.resolve(URI_FILE))) return null;
        try (Stream<Path> files = Files.walk(dir)) {
            return files.filter(file -> !file.getFileName().toString().startsWith("."))
                    .filter(Files::isRegularFile)
                    .findFirst()
                    .orElse(null);
        } catch (IOException e) {
            return null;
        }
    }

    private static void writeLibraryFile(Path dir, Path file, String uri, String code) throws IOException {
        Files.createDirectories(file.getParent());
        Files.writeString(dir.resolve(URI_FILE), uri);
        Path temp = Files.createTempFile(file.getParent(), ".source", ".tmp");
        Files.writeString(temp, code);
        temp.toFile().setReadOnly();
        Files.move(temp, file, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
    }

    /**
     * Where a library URI's text goes inside its directory: the URI's own path, so Zed shows where
     * the file comes from. {@code jar:///usr/lib/jvm/jdk/lib/src.zip!/java.base/java/util/List.java}
     * becomes {@code usr/lib/jvm/jdk/lib/src.zip!/java.base/java/util/List.java}, and
     * {@code jrt:/java.base/java/util/List.class} becomes {@code jrt/java.base/java/util/List.java}.
     * Decompiled {@code .class} files get the extension of the language the server reports.
     */
    static Path libraryPath(String libraryUri, Object language) {
        var segments = new ArrayList<String>();
        int separator = libraryUri.indexOf("!/");
        if (libraryUri.startsWith("jar:") && separator >= 0) {
            String archive = libraryUri.substring("jar:".length(), separator);
            if (archive.startsWith("file:")) archive = archive.substring("file:".length());
            segments.addAll(pathSegments(archive));
            if (!segments.isEmpty()) segments.add(segments.removeLast() + "!");
            segments.addAll(pathSegments(libraryUri.substring(separator + 2)));
        } else {
            int colon = libraryUri.indexOf(':');
            if (colon > 0) segments.add(libraryUri.substring(0, colon));
            segments.addAll(pathSegments(libraryUri.substring(colon + 1)));
        }

        String extension = "kotlin".equals(language) ? ".kt" : ".java";
        String name = segments.isEmpty() ? "Source" : segments.removeLast();
        if (name.endsWith(".class")) name = name.substring(0, name.length() - ".class".length()) + extension;
        if (!name.contains(".")) name = name + extension;
        segments.add(name);
        return Path.of(segments.getFirst(), segments.subList(1, segments.size()).toArray(String[]::new));
    }

    /** The decoded segments of a URI path, made safe as file names on every platform. */
    private static List<String> pathSegments(String path) {
        String decoded = path;
        try {
            decoded = URI.create("file:///" + path.replaceFirst("^/+", "")).getPath();
        } catch (IllegalArgumentException e) {
            // Not percent-encoded; use it as it is.
        }
        var segments = new ArrayList<String>();
        for (String segment : decoded.split("/")) {
            if (segment.isEmpty() || segment.equals(".")) continue;
            segments.add(segment.equals("..") ? "_" : segment.replaceAll("[<>:\"|?*\\\\\\x00-\\x1f]", "_"));
        }
        return segments;
    }

    /**
     * Marks the project's system directory as in use, now and while the proxy runs, and cleans up
     * the caches in the background.
     */
    private static void startHousekeeping(Path librarySources, Path systemDir, String project) {
        long retentionDays = Long.getLong(RETENTION_PROPERTY, 0);
        var executor = Executors.newSingleThreadScheduledExecutor(runnable -> {
            Thread thread = new Thread(runnable, "housekeeping");
            thread.setDaemon(true);
            thread.setPriority(Thread.MIN_PRIORITY);
            return thread;
        });
        executor.scheduleAtFixedRate(
                () -> markInUse(systemDir, project), MARK_IN_USE_HOURS, MARK_IN_USE_HOURS, TimeUnit.HOURS);
        executor.execute(() -> {
            try {
                deleteTree(systemDir.resolve(OLD_LIBRARY_SOURCES));
                if (retentionDays > 0) {
                    Instant cutoff = Instant.now().minus(Duration.ofDays(retentionDays));
                    pruneProjectCaches(systemDir, cutoff);
                    pruneLibraryFiles(librarySources, cutoff);
                }
            } catch (IOException | RuntimeException e) {
                System.err.println("[caches] cleanup failed: " + e);
            }
        });
    }

    private static void markInUse(Path systemDir, String project) {
        try {
            Files.createDirectories(systemDir);
            Files.writeString(systemDir.resolve(PROJECT_FILE), project);
        } catch (IOException | RuntimeException e) {
            System.err.println("[caches] could not mark " + systemDir + " in use: " + e);
        }
    }

    /** Deletes the system directories, next to this project's, that nothing will use again. */
    static void pruneProjectCaches(Path systemDir, Instant cutoff) throws IOException {
        Path root = systemDir.getParent();
        if (root == null) return;
        List<Path> dirs;
        try (Stream<Path> entries = Files.list(root)) {
            dirs = entries.filter(dir -> Files.isDirectory(dir, LinkOption.NOFOLLOW_LINKS)).toList();
        }
        for (Path dir : dirs) {
            String name = dir.getFileName().toString();
            if (name.endsWith(DELETING_SUFFIX)
                    && PROJECT_CACHE.matcher(name.substring(0, name.length() - DELETING_SUFFIX.length())).matches()) {
                // Another proxy may be about to give it back, its project just opened.
                if (staleReason(dir, cutoff) != null) deleteTree(dir);
                continue;
            }
            if (!PROJECT_CACHE.matcher(name).matches() || dir.equals(systemDir)) continue;
            String reason = staleReason(dir, cutoff);
            if (reason == null) continue;
            Path doomed = dir.resolveSibling(name + DELETING_SUFFIX);
            try {
                Files.move(dir, doomed, StandardCopyOption.ATOMIC_MOVE);
            } catch (IOException e) {
                continue; // In use, on Windows.
            }
            // The project may have been opened between the check and the move; if so, give it back.
            if (staleReason(doomed, cutoff) == null) {
                try {
                    Files.move(doomed, dir, StandardCopyOption.ATOMIC_MOVE);
                } catch (IOException e) {
                    // A fresh cache took its place; the old one goes with the next cleanup.
                }
                continue;
            }
            System.err.println("[caches] deleting " + dir + ": " + reason);
            deleteTree(doomed);
        }
    }

    /** Why a project's system directory can go, or {@code null} if it stays. */
    static String staleReason(Path dir, Instant cutoff) throws IOException {
        Path marker = dir.resolve(PROJECT_FILE);
        if (Files.isRegularFile(marker)) {
            String project = Files.readString(marker).strip();
            if (!project.isEmpty() && !exists(project)) return "its project " + project + " no longer exists";
            Instant lastUsed = Files.getLastModifiedTime(marker).toInstant();
            return lastUsed.isBefore(cutoff) ? "unused since " + lastUsed : null;
        }
        // A cache from before the marker: go by the latest change at its top level.
        Instant latest = Files.getLastModifiedTime(dir).toInstant();
        try (Stream<Path> entries = Files.list(dir)) {
            for (Path entry : entries.toList()) {
                Instant modified = Files.getLastModifiedTime(entry, LinkOption.NOFOLLOW_LINKS).toInstant();
                if (modified.isAfter(latest)) latest = modified;
            }
        }
        return latest.isBefore(cutoff) ? "unused since " + latest : null;
    }

    private static boolean exists(String path) {
        try {
            return Files.exists(Path.of(path));
        } catch (IllegalArgumentException e) {
            return true; // Not a path here; better kept than wrongly deleted.
        }
    }

    /** Deletes library files nobody opened since the cutoff. */
    static void pruneLibraryFiles(Path librarySources, Instant cutoff) throws IOException {
        if (!Files.isDirectory(librarySources)) return;
        List<Path> dirs;
        try (Stream<Path> entries = Files.list(librarySources)) {
            dirs = entries.toList();
        }
        for (Path dir : dirs) {
            Path uriFile = dir.resolve(URI_FILE);
            Path stamp = Files.isRegularFile(uriFile) ? uriFile : dir;
            if (Files.getLastModifiedTime(stamp).toInstant().isBefore(cutoff)) deleteTree(dir);
        }
    }

    /** Deletes a file tree, as far as it can; library files are read-only. */
    static void deleteTree(Path root) throws IOException {
        if (!Files.exists(root, LinkOption.NOFOLLOW_LINKS)) return;
        List<Path> paths;
        try (Stream<Path> walk = Files.walk(root)) {
            paths = walk.sorted(java.util.Comparator.reverseOrder()).toList();
        }
        for (Path path : paths) {
            try {
                path.toFile().setWritable(true);
                Files.deleteIfExists(path);
            } catch (IOException e) {
                System.err.println("[caches] could not delete " + path + ": " + e);
            }
        }
    }

    private static void touch(Path file) {
        try {
            Files.setLastModifiedTime(file, FileTime.from(Instant.now()));
        } catch (IOException e) {
            // Only the cleanup relies on it.
        }
    }

    private static void compileSelfInBackground() {
        String source = System.getProperty(SOURCE_PROPERTY);
        String jar = System.getProperty(JAR_PROPERTY);
        if (source == null || jar == null) return;
        Thread thread = new Thread(() -> {
            try {
                compileSelf(Path.of(source), Path.of(jar));
            } catch (IOException | RuntimeException e) {
                System.err.println("[proxy] could not compile the proxy for faster starts: " + e);
            }
        }, "compile-self");
        thread.setDaemon(true);
        thread.setPriority(Thread.MIN_PRIORITY);
        thread.start();
    }

    /** Compile this file into {@code jar}, which only ever appears complete. */
    private static void compileSelf(Path source, Path jar) throws IOException {
        if (Files.exists(jar)) return;
        var compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) return;
        Path dir = jar.toAbsolutePath().getParent();
        Path classes = Files.createTempDirectory(dir, ".classes");
        Path tempJar = Files.createTempFile(dir, ".proxy", ".jar");
        try {
            var errors = new ByteArrayOutputStream();
            if (compiler.run(null, null, errors, "-d", classes.toString(), source.toString()) != 0) {
                throw new IOException(errors.toString(StandardCharsets.UTF_8));
            }
            var manifest = new Manifest();
            manifest.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
            manifest.getMainAttributes().put(Attributes.Name.MAIN_CLASS, IntellijServerProxy.class.getName());
            try (var out = new JarOutputStream(Files.newOutputStream(tempJar), manifest);
                    Stream<Path> files = Files.walk(classes)) {
                for (Path file : files.filter(Files::isRegularFile).toList()) {
                    out.putNextEntry(new JarEntry(classes.relativize(file).toString().replace('\\', '/')));
                    Files.copy(file, out);
                    out.closeEntry();
                }
            }
            Files.move(tempJar, jar, StandardCopyOption.ATOMIC_MOVE);
        } finally {
            Files.deleteIfExists(tempJar);
            try (Stream<Path> files = Files.walk(classes)) {
                for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.delete(file);
            }
        }
    }

    private static boolean isLibraryUri(Object value) {
        return value instanceof String s && (s.startsWith("jar:") || s.startsWith("jrt:"));
    }

    private static boolean isUriKey(String key) {
        return key.equals("uri") || key.equals("targetUri");
    }

    private static void collectLibraryUris(Object node, List<String> uris) {
        if (node instanceof Map<?, ?> map) {
            map.forEach((key, value) -> {
                if (isUriKey((String) key) && isLibraryUri(value)) uris.add((String) value);
                else collectLibraryUris(value, uris);
            });
        } else if (node instanceof List<?> list) {
            list.forEach(item -> collectLibraryUris(item, uris));
        }
    }

    /** Replaces library URIs whose file is ready; the rest stay as they are. */
    private Object toFileUris(Object node) {
        return mapUris(node, value -> {
            LibraryFile entry = libraryFiles.get(value);
            CompletableFuture<String> file = entry == null ? null : entry.uri();
            return file != null && file.isDone() && !file.isCompletedExceptionally() ? file.join() : value;
        });
    }

    private Object toLibraryUris(Object node) {
        return mapUris(node, value -> {
            if (!value.startsWith("file:")) return value;
            try {
                Path path = Path.of(URI.create(value)).toAbsolutePath().normalize();
                if (!path.startsWith(cacheDir)) return value;
                Path relative = cacheDir.relativize(path);
                if (relative.getNameCount() < 2) return value;
                Path uriFile = cacheDir.resolve(relative.getName(0)).resolve(URI_FILE);
                markUsed(uriFile);
                return Files.isRegularFile(uriFile) ? Files.readString(uriFile).strip() : value;
            } catch (IllegalArgumentException | IOException e) {
                return value;
            }
        });
    }

    private static Object mapUris(Object node, java.util.function.UnaryOperator<String> mapper) {
        if (node instanceof Map<?, ?> map) {
            var result = new LinkedHashMap<String, Object>();
            map.forEach((key, value) -> result.put((String) key,
                    isUriKey((String) key) && value instanceof String s ? mapper.apply(s) : mapUris(value, mapper)));
            return result;
        }
        if (node instanceof List<?> list) {
            return list.stream().map(item -> mapUris(item, mapper)).toList();
        }
        return node;
    }

    private static Object field(Object node, String name) {
        return node instanceof Map<?, ?> map ? map.get(name) : null;
    }

    private static String hash(String value) {
        try {
            byte[] digest = MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8));
            return HexFormat.of().formatHex(digest, 0, 8);
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    private static byte[] readMessage(InputStream in) throws IOException {
        int length = -1;
        var line = new ByteArrayOutputStream();
        while (true) {
            int b = in.read();
            if (b == -1) return null;
            if (b != '\n') {
                line.write(b);
                continue;
            }
            String header = line.toString(StandardCharsets.US_ASCII).strip();
            line.reset();
            if (header.isEmpty()) break;
            int colon = header.indexOf(':');
            if (colon > 0 && header.substring(0, colon).strip().equalsIgnoreCase("Content-Length")) {
                length = Integer.parseInt(header.substring(colon + 1).strip());
            }
        }
        if (length < 0) throw new IOException("message without Content-Length");
        byte[] body = in.readNBytes(length);
        if (body.length < length) return null;
        return body;
    }

    private static void write(OutputStream out, String body) throws IOException {
        byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
        synchronized (out) {
            out.write(("Content-Length: " + bytes.length + "\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
            out.write(bytes);
            out.flush();
        }
    }

    /** Just enough JSON: objects, arrays, strings, literals; numbers keep their exact text. */
    static final class Json {
        record Number(String text) {}

        private final String s;
        private int pos;

        private Json(String s) {
            this.s = s;
        }

        static Object parse(String text) {
            var json = new Json(text);
            json.skipSpace();
            Object value = json.value();
            json.skipSpace();
            if (json.pos != text.length()) throw json.error("trailing data");
            return value;
        }

        static String write(Object value) {
            var out = new StringBuilder();
            write(value, out);
            return out.toString();
        }

        private Object value() {
            char c = peek();
            switch (c) {
                case '{' -> {
                    pos++;
                    var map = new LinkedHashMap<String, Object>();
                    skipSpace();
                    if (peek() == '}') {
                        pos++;
                        return map;
                    }
                    while (true) {
                        skipSpace();
                        String key = string();
                        skipSpace();
                        expect(':');
                        skipSpace();
                        map.put(key, value());
                        skipSpace();
                        if (peek() == ',') {
                            pos++;
                            continue;
                        }
                        expect('}');
                        return map;
                    }
                }
                case '[' -> {
                    pos++;
                    var list = new ArrayList<Object>();
                    skipSpace();
                    if (peek() == ']') {
                        pos++;
                        return list;
                    }
                    while (true) {
                        skipSpace();
                        list.add(value());
                        skipSpace();
                        if (peek() == ',') {
                            pos++;
                            continue;
                        }
                        expect(']');
                        return list;
                    }
                }
                case '"' -> {
                    return string();
                }
                case 't' -> {
                    literal("true");
                    return Boolean.TRUE;
                }
                case 'f' -> {
                    literal("false");
                    return Boolean.FALSE;
                }
                case 'n' -> {
                    literal("null");
                    return null;
                }
                default -> {
                    int start = pos;
                    while (pos < s.length() && "+-0123456789.eE".indexOf(s.charAt(pos)) >= 0) pos++;
                    if (start == pos) throw error("unexpected character");
                    return new Number(s.substring(start, pos));
                }
            }
        }

        private String string() {
            expect('"');
            var out = new StringBuilder();
            while (true) {
                char c = s.charAt(pos++);
                if (c == '"') return out.toString();
                if (c != '\\') {
                    out.append(c);
                    continue;
                }
                char e = s.charAt(pos++);
                switch (e) {
                    case 'b' -> out.append('\b');
                    case 'f' -> out.append('\f');
                    case 'n' -> out.append('\n');
                    case 'r' -> out.append('\r');
                    case 't' -> out.append('\t');
                    case 'u' -> {
                        out.append((char) Integer.parseInt(s.substring(pos, pos + 4), 16));
                        pos += 4;
                    }
                    default -> out.append(e);
                }
            }
        }

        private static void write(Object value, StringBuilder out) {
            if (value == null) {
                out.append("null");
            } else if (value instanceof String str) {
                out.append('"');
                for (int i = 0; i < str.length(); i++) {
                    char c = str.charAt(i);
                    switch (c) {
                        case '"' -> out.append("\\\"");
                        case '\\' -> out.append("\\\\");
                        case '\n' -> out.append("\\n");
                        case '\r' -> out.append("\\r");
                        case '\t' -> out.append("\\t");
                        default -> {
                            if (c < 0x20) out.append(String.format("\\u%04x", (int) c));
                            else out.append(c);
                        }
                    }
                }
                out.append('"');
            } else if (value instanceof Number number) {
                out.append(number.text());
            } else if (value instanceof Boolean bool) {
                out.append(bool);
            } else if (value instanceof Map<?, ?> map) {
                out.append('{');
                boolean first = true;
                for (var entry : map.entrySet()) {
                    if (!first) out.append(',');
                    first = false;
                    write(entry.getKey(), out);
                    out.append(':');
                    write(entry.getValue(), out);
                }
                out.append('}');
            } else if (value instanceof List<?> list) {
                out.append('[');
                for (int i = 0; i < list.size(); i++) {
                    if (i > 0) out.append(',');
                    write(list.get(i), out);
                }
                out.append(']');
            } else {
                throw new IllegalArgumentException("not JSON: " + value.getClass());
            }
        }

        private void skipSpace() {
            while (pos < s.length() && Character.isWhitespace(s.charAt(pos))) pos++;
        }

        private char peek() {
            if (pos >= s.length()) throw error("unexpected end");
            return s.charAt(pos);
        }

        private void expect(char c) {
            if (peek() != c) throw error("expected '" + c + "'");
            pos++;
        }

        private void literal(String word) {
            if (!s.startsWith(word, pos)) throw error("expected " + word);
            pos += word.length();
        }

        private IllegalArgumentException error(String message) {
            return new IllegalArgumentException(message + " at " + pos);
        }
    }
}
