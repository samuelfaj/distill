// Modified for Distill by Samuel Fajreldines, 2026.
//! Cross-platform system clipboard access.
//!
//! On macOS, delegates to `pbcopy`/`pbpaste` subprocesses instead of linking AppKit.
//! The `arboard` crate pulls in `objc2-app-kit`, which makes `dyld` load `AppKit.framework` at process startup.
//! AppKit unconditionally initialises the Metal/IOAccelerator GPU subsystem, costing ~2 GB of GPU memory even in headless processes like the leader.
//!
//! The fast pasteboard probe ([`clipboard_image_snapshot`]) keeps that rule.
//! It reaches `NSPasteboard` via objc2 runtime messaging against a lazily `dlopen`ed AppKit, so no binary links AppKit.
//! A process that never probes never loads it.
//! The focus/tick probes message only metadata selectors (`changeCount` / `types`), never content reads.
//! That keeps them sub-millisecond and out of macOS 15.4+ pasteboard privacy alerts.
//!
//! Paste-time image reads ([`get_image`] / [`get_attachments`]) use the same lazily loaded AppKit to read raster bytes in-process (`dataForType:`).
//! That covers the unambiguous hot path (raster advertised, no file-URL type alongside), skipping the ~0.5-0.9 s `osascript` round trip.
//! Content is only read at an explicit user paste, the same user-intent boundary where the `osascript`/`pbpaste` subprocesses read the pasteboard.
//! Every other pasteboard shape (file URLs, text-to-furl coercions, AppKit unavailable, read failure) falls back to the subprocess path.
//! `GROK_CLIPBOARD_NO_NATIVE_READ=1` disables the in-process read entirely (kill switch if a future macOS gates `dataForType:` behind a prompt).
//!
//! On Linux and Windows, `arboard` is used directly (it does not link AppKit on those platforms).
//!
//! ## OSC 52 (remote clipboard)
//!
//! Over SSH or inside tmux/screen on a remote host, the native system clipboard (`pbcopy`, `arboard`) writes to the *remote* machine's clipboard.
//! The user never sees that clipboard.
//! OSC 52 is an escape sequence that tells the user's *local* terminal emulator to set its clipboard, tunnelling through SSH and multiplexers.
//!
//! [`set_text_osc52`] writes an OSC 52 sequence to stderr (the TUI output stream).
//! [`is_remote_session`] detects SSH/tmux/screen environments where OSC 52 should be preferred.

/// Encoded image data read from the system clipboard.
///
/// `data` is encoded bytes, not raw RGBA pixels, so it can be persisted or base64-encoded directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    /// Encoded image bytes (PNG, JPEG, TIFF, etc.).
    pub data: Vec<u8>,
    /// MIME type of the encoded data (e.g. `"image/png"`).
    pub mime_type: String,
}

/// Read text from the system clipboard.
///
/// Returns `Ok(None)` when the clipboard is empty or does not contain text.
pub fn get_text() -> anyhow::Result<Option<String>> {
    platform::get_text()
}

/// Read UTF-8 text from the X11 PRIMARY selection. Requires a non-empty `DISPLAY`. Pure X11 may fall back to arboard;
/// XWayland requires xclip or xsel so arboard cannot return Wayland PRIMARY by mistake.
#[cfg(target_os = "linux")]
pub fn get_primary_text() -> anyhow::Result<Option<String>> {
    platform::get_primary_text()
}

/// Whether this Linux process has a non-empty `DISPLAY` environment value.
#[cfg(target_os = "linux")]
pub fn x11_display_env_present() -> bool {
    platform::x11_display_env_present()
}

/// Which backend answered a paste-time image read, so telemetry can localize a wrong-image report to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardReadPath {
    Native,
    Osascript,
    Arboard,
    LinuxCli,
}

#[derive(Debug, Clone)]
pub struct ClipboardImageRead {
    /// Encoded bytes (not raw RGBA); `None` when the clipboard holds no image.
    pub image: Option<ImageData>,
    pub read_path: ClipboardReadPath,
}

/// Read an image from the system clipboard.
pub fn get_image() -> anyhow::Result<ClipboardImageRead> {
    platform::get_image()
}

/// Read file references the file manager places on the clipboard when a file is selected but no plain text accompanies it.
/// Backed by `«class furl»` (osascript) on macOS, `arboard::Get::file_list` (CF_HDROP / `text/uri-list`) elsewhere.
/// Returns newline-joined absolute paths in the format the drop-path parser accepts.
pub fn get_file_urls() -> anyhow::Result<Option<String>> {
    platform::get_file_urls()
}

/// File URLs and/or image data from the system clipboard in one probe. On macOS one `osascript` tries `«class furl»`
/// first and coerces PNGf, TIFF, then JPEG only when no file URLs are present. On other platforms this composes
/// [`get_file_urls`] and [`get_image`].
#[derive(Debug, Clone)]
pub struct ClipboardAttachments {
    /// Newline-joined POSIX paths (same format as [`get_file_urls`]).
    pub file_urls: Option<String>,
    /// Encoded image bytes when the pasteboard holds a raster image.
    pub image: Option<ImageData>,
    pub read_path: ClipboardReadPath,
}

/// Probe file URLs then image (macOS: one `osascript`; elsewhere: two reads).
pub fn get_attachments() -> anyhow::Result<ClipboardAttachments> {
    platform::get_attachments()
}

/// A copy landing mid-probe can't mix a changeCount from one state with a classification from another. `change_count` is
/// the monotonic `NSPasteboard.changeCount` (`None` off-macOS or when AppKit can't load). Native and sub-millisecond on
/// macOS: it inspects metadata only, with no data read and no subprocess.
pub fn clipboard_image_snapshot() -> (Option<u64>, bool) {
    platform::clipboard_image_snapshot()
}

/// Cheap pasteboard `changeCount` read: a SINGLE native message, no type scan and no data read. Each throttled poll of
/// the focus-driven clipboard-image tip calls only this. It pays for the heavier [`clipboard_image_snapshot`]
/// classification ONLY when the count changed since the last look. `None` off-macOS or when AppKit can't load.
pub fn clipboard_change_count() -> Option<u64> {
    platform::clipboard_change_count()
}

/// Whether the fast image probe exists on this platform.
/// It gates the focus-driven clipboard-image tip so non-macOS never probes.
pub fn clipboard_image_probe_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Trigger the one-time lazy AppKit `dlopen` (the framework load and its GPU init) WITHOUT reading the pasteboard. A
/// later synchronous [`clipboard_image_snapshot`] is then just the cheap metadata read. The load is memoised, and since
/// this touches no pasteboard it is sound to call from a background thread. No-op off-macOS.
pub fn clipboard_prewarm() {
    platform::clipboard_prewarm();
}

/// [`get_attachments`]'s probe tries `«class furl»` first and reads image data only when no file URLs are present. So any
/// file-URL advertisement (`public.file-url`, or its pre-UTI spelling `NSFilenamesPboardType`) means the raster is NOT
/// what ctrl+v inserts. It stays platform-independent so the routing rule is unit-tested on every platform.
#[cfg(any(target_os = "macos", test))]
fn image_pasteable_from_types<'a>(types: impl IntoIterator<Item = &'a [u8]>) -> bool {
    let mut has_image = false;
    for ty in types {
        if matches!(ty, b"public.file-url" | b"NSFilenamesPboardType") {
            return false;
        }
        if matches!(ty, b"public.png" | b"public.tiff" | b"public.jpeg") {
            has_image = true;
        }
    }
    has_image
}

/// Raster pasteboard UTIs in read-priority order with their MIME types. The order mirrors the `osascript` probes'
/// coercions: `PNGf`, then `TIFF`, then `JPEG`. The native in-process read therefore picks the same class the subprocess
/// path would.
#[cfg(any(target_os = "macos", test))]
const NATIVE_IMAGE_TYPES: &[(&[u8], &str)] = &[
    (b"public.png", "image/png"),
    (b"public.tiff", "image/tiff"),
    (b"public.jpeg", "image/jpeg"),
];

/// Pick which advertised raster type the native read should request. `None` unless the advertised type list classifies as
/// a pasteable image under [`image_pasteable_from_types`]. File URLs win and route through the `osascript` furl path. It
/// stays pure so the routing rule is unit-tested on every platform.
#[cfg(any(target_os = "macos", test))]
fn native_image_type_from_types(
    types: &[impl AsRef<[u8]>],
) -> Option<(&'static [u8], &'static str)> {
    if !image_pasteable_from_types(types.iter().map(|t| t.as_ref())) {
        return None;
    }
    for (uti, mime) in NATIVE_IMAGE_TYPES {
        if types.iter().any(|t| t.as_ref() == *uti) {
            return Some((uti, mime));
        }
    }
    None
}

/// Returns `"bin"` for unrecognized types.
pub fn mime_to_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/tiff" => "tiff",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => "bin",
    }
}

/// Infer the MIME type from the first bytes of an image payload.
///
/// Falls back to `"application/octet-stream"` if the magic bytes are unrecognized.
pub fn mime_from_bytes(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.starts_with(b"II\x2a\x00") || data.starts_with(b"MM\x00\x2a") {
        "image/tiff"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.len() >= 12
        && data.starts_with(b"RIFF")
        && data.get(8..12) == Some(b"WEBP".as_slice())
    {
        "image/webp"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
}

/// Per-leg outcome of a native clipboard write (for telemetry).
///
/// On Linux, every viable CLI backend for the session is attempted (see `linux_write_tool_specs`).
#[derive(Debug, Clone, Default)]
pub struct NativeWriteOutcome {
    pub cli_tools_tried: Vec<&'static str>,
    /// Subset of `cli_tools_tried` that succeeded (order preserved). On Wayland, wl-copy is read-back-verified only when
    /// data-control is absent. When `data_control && arboard_ok`, its exit-0 is credited unverified (the arboard write is
    /// authoritative).
    pub cli_ok_tools: Vec<&'static str>,
    pub cli_ok: bool,
    pub arboard_ok: bool,
    /// The Wayland data-control protocol was available for this write (the environment probe, [`wayland_data_control_supported`]).
    /// This is NOT proof the arboard write landed; a focus-free authoritative write additionally requires `arboard_ok`.
    /// Always false on macOS/Windows/X11.
    pub data_control: bool,
    /// True when at least one leg succeeded.
    pub any_ok: bool,
}

/// Write text and return per-leg outcomes for telemetry callers.
pub fn set_text_with_outcome(text: &str) -> NativeWriteOutcome {
    platform::set_text_with_outcome(text)
}

pub fn set_text(text: &str) -> anyhow::Result<()> {
    let outcome = platform::set_text_with_outcome(text);
    if outcome.any_ok {
        Ok(())
    } else {
        anyhow::bail!("no clipboard backend available")
    }
}

/// On macOS, uses `osascript` to set the pasteboard from a file path.
/// On Linux, uses `wl-copy` or `xclip` if available.
/// On Windows, returns an error (not yet supported).
pub fn set_image_file(path: &std::path::Path) -> anyhow::Result<()> {
    platform::set_image_file(path)
}

/// The clipboard tool used for native writes on the current platform.
///
/// Returns `"pbcopy"` on macOS, `"arboard"` on Windows, and the probed CLI tool name on Linux (or `"arboard"` if no CLI tool was found).
pub fn native_tool_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "pbcopy"
    }
    #[cfg(target_os = "linux")]
    {
        platform::linux_tool_spec().map_or("arboard", |spec| spec.name)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "arboard"
    }
}

/// Result of an explicit Wayland data-control capability probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaylandDataControlProbe {
    /// The compositor definitively reported the protocol present or absent.
    Available(bool),
    /// The bounded worker did not answer before its deadline.
    Unavailable,
    /// The worker or compositor returned an operational error.
    Error(String),
}

/// Run one explicit, bounded Wayland data-control capability probe.
///
/// Unlike [`wayland_data_control_supported`], this preserves inconclusive outcomes for diagnostics instead of mapping them to `false`.
pub fn probe_wayland_data_control() -> WaylandDataControlProbe {
    platform::probe_wayland_data_control()
}

/// Native copies then survive the terminal losing focus mid-copy. Memoized per process. An unanswered probe (timeout,
/// connection failure) fails closed: `false` for the current call, retried on a later call up to a small cap. Otherwise
/// arboard's own backend selection would still speak data-control to the compositor regardless of this probe's answer.
pub fn wayland_data_control_supported() -> bool {
    platform::wayland_data_control_supported()
}

/// Error from [`wait_with_deadline`] when the deadline expired (child killed).
#[derive(Debug, thiserror::Error)]
#[error("process did not exit within {0:?}")]
pub struct WaitTimeout(pub std::time::Duration);

/// Deadline for one paste-time `osascript`; the pager's probe timeout must stay above it so the child dies first.
pub const OSASCRIPT_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

/// Clipboard helpers spawn tools (`wl-copy`, `xclip`, `pbcopy`, `tmux load-buffer`) that can hang on a stuck compositor
/// or server. Callers must take/close `child.stdin` (or feed it from a file) before waiting. Unlike `wait()`, the
/// `try_wait` loop does not drop stdin, so a child still reading a held pipe would burn the whole deadline.
pub fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: std::time::Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WaitTimeout(deadline).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

/// A pipe write from the UI thread blocks once the payload exceeds the ~64 KiB pipe buffer. A regular file is fully
/// written before spawn and needs no writer afterwards. The temp file is mode 0600 and unlinked before this returns; the
/// returned fd (and the child's dup of it) stays readable.
pub fn spool_for_stdin(data: &[u8]) -> anyhow::Result<std::fs::File> {
    use anyhow::Context;
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().context("spool clipboard payload to temp file")?;
    tmp.write_all(data)
        .context("spool clipboard payload to temp file")?;
    tmp.reopen().context("spool clipboard payload to temp file")
}

/// Build the OSC 52 clipboard-set byte sequence.
/// When `tmux_passthrough` is true, wrap it in the tmux DCS passthrough envelope; that is only correct when tmux is the IMMEDIATE terminal.
/// Otherwise emit a plain OSC 52.
fn osc52_sequence(text: &str, tmux_passthrough: bool) -> Vec<u8> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if tmux_passthrough {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\").into_bytes()
    } else {
        format!("\x1b]52;c;{encoded}\x07").into_bytes()
    }
}

/// Writes `\x1b]52;c;<base64>\x07` to stderr (the pager's terminal output stream). The caller decides `tmux_passthrough`:
/// the tmux DCS passthrough envelope is only correct when tmux is the IMMEDIATE terminal. Inside an editor `:terminal`
/// (Neovim/Vim/Emacs) the immediate emulator is the editor's libvterm, not tmux.
pub fn set_text_osc52(text: &str, tmux_passthrough: bool) -> anyhow::Result<()> {
    use std::io::Write;

    let seq = osc52_sequence(text, tmux_passthrough);
    crate::stderr::with_locked_stderr(|stderr| -> std::io::Result<()> {
        stderr.write_all(&seq)?;
        stderr.flush()
    })?;
    Ok(())
}

/// Returns `true` when the process appears to be running inside a remote SSH session (with or without a multiplexer like tmux/screen).
///
/// Checks for `SSH_CONNECTION`, `SSH_TTY`, or `SSH_CLIENT` environment variables set by the OpenSSH server on the remote side.
pub fn is_remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
}

/// Returns `true` when the process appears to be running inside a container (Docker, Podman, Kubernetes, etc.) without a
/// display server. In this environment the native system clipboard (`arboard`) will fail because there is no X11/Wayland
/// compositor. OSC 52 terminal escapes are the only viable clipboard path. Windows Terminal, iTerm2).
pub fn is_containerized_without_display() -> bool {
    // If a display server is available, native clipboard should work.
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return false;
    }

    // Docker creates this sentinel file in every container.
    if std::path::Path::new("/.dockerenv").exists() {
        return true;
    }

    // Podman creates this file.
    if std::path::Path::new("/run/.containerenv").exists() {
        return true;
    }

    // Many container runtimes set this env var (podman, systemd-nspawn, etc.).
    if std::env::var_os("container").is_some() {
        return true;
    }

    false
}

// Compiled on every platform so the Linux presubmit covers the pure parts.
#[cfg(any(target_os = "macos", test))]
mod attachments_protocol {
    pub const FURL_MARKER: &str = "<<<FURL>>>";
    pub const IMAGE_MARKER: &str = "<<<IMAGE>>>";

    /// Pasteboard raster classes the AppleScript coerces to, in probe order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RasterClass {
        Png,
        Tiff,
        Jpeg,
    }

    impl RasterClass {
        pub const ALL: [Self; 3] = [Self::Png, Self::Tiff, Self::Jpeg];

        /// The `«class PNGf»` code, also what the script prints on stdout.
        pub fn code(self) -> &'static str {
            match self {
                Self::Png => "PNGf",
                Self::Tiff => "TIFF",
                Self::Jpeg => "JPEG",
            }
        }

        pub fn mime(self) -> &'static str {
            match self {
                Self::Png => "image/png",
                Self::Tiff => "image/tiff",
                Self::Jpeg => "image/jpeg",
            }
        }
    }

    impl std::str::FromStr for RasterClass {
        type Err = ();

        fn from_str(code: &str) -> Result<Self, ()> {
            Self::ALL
                .into_iter()
                .find(|class| class.code() == code)
                .ok_or(())
        }
    }

    /// Private 0700 scratch dir for one `osascript` raster hand-off, removed on drop; a path shared by every grok process let a concurrent probe swap the file between write and read.
    pub struct ProbeTemps(tempfile::TempDir);

    impl ProbeTemps {
        pub fn new() -> anyhow::Result<Self> {
            let mut builder = tempfile::Builder::new();
            builder.prefix("grok-clipboard-probe-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            builder
                .tempdir()
                .map(Self)
                .map_err(|e| anyhow::anyhow!("failed to create clipboard probe dir: {e}"))
        }

        /// The one file the chain writes: every branch truncates it first and the class that wrote arrives on stdout.
        pub fn path(&self) -> std::path::PathBuf {
            self.0.path().join("probe")
        }
    }

    pub(super) fn applescript_string(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// Nested `try` chain coercing each class in order into this probe's file; the code of the one that wrote lands in the caller's `imageOut`. Guillemets are required for `«class …»`.
    pub fn raster_try_chain(temps: &ProbeTemps) -> String {
        let path = applescript_string(&temps.path().display().to_string());
        let mut script = String::new();
        for class in RasterClass::ALL {
            script.push_str(&format!(
                "try\n\
                 set imgData to the clipboard as \u{00AB}class {code}\u{00BB}\n\
                 set filePath to POSIX file {path} as text\n\
                 set fRef to open for access file filePath with write permission\n\
                 set eof of fRef to 0\n\
                 write imgData to fRef\n\
                 close access fRef\n\
                 set imageOut to \"{code}\"\n\
                 on error\n",
                code = class.code(),
            ));
        }
        for _ in RasterClass::ALL {
            script.push_str("end try\n");
        }
        script
    }

    /// Prints the class code that wrote, else `none`.
    pub fn image_osascript(temps: &ProbeTemps) -> String {
        format!(
            "set imageOut to \"none\"\n{}return imageOut",
            raster_try_chain(temps)
        )
    }

    /// File URLs first, rasters only when none are present; without `temps` the raster half is skipped so a Finder paste still works. Stdout follows [`parse_attachments_output`].
    pub fn attachments_osascript(temps: Option<&ProbeTemps>) -> String {
        let image_probe = temps.map_or_else(String::new, |temps| {
            format!(
                "if furlOut is \"none\" then\n{}end if\n",
                raster_try_chain(temps)
            )
        });
        format!(
            "set furlOut to \"none\"\n\
             try\n\
             set urlList to the clipboard as list\n\
             set out to \"\"\n\
             repeat with u in urlList\n\
             try\n\
             set itemRef to contents of u as \u{00AB}class furl\u{00BB}\n\
             set out to out & POSIX path of itemRef & \"\\n\"\n\
             end try\n\
             end repeat\n\
             if out is not \"\" then\n\
             set furlOut to out\n\
             else\n\
             try\n\
             set urlRef to the clipboard as \u{00AB}class furl\u{00BB}\n\
             set furlOut to POSIX path of urlRef\n\
             on error\n\
             end try\n\
             end if\n\
             on error\n\
             try\n\
             set urlRef to the clipboard as \u{00AB}class furl\u{00BB}\n\
             set furlOut to POSIX path of urlRef\n\
             on error\n\
             end try\n\
             end try\n\
             set imageOut to \"NONE\"\n\
             {image_probe}\
             return \"{furl_marker}\" & linefeed & furlOut & linefeed & \"{image_marker}\" & linefeed & \"IMAGE:\" & imageOut",
            furl_marker = FURL_MARKER,
            image_marker = IMAGE_MARKER,
        )
    }

    /// Parse the stdout payload of the `get_file_urls` AppleScript. The script returns one of: one or more POSIX paths
    /// separated by `\n` (success), or; the literal string `"none"` (no file URLs present), or; empty / whitespace-only
    /// (degenerate cases that map to `None`).
    pub fn parse_osascript_furl_output(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "none" {
            return None;
        }
        Some(trimmed.to_owned())
    }

    /// Parse stdout from the unified attachments AppleScript. The image section is `NONE` when file URLs were found (image
    /// probe skipped) or when no image type is on the pasteboard. When both sections parse successfully, file URLs take
    /// precedence over the image class at the [`get_attachments`] layer. Image bytes are not read if `file_urls` is `Some`.
    pub fn parse_attachments_output(raw: &str) -> (Option<String>, Option<RasterClass>) {
        let (furl_section, image_line) = match raw.split_once(IMAGE_MARKER) {
            Some((before, after)) => (before, Some(after)),
            None => (raw, None),
        };
        let file_urls = if furl_section.contains(FURL_MARKER) {
            let furl_body = furl_section
                .strip_prefix(FURL_MARKER)
                .unwrap_or(furl_section)
                .trim_start_matches(['\n', '\r']);
            parse_osascript_furl_output(furl_body)
        } else {
            None
        };
        let image_class = image_line.and_then(|line| {
            line.trim()
                .strip_prefix("IMAGE:")
                .and_then(|code| code.trim().parse::<RasterClass>().ok())
        });
        if !raw.trim().is_empty() && file_urls.is_none() && image_class.is_none() {
            tracing::debug!(
                raw_len = raw.len(),
                has_furl_marker = raw.contains(FURL_MARKER),
                has_image_marker = raw.contains(IMAGE_MARKER),
                "parse_attachments_output: non-empty stdout did not parse to file URLs or image class",
            );
        }
        (file_urls, image_class)
    }

    /// The bytes the script wrote for the class it reported; `None` when it wrote nothing.
    pub fn read_probe_raster(
        class: RasterClass,
        temps: &ProbeTemps,
    ) -> anyhow::Result<Option<super::ImageData>> {
        let data = std::fs::read(temps.path())
            .map_err(|e| anyhow::anyhow!("failed to read clipboard temp file: {e}"))?;
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(super::ImageData {
            data,
            mime_type: class.mime().to_owned(),
        }))
    }

    /// The `osascript` half of [`super::get_attachments`]; `run` executes a script and returns its stdout. Without a scratch dir the
    /// furl half still answers a file paste; with no file URL either the board is unreadable, not empty, as [`super::get_image`] reports it.
    pub fn osascript_attachments(
        temps: anyhow::Result<ProbeTemps>,
        run: impl FnOnce(&str) -> anyhow::Result<Vec<u8>>,
    ) -> anyhow::Result<super::ClipboardAttachments> {
        if let Err(error) = &temps {
            tracing::warn!(%error, "clipboard probe reads file URLs only");
        }
        let stdout = run(&attachments_osascript(temps.as_ref().ok()))?;
        let (file_urls, image_class) = parse_attachments_output(&String::from_utf8_lossy(&stdout));
        let read_path = super::ClipboardReadPath::Osascript;
        if file_urls.is_some() {
            return Ok(super::ClipboardAttachments {
                file_urls,
                image: None,
                read_path,
            });
        }
        let temps = temps?;
        let image = match image_class {
            Some(class) => read_probe_raster(class, &temps)?,
            None => None,
        };
        Ok(super::ClipboardAttachments {
            file_urls: None,
            image,
            read_path,
        })
    }
}

// ---------------------------------------------------------------------------
// macOS: subprocess-based clipboard (no AppKit linkage)
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod platform {
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    use super::attachments_protocol::{
        ProbeTemps, RasterClass, image_osascript, osascript_attachments, read_probe_raster,
    };
    use super::{ClipboardAttachments, ClipboardImageRead, ClipboardReadPath, OSASCRIPT_WAIT};

    // These probes deliberately do NOT use `objc2-app-kit`: that crate emits a `#[link]` against AppKit, which this module
    // exists to avoid. Instead AppKit is `dlopen`ed lazily at the FIRST probe and NSPasteboard is reached via objc2 runtime
    // messaging (libobjc only). Headless processes that never probe never load AppKit at all

    /// Load AppKit once so `objc_getClass("NSPasteboard")` can resolve.
    /// Returns false (probes unavailable) if the load fails.
    fn appkit_loaded() -> bool {
        static LOADED: OnceLock<bool> = OnceLock::new();
        *LOADED.get_or_init(|| {
            let path = c"/System/Library/Frameworks/AppKit.framework/AppKit";
            // SAFETY: dlopen with a constant NUL-terminated path; the handle
            // is intentionally leaked (AppKit stays loaded for the process).
            let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY) };
            !handle.is_null()
        })
    }

    pub(super) fn clipboard_prewarm() {
        // Framework load only (memoised); reads no pasteboard, so this is sound to run on the off-UI-thread warm-up
        let _ = appkit_loaded();
    }

    /// No Wayland on macOS.
    pub(super) fn wayland_data_control_supported() -> bool {
        false
    }

    pub(super) fn probe_wayland_data_control() -> super::WaylandDataControlProbe {
        super::WaylandDataControlProbe::Available(false)
    }

    /// The focus/tick metadata probes run on the UI thread, but [`native_image_read`] runs on a blocking-pool thread and can
    /// overlap them. AppKit reached via a bare `dlopen` (no NSApplication) is NOT safe against concurrent pasteboard
    /// messaging. Every native pasteboard entry point therefore takes this mutex for the duration of its autoreleasepool.
    static PASTEBOARD_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// The general pasteboard as a runtime-messaged object, or `None` when AppKit is unavailable. Thread-safety basis:
    /// objc2-app-kit does NOT classify NSPasteboard as MainThreadOnly. There is no MainThreadMarker on `generalPasteboard`,
    /// unlike NSView/NSWindow. Callers hold [`PASTEBOARD_LOCK`] so there is no concurrent in-process pasteboard access.
    fn general_pasteboard() -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
        if !appkit_loaded() {
            return None;
        }
        let cls = objc2::runtime::AnyClass::get(c"NSPasteboard")?;
        // SAFETY: +[NSPasteboard generalPasteboard] takes no arguments and
        // returns the shared pasteboard instance (nullable-safe via Option).
        unsafe { objc2::msg_send![cls, generalPasteboard] }
    }

    pub(super) fn clipboard_image_snapshot() -> (Option<u64>, bool) {
        let _guard = PASTEBOARD_LOCK.lock();
        objc2::rc::autoreleasepool(|_| {
            let Some(pb) = general_pasteboard() else {
                return (None, false);
            };
            // SAFETY: -[NSPasteboard changeCount] returns NSInteger; it is
            // monotonic and non-negative, so the cast to u64 is lossless.
            let count: isize = unsafe { objc2::msg_send![&*pb, changeCount] };
            // Collect the advertised identifiers, then classify with the shared rule (`image_pasteable_from_types`)
            // A raster type only counts when no file-URL type is advertised alongside, matching the paste-path priority (file URLs win)
            let has_image = advertised_types(&pb).is_some_and(|advertised| {
                super::image_pasteable_from_types(advertised.iter().map(|t| t.as_slice()))
            });
            (Some(count as u64), has_image)
        })
    }

    pub(super) fn clipboard_change_count() -> Option<u64> {
        let _guard = PASTEBOARD_LOCK.lock();
        objc2::rc::autoreleasepool(|_| {
            let pb = general_pasteboard()?;
            // SAFETY: -[NSPasteboard changeCount] returns a monotonic, non-negative NSInteger; the cast to u64 is lossless. This
            // messages ONLY changeCount — no `types` scan, no data read — so it is the cheapest possible pasteboard touch for the
            // throttled poll.
            let count: isize = unsafe { objc2::msg_send![&*pb, changeCount] };
            Some(count as u64)
        })
    }

    /// Advertised pasteboard type identifiers as raw byte strings. `None` when AppKit is unavailable or `types` returns nil.
    /// Shared by the snapshot probe and the native paste-time read so both classify the same advertised list with
    /// `image_pasteable_from_types`.
    fn advertised_types(pb: &objc2::runtime::AnyObject) -> Option<Vec<Vec<u8>>> {
        // SAFETY: -[NSPasteboard types] returns a nullable
        // NSArray<NSPasteboardType>; only count/objectAtIndex/UTF8String
        // are messaged on it, all data-free.
        let types: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
            unsafe { objc2::msg_send![pb, types] };
        let types = types?;
        let n: usize = unsafe { objc2::msg_send![&*types, count] };
        let mut advertised: Vec<Vec<u8>> = Vec::with_capacity(n);
        for i in 0..n {
            let ty: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
                unsafe { objc2::msg_send![&*types, objectAtIndex: i] };
            let Some(ty) = ty else { continue };
            let utf8: *const std::os::raw::c_char = unsafe { objc2::msg_send![&*ty, UTF8String] };
            if utf8.is_null() {
                continue;
            }
            // SAFETY: UTF8String is NUL-terminated and outlives this
            // iteration (the owning NSString is retained above).
            let bytes = unsafe { std::ffi::CStr::from_ptr(utf8) }.to_bytes();
            advertised.push(bytes.to_vec());
        }
        Some(advertised)
    }

    /// `None` also when `GROK_CLIPBOARD_NO_NATIVE_READ` is set, the kill switch if a future macOS gates `dataForType:` behind
    /// a privacy prompt. The focus/tick probes stay metadata-only either way. Thread-safety basis matches
    /// [`general_pasteboard`]: NSPasteboard is not MainThreadOnly, and only `types` and `dataForType:` are messaged.
    pub(super) fn native_image_read() -> Option<super::ImageData> {
        if std::env::var_os("GROK_CLIPBOARD_NO_NATIVE_READ").is_some() {
            return None;
        }
        let _guard = PASTEBOARD_LOCK.lock();
        objc2::rc::autoreleasepool(|_| {
            let pb = general_pasteboard()?;
            let advertised = advertised_types(&pb)?;
            let (uti, mime) = super::native_image_type_from_types(&advertised)?;
            let uti_cstring = std::ffi::CString::new(uti).ok()?;
            let cls = objc2::runtime::AnyClass::get(c"NSString")?;
            // SAFETY: +[NSString stringWithUTF8String:] takes a NUL-terminated
            // C string (valid for the duration of the call) and returns a
            // nullable autoreleased NSString.
            let ns_type: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
                unsafe { objc2::msg_send![cls, stringWithUTF8String: uti_cstring.as_ptr()] };
            let ns_type = ns_type?;
            // SAFETY: -[NSPasteboard dataForType:] takes an NSPasteboardType
            // (NSString) and returns a nullable NSData; only requested for a
            // type the pasteboard itself advertised.
            let data: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
                unsafe { objc2::msg_send![&*pb, dataForType: &*ns_type] };
            let data = data?;
            // SAFETY: -[NSData length] returns NSUInteger.
            let len: usize = unsafe { objc2::msg_send![&*data, length] };
            if len == 0 {
                return None;
            }
            // SAFETY: -[NSData bytes] returns a pointer valid for `len` bytes
            // while `data` is retained (it is, until the end of this scope);
            // the bytes are copied out immediately.
            let bytes_ptr: *const std::os::raw::c_void = unsafe { objc2::msg_send![&*data, bytes] };
            if bytes_ptr.is_null() {
                return None;
            }
            let mut buf = vec![0u8; len];
            // SAFETY: source is valid for `len` reads (NSData contract),
            // destination is a fresh Vec of exactly `len` bytes, and the
            // regions cannot overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes_ptr as *const u8, buf.as_mut_ptr(), len);
            }
            Some(super::ImageData {
                data: buf,
                mime_type: mime.to_owned(),
            })
        })
    }

    pub(super) fn checked_command_stdout(
        label: &str,
        output: std::io::Result<std::process::Output>,
    ) -> anyhow::Result<Vec<u8>> {
        let output = output.map_err(|error| anyhow::anyhow!("failed to run {label}: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("{label} exited with {}: {}", output.status, stderr.trim());
        }
        Ok(output.stdout)
    }

    /// Killed at [`OSASCRIPT_WAIT`]: a script hung on a stuck pasteboard owner or a privacy prompt must not outlive the paste.
    fn run_osascript(script: &str) -> anyhow::Result<Vec<u8>> {
        run_osascript_with_deadline(script, OSASCRIPT_WAIT)
    }

    pub(super) fn run_osascript_with_deadline(
        script: &str,
        deadline: std::time::Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let mut cmd = Command::new("osascript");
        cmd.arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        distill_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // short-lived clipboard helper, waited on below
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to run osascript: {e}"))?;
        // Drain both pipes on workers so the deadline wait can kill a hung script without deadlocking on a full pipe
        fn drain<R: std::io::Read + Send + 'static>(
            mut reader: R,
        ) -> std::io::Result<std::thread::JoinHandle<Vec<u8>>> {
            std::thread::Builder::new().spawn(move || {
                let mut buf = Vec::new();
                let _ = reader.read_to_end(&mut buf);
                buf
            })
        }
        let readers = match (child.stdout.take(), child.stderr.take()) {
            (Some(stdout), Some(stderr)) => {
                drain(stdout).and_then(|stdout| Ok((stdout, drain(stderr)?)))
            }
            _ => Err(std::io::Error::other("stdout or stderr was not piped")),
        };
        let (stdout, stderr) = match readers {
            Ok(readers) => readers,
            Err(error) => {
                // Without its readers the script would run on unwaited, writing into a scratch dir this probe is about to remove
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("failed to start osascript pipe reader: {error}");
            }
        };
        let status = super::wait_with_deadline(&mut child, deadline)?;
        let collect = |handle: std::thread::JoinHandle<Vec<u8>>| handle.join().unwrap_or_default();
        checked_command_stdout(
            "osascript",
            Ok(std::process::Output {
                status,
                stdout: collect(stdout),
                stderr: collect(stderr),
            }),
        )
    }

    /// It is reachable only when the pasteboard text was empty or unactionable, so the AppleScript's text-to-`furl` coercions
    /// cannot apply. A furl can only come from an advertised file-URL type, which routes to the `osascript` below.
    pub fn get_attachments() -> anyhow::Result<ClipboardAttachments> {
        if let Some(image) = native_image_read() {
            return Ok(ClipboardAttachments {
                file_urls: None,
                image: Some(image),
                read_path: ClipboardReadPath::Native,
            });
        }
        osascript_attachments(ProbeTemps::new(), run_osascript)
    }

    /// Read text via `pbpaste -Prefer txt`.
    ///
    /// The `-Prefer txt` flag ensures we only get plain-text content, matching the behaviour of `arboard::Clipboard::get_text()`.
    pub fn get_text() -> anyhow::Result<Option<String>> {
        let mut cmd = Command::new("pbpaste");
        cmd.arg("-Prefer")
            .arg("txt")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        distill_tty_utils::detach_std_command(&mut cmd);
        let stdout = checked_command_stdout("pbpaste", cmd.output())?;
        if stdout.is_empty() {
            return Ok(None);
        }

        Ok(Some(String::from_utf8_lossy(&stdout).into_owned()))
    }

    pub fn set_text_with_outcome(text: &str) -> super::NativeWriteOutcome {
        let mut outcome = super::NativeWriteOutcome {
            cli_tools_tried: vec!["pbcopy"],
            ..Default::default()
        };
        let result = (|| -> anyhow::Result<()> {
            // Spooled stdin (not a pipe): a stalled pbcopy must not block the UI thread on the write, and the deadline wait needs stdin closed
            let stdin = super::spool_for_stdin(text.as_bytes())?;
            let mut cmd = Command::new("pbcopy");
            cmd.stdin(Stdio::from(stdin))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            distill_tty_utils::detach_std_command(&mut cmd);
            #[allow(clippy::disallowed_methods)] // short-lived clipboard helper, waited on below
            let mut child = cmd
                .spawn()
                .map_err(|e| anyhow::anyhow!("failed to spawn pbcopy: {e}"))?;
            let deadline = std::time::Duration::from_secs(2);
            let status = super::wait_with_deadline(&mut child, deadline)?;
            if !status.success() {
                anyhow::bail!("pbcopy exited with status {status}");
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                outcome.cli_ok = true;
                outcome.cli_ok_tools.push("pbcopy");
                outcome.any_ok = true;
            }
            Err(e) => tracing::debug!("pbcopy failed: {e}"),
        }
        outcome
    }

    /// In-process `NSPasteboard` first; the `osascript` fallback probes PNG, TIFF, then JPEG in one invocation via this probe's temp file.
    pub fn get_image() -> anyhow::Result<ClipboardImageRead> {
        // Every shape the native read declines (legacy raster spellings included) falls through to the AppleScript coercion
        if let Some(image) = native_image_read() {
            return Ok(ClipboardImageRead {
                image: Some(image),
                read_path: ClipboardReadPath::Native,
            });
        }
        let read_path = ClipboardReadPath::Osascript;

        let temps = ProbeTemps::new()?;
        let stdout = run_osascript(&image_osascript(&temps))?;
        let image = match String::from_utf8_lossy(&stdout)
            .trim()
            .parse::<RasterClass>()
        {
            Ok(class) => read_probe_raster(class, &temps)?,
            Err(()) => None,
        };
        Ok(ClipboardImageRead { image, read_path })
    }

    /// On several macOS versions `the clipboard as «class furl»` on a multi-file selection returns only the first file's URL
    /// instead of erroring. Inside the list iteration each item is coerced via `as «class furl»`. Non-furl items (text-only
    /// clipboards that still coerce to a single-item list) are skipped rather than passed through as bogus "paths".
    pub fn get_file_urls() -> anyhow::Result<Option<String>> {
        Ok(get_attachments()?.file_urls)
    }

    /// Copy an image file to the macOS clipboard via `osascript`. Detects the pasteboard class from the file extension (PNG,
    /// JPEG, TIFF). Falls back to PNG if the extension is unrecognized.
    pub fn set_image_file(path: &std::path::Path) -> anyhow::Result<()> {
        let class = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("jpg" | "jpeg") => "JPEG",
            Some("tiff" | "tif") => "TIFF",
            _ => "PNGf",
        };
        let path_str = path.display().to_string().replace('"', "\\\"");
        let script = format!(
            "set the clipboard to (read (POSIX file \"{path_str}\") as \u{00AB}class {class}\u{00BB})"
        );
        let mut cmd = Command::new("osascript");
        cmd.arg("-e")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        distill_tty_utils::detach_std_command(&mut cmd);
        let output = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run osascript: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("osascript failed: {stderr}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Linux / Windows: arboard with CLI-tool fallback on Linux
// ---------------------------------------------------------------------------
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{ClipboardImageRead, ClipboardReadPath, ImageData};
    use std::process::{Command, Stdio};

    /// No subprocess-free pasteboard probe exists off-macOS.
    pub(super) fn clipboard_image_snapshot() -> (Option<u64>, bool) {
        (None, false)
    }

    /// No native pasteboard changeCount off-macOS.
    pub(super) fn clipboard_change_count() -> Option<u64> {
        None
    }

    /// No AppKit to pre-warm off-macOS.
    pub(super) fn clipboard_prewarm() {}

    // -- arboard helpers (the in-process leg on all non-macOS platforms) ------

    /// Run `f` on a named worker thread and wait up to `deadline` for its result.
    /// `Err(Timeout)` abandons the worker, which stays parked on the blocked call (a leaked thread beats a frozen UI).
    /// `Err(Disconnected)` means the worker died before sending (spawn failure or panic).
    fn spawn_with_deadline<T: Send + 'static>(
        name: &str,
        deadline: std::time::Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let _ = tx.send(f());
            });
        if spawned.is_err() {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        }
        rx.recv_timeout(deadline)
    }

    /// Memoized read of the `GROK_CLIPBOARD_NO_DATA_CONTROL` kill switch.
    /// Both gates (the data-control probe and the arboard bypass) read this one site, so they can never drift apart.
    #[cfg(target_os = "linux")]
    fn data_control_kill_switch_set() -> bool {
        static SET: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SET.get_or_init(|| std::env::var_os("GROK_CLIPBOARD_NO_DATA_CONTROL").is_some())
    }

    /// True when the arboard leg must be skipped entirely, so copies and pastes ride the CLI tools instead.
    /// On Wayland the `GROK_CLIPBOARD_NO_DATA_CONTROL` kill switch has to stop arboard from speaking data-control at all.
    /// There is no way to force arboard off that backend: it picks data-control on its own whenever `WAYLAND_DISPLAY` is set.
    fn arboard_wayland_bypassed() -> bool {
        #[cfg(target_os = "linux")]
        {
            data_control_kill_switch_set() && env_present("WAYLAND_DISPLAY")
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Deadline for opening an in-process display connection, shared by the data-control probe and arboard `Clipboard::new()` (the same connect).
    /// With separate budgets the probe could time out while the init succeeds, recording `data_control = false` for writes that use data-control.
    const DISPLAY_CONN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Failure, timeout, or the Wayland kill switch (env vars don't change at runtime) is cached as `None`. The mutex is only
    /// held for the in-process arboard call, never across subprocess invocations. Reads do NOT take the lease: they run on
    /// abandonable worker threads (`arboard_read_with_deadline`).
    fn arboard_lease() -> anyhow::Result<&'static parking_lot::Mutex<arboard::Clipboard>> {
        static LEASE: std::sync::OnceLock<Option<parking_lot::Mutex<arboard::Clipboard>>> =
            std::sync::OnceLock::new();
        LEASE
            .get_or_init(|| {
                if arboard_wayland_bypassed() {
                    tracing::debug!("arboard leg disabled (GROK_CLIPBOARD_NO_DATA_CONTROL)");
                    return None;
                }
                match spawn_with_deadline(
                    "clipboard-init",
                    DISPLAY_CONN_WAIT,
                    arboard::Clipboard::new,
                ) {
                    Ok(Ok(clipboard)) => Some(parking_lot::Mutex::new(clipboard)),
                    Ok(Err(e)) => {
                        tracing::debug!("arboard Clipboard::new failed: {e}");
                        None
                    }
                    Err(e) => {
                        tracing::debug!("arboard Clipboard::new did not complete: {e}");
                        None
                    }
                }
            })
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("arboard unavailable"))
    }

    /// The Wayland data-control read has no internal timeout and blocks forever on a hung selection owner; the X11 path has a
    /// 4 s budget. Reads therefore run on a worker thread that is abandoned on expiry. The worker's `Clipboard` instance
    /// leaks with it; harmless while the lease keeps the shared backend alive.
    const ARBOARD_READ_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    fn arboard_read_with_deadline<T: Send + 'static>(
        op: impl FnOnce(&mut arboard::Clipboard) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        use std::sync::mpsc::RecvTimeoutError;
        if arboard_wayland_bypassed() {
            anyhow::bail!("arboard leg disabled (GROK_CLIPBOARD_NO_DATA_CONTROL)");
        }
        let result = spawn_with_deadline("clipboard-read", ARBOARD_READ_WAIT, move || {
            arboard::Clipboard::new()
                .map_err(anyhow::Error::from)
                .and_then(|mut clipboard| op(&mut clipboard))
        });
        match result {
            Ok(inner) => inner,
            Err(RecvTimeoutError::Timeout) => Err(anyhow::anyhow!("arboard read timed out")),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!("arboard read worker died")),
        }
    }

    fn arboard_get_text() -> anyhow::Result<Option<String>> {
        arboard_read_with_deadline(|clipboard| match clipboard.get_text() {
            Ok(text) if text.is_empty() => Ok(None),
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(err) => Err(err.into()),
        })
    }

    #[cfg(target_os = "linux")]
    fn arboard_get_primary_text() -> anyhow::Result<Option<String>> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        arboard_read_with_deadline(|clipboard| {
            match clipboard
                .get()
                .clipboard(LinuxClipboardKind::Primary)
                .text()
            {
                Ok(text) if text.is_empty() => Ok(None),
                Ok(text) => Ok(Some(text)),
                Err(
                    arboard::Error::ContentNotAvailable | arboard::Error::ClipboardNotSupported,
                ) => Ok(None),
                Err(err) => Err(err.into()),
            }
        })
    }

    fn arboard_set_text(text: &str) -> anyhow::Result<()> {
        arboard_lease()?.lock().set_text(text)?;
        Ok(())
    }

    fn arboard_get_image() -> anyhow::Result<Option<ImageData>> {
        arboard_read_with_deadline(|clipboard| {
            let img_data = match clipboard.get_image() {
                Ok(data) => data,
                Err(arboard::Error::ContentNotAvailable) => return Ok(None),
                Err(err) => return Err(err.into()),
            };

            let png_bytes = encode_rgba_to_png(
                &img_data.bytes,
                img_data.width as u32,
                img_data.height as u32,
            )?;

            Ok(Some(ImageData {
                data: png_bytes,
                mime_type: "image/png".to_owned(),
            }))
        })
    }

    // arboard is built with `wayland-data-control` On compositors exposing the data-control protocol (probe:
    // `wayland_data_control_supported`) it sets the Wayland selection focus-free. That arboard write is authoritative. Reads
    // shell out when arboard fails, or when its "empty" answer is not authoritative on Wayland (see `wayland_tool_selected`)

    /// Argv specs for each Linux clipboard tool.
    /// All CLI dispatch goes through `run_pipe_in` / `run_capture_out` with these specs.
    #[cfg(target_os = "linux")]
    pub(super) struct ToolSpec {
        pub(super) name: &'static str,
        /// Tool reads the Wayland selection (not the X11 CLIPBOARD arboard checks).
        reads_wayland_selection: bool,
        write_text: &'static [&'static str],
        read_text: &'static [&'static str],
        read_primary: Option<&'static [&'static str]>,
        write_png: Option<&'static [&'static str]>,
        read_png: Option<&'static [&'static str]>,
    }

    #[cfg(target_os = "linux")]
    const WL_SPEC: ToolSpec = ToolSpec {
        name: "wl-copy",
        reads_wayland_selection: true,
        // `-t text`: exit non-zero on non-text clipboards instead of dumping raw bytes of an arbitrary MIME type
        // `--no-newline` makes wl-paste return the selection verbatim, which `wayland_readback_matches` compares exactly
        read_text: &["wl-paste", "--no-newline", "-t", "text"],
        write_text: &["wl-copy"],
        read_primary: None,
        write_png: Some(&["wl-copy", "-t", "image/png"]),
        read_png: Some(&["wl-paste", "-t", "image/png"]),
    };

    #[cfg(target_os = "linux")]
    const XCLIP_SPEC: ToolSpec = ToolSpec {
        name: "xclip",
        reads_wayland_selection: false,
        write_text: &["xclip", "-selection", "clipboard"],
        read_text: &["xclip", "-o", "-selection", "clipboard"],
        read_primary: Some(&["xclip", "-o", "-selection", "primary"]),
        write_png: Some(&["xclip", "-selection", "clipboard", "-t", "image/png", "-i"]),
        read_png: Some(&["xclip", "-selection", "clipboard", "-t", "image/png", "-o"]),
    };

    #[cfg(target_os = "linux")]
    const XSEL_SPEC: ToolSpec = ToolSpec {
        name: "xsel",
        reads_wayland_selection: false,
        write_text: &["xsel", "--clipboard", "--input"],
        read_text: &["xsel", "--clipboard", "--output"],
        read_primary: Some(&["xsel", "--primary", "--output"]),
        write_png: None, // xsel doesn't support typed clipboard
        read_png: None,
    };

    /// Cached tool spec, probed once at first use.
    #[cfg(target_os = "linux")]
    pub(super) fn linux_tool_spec() -> Option<&'static ToolSpec> {
        static SPEC: std::sync::OnceLock<Option<&'static ToolSpec>> = std::sync::OnceLock::new();
        *SPEC.get_or_init(probe_tool_spec)
    }

    /// On Wayland-only sessions (no focused X11/XWayland client) the X11 CLIPBOARD has no owner, so arboard's `Ok(None)` is
    /// not authoritative. `wl-paste` must be consulted; `xclip`/`xsel` re-read the same X11 selection, so they don't qualify.
    #[cfg(target_os = "linux")]
    fn wayland_tool_selected(spec: Option<&ToolSpec>) -> bool {
        spec.is_some_and(|spec| spec.reads_wayland_selection)
    }

    /// Indefinite probe outcomes tolerated before deciding `false` permanently.
    /// The cap stops a wedged compositor taxing every copy with a bounded probe forever; the lease treats its timeout the same way.
    #[cfg(target_os = "linux")]
    const PROBE_INDEFINITE_MAX: u32 = 3;

    /// Probe cache: `decided` is the permanent answer once set; `indefinite_seen` counts completed unanswered probes toward the cap.
    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ProbeCache {
        decided: Option<bool>,
        indefinite_seen: u32,
    }

    #[cfg(target_os = "linux")]
    impl ProbeCache {
        const fn new() -> Self {
            Self {
                decided: None,
                indefinite_seen: 0,
            }
        }
    }

    /// Apply one probe outcome to the cache and return the answer for this call. Definitive answers decide permanently;
    /// indefinite outcomes fail closed and only decide (as `false`) once the retry cap is exhausted. Deciding earlier would
    /// be a permanent false negative: the lease init runs the same connect on the same budget and may succeed moments later.
    #[cfg(target_os = "linux")]
    fn apply_probe_outcome(
        cache: &mut ProbeCache,
        outcome: &super::WaylandDataControlProbe,
    ) -> bool {
        match outcome {
            super::WaylandDataControlProbe::Available(supported) => {
                cache.decided = Some(*supported);
                *supported
            }
            super::WaylandDataControlProbe::Unavailable
            | super::WaylandDataControlProbe::Error(_) => {
                cache.indefinite_seen += 1;
                if cache.indefinite_seen >= PROBE_INDEFINITE_MAX {
                    cache.decided = Some(false);
                }
                false
            }
        }
    }

    /// Testable core of the memoized probe: return the decided answer, or run `probe` and apply its outcome.
    #[cfg(target_os = "linux")]
    fn cached_probe_answer(
        cache: &parking_lot::Mutex<ProbeCache>,
        probe: impl FnOnce() -> super::WaylandDataControlProbe,
    ) -> bool {
        let mut cache = cache.lock();
        if let Some(decided) = cache.decided {
            return decided;
        }
        // Probing while holding the lock serializes concurrent callers (UI copy vs. the spawn_blocking telemetry snapshot).
        // The second caller waits for the first's answer, bounded by the probe's own DISPLAY_CONN_WAIT deadline
        // Without that, callers could double-probe, race the cap count past PROBE_INDEFINITE_MAX, or drop a concurrent definitive answer
        let outcome = probe();
        apply_probe_outcome(&mut cache, &outcome)
    }

    /// Memoized data-control probe; the public wrapper documents the caching rules.
    #[cfg(target_os = "linux")]
    pub(super) fn wayland_data_control_supported() -> bool {
        static CACHE: parking_lot::Mutex<ProbeCache> = parking_lot::Mutex::new(ProbeCache::new());
        // Env gates are runtime-constant: no lock and no probe
        // The kill switch and non-Wayland sessions are always "no data-control"
        if data_control_kill_switch_set() || !env_present("WAYLAND_DISPLAY") {
            return false;
        }
        cached_probe_answer(&CACHE, probe_data_control)
    }

    /// No Wayland off Linux.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn wayland_data_control_supported() -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    pub(super) fn probe_wayland_data_control() -> super::WaylandDataControlProbe {
        if data_control_kill_switch_set() || !env_present("WAYLAND_DISPLAY") {
            return super::WaylandDataControlProbe::Available(false);
        }
        probe_data_control()
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn probe_wayland_data_control() -> super::WaylandDataControlProbe {
        super::WaylandDataControlProbe::Available(false)
    }

    /// Run the same compositor probe arboard's `Clipboard::new` uses to pick its Wayland backend, on a bounded worker.
    /// Both the cached bool adapter (`wayland_data_control_supported`) and the explicit diagnostics probe consume this typed result.
    #[cfg(target_os = "linux")]
    fn probe_data_control() -> super::WaylandDataControlProbe {
        use std::sync::mpsc::RecvTimeoutError;
        match spawn_with_deadline(
            "clipboard-dc-probe",
            DISPLAY_CONN_WAIT,
            wl_clipboard_rs::utils::is_primary_selection_supported,
        ) {
            Ok(result) => data_control_from_result(result),
            Err(RecvTimeoutError::Timeout) => super::WaylandDataControlProbe::Unavailable,
            Err(RecvTimeoutError::Disconnected) => {
                super::WaylandDataControlProbe::Error("probe worker died".to_owned())
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum DataControlSemanticResult {
        Present,
        MissingProtocol,
        NoSeats,
        ConnectionUnavailable,
    }

    #[cfg(target_os = "linux")]
    fn classify_data_control_semantic(
        result: DataControlSemanticResult,
    ) -> super::WaylandDataControlProbe {
        use super::WaylandDataControlProbe;
        match result {
            DataControlSemanticResult::Present => WaylandDataControlProbe::Available(true),
            DataControlSemanticResult::MissingProtocol => WaylandDataControlProbe::Available(false),
            DataControlSemanticResult::NoSeats
            | DataControlSemanticResult::ConnectionUnavailable => {
                WaylandDataControlProbe::Unavailable
            }
        }
    }

    /// Exhaustive mapping from the `wl_clipboard_rs` probe result to a probe outcome.
    #[cfg(target_os = "linux")]
    fn data_control_from_result(
        result: Result<bool, wl_clipboard_rs::utils::PrimarySelectionCheckError>,
    ) -> super::WaylandDataControlProbe {
        use wl_clipboard_rs::utils::PrimarySelectionCheckError;
        let semantic = match result {
            Ok(_) => DataControlSemanticResult::Present,
            Err(PrimarySelectionCheckError::MissingProtocol) => {
                DataControlSemanticResult::MissingProtocol
            }
            Err(PrimarySelectionCheckError::NoSeats) => DataControlSemanticResult::NoSeats,
            Err(
                PrimarySelectionCheckError::SocketOpenError(_)
                | PrimarySelectionCheckError::WaylandConnection(_)
                | PrimarySelectionCheckError::WaylandCommunication(_),
            ) => DataControlSemanticResult::ConnectionUnavailable,
        };
        classify_data_control_semantic(semantic)
    }

    #[cfg(target_os = "linux")]
    fn env_value_present(value: Option<&std::ffi::OsStr>) -> bool {
        value.is_some_and(|value| !value.is_empty())
    }

    #[cfg(target_os = "linux")]
    fn env_present(var: &str) -> bool {
        env_value_present(std::env::var_os(var).as_deref())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn x11_display_env_present() -> bool {
        env_present("DISPLAY")
    }

    /// Deadline for CLI clipboard writes (payload piped through the tool).
    #[cfg(target_os = "linux")]
    const CLI_WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Deadline for CLI availability probes and write read-backs.
    #[cfg(target_os = "linux")]
    const CLI_PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

    /// Deadline for CLI content reads (wl-paste/xclip text and image fallbacks).
    /// It matches the in-process `ARBOARD_READ_WAIT` so a paste gets equal time on either backend.
    #[cfg(target_os = "linux")]
    const CLI_READ_WAIT: std::time::Duration = ARBOARD_READ_WAIT;

    #[cfg(target_os = "linux")]
    fn tool_available(spec: &ToolSpec) -> bool {
        let Some(bin) = spec.write_text.first() else {
            return false;
        };
        let mut cmd = Command::new(bin);
        cmd.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        distill_tty_utils::detach_std_command(&mut cmd);
        // Availability means the tool ran and exited in time (any exit status)
        #[allow(clippy::disallowed_methods)] // availability probe, waited on with a timeout
        let Ok(mut child) = cmd.spawn() else {
            return false;
        };
        super::wait_with_deadline(&mut child, CLI_PROBE_WAIT).is_ok()
    }

    /// Cache only successful discovery so transient misses are retried.
    #[cfg(target_os = "linux")]
    fn cache_successful_probe(
        cache: &std::sync::OnceLock<()>,
        probe: impl FnOnce() -> bool,
    ) -> bool {
        if cache.get().is_some() {
            return true;
        }
        if !probe() {
            return false;
        }
        let _ = cache.set(());
        true
    }

    #[cfg(target_os = "linux")]
    fn x11_primary_tool_available(spec: &ToolSpec) -> bool {
        static XCLIP_DISCOVERED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        static XSEL_DISCOVERED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if std::ptr::eq(spec, &XCLIP_SPEC) {
            cache_successful_probe(&XCLIP_DISCOVERED, || tool_available(&XCLIP_SPEC))
        } else {
            debug_assert!(std::ptr::eq(spec, &XSEL_SPEC));
            cache_successful_probe(&XSEL_DISCOVERED, || tool_available(&XSEL_SPEC))
        }
    }

    #[cfg(target_os = "linux")]
    fn primary_arboard_fallback_allowed(
        display_env_present: bool,
        wayland_env_present: bool,
    ) -> bool {
        display_env_present && !wayland_env_present
    }

    /// Primary probe: the first tool whose env matches and which is on PATH, in wl-copy, xclip, xsel order.
    #[cfg(target_os = "linux")]
    fn probe_tool_spec() -> Option<&'static ToolSpec> {
        let specs: &[(&str, &ToolSpec)] = &[
            ("WAYLAND_DISPLAY", &WL_SPEC),
            ("DISPLAY", &XCLIP_SPEC),
            ("DISPLAY", &XSEL_SPEC),
        ];
        for &(env_var, spec) in specs {
            if env_present(env_var) && tool_available(spec) {
                return Some(spec);
            }
        }
        None
    }

    /// Every CLI backend we should fire on write (not just the primary probe). Probe/`native_tool_name()` stays single-winner
    /// for labels. Writes fire every backend viable for the session so hybrid Wayland/X11 desktops (KDE/XWayland, GNOME)
    /// populate both selections. Order is wl-copy then xclip then xsel; at most one X11 tool (xclip preferred).
    #[cfg(target_os = "linux")]
    fn linux_write_tool_specs() -> &'static [&'static ToolSpec] {
        static SPECS: std::sync::OnceLock<Vec<&'static ToolSpec>> = std::sync::OnceLock::new();
        SPECS
            .get_or_init(|| {
                collect_linux_write_specs(
                    env_present("WAYLAND_DISPLAY"),
                    env_present("DISPLAY"),
                    tool_available(&WL_SPEC),
                    tool_available(&XCLIP_SPEC),
                    tool_available(&XSEL_SPEC),
                )
            })
            .as_slice()
    }

    /// Pure write-target selection (unit-testable without subprocesses).
    #[cfg(target_os = "linux")]
    fn collect_linux_write_specs(
        wayland_env: bool,
        display_env: bool,
        wl_ok: bool,
        xclip_ok: bool,
        xsel_ok: bool,
    ) -> Vec<&'static ToolSpec> {
        let mut out: Vec<&'static ToolSpec> = Vec::new();
        if wayland_env && wl_ok {
            out.push(&WL_SPEC);
        }
        if display_env {
            if xclip_ok {
                out.push(&XCLIP_SPEC);
            } else if xsel_ok {
                out.push(&XSEL_SPEC);
            }
        }
        out
    }

    /// Pipe `data` into a CLI tool's stdin.
    #[cfg(target_os = "linux")]
    fn run_pipe_in(argv: &[&str], data: &[u8]) -> anyhow::Result<()> {
        let (bin, args) = argv.split_first().expect("argv non-empty");

        // Spooled stdin (`spool_for_stdin`), not a pipe: clipboard tools (wl-copy/xclip) daemonize and read the payload after forking
        // That read races a pipe write and can leave the selection empty; the daemon keeps its fd to the unlinked temp file
        let stdin = super::spool_for_stdin(data)?;

        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        distill_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // short-lived clipboard helper, waited on below
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn {bin}: {e}"))?;
        let status = super::wait_with_deadline(&mut child, CLI_WRITE_WAIT)?;
        if !status.success() {
            anyhow::bail!("{bin} exited with status {status}");
        }
        Ok(())
    }

    /// Run a CLI tool and capture its stdout, bounded by `deadline` (`CLI_PROBE_WAIT` for read-backs, `CLI_READ_WAIT` for content reads).
    #[cfg(target_os = "linux")]
    fn run_capture_out_with_status(
        argv: &[&str],
        deadline: std::time::Duration,
    ) -> anyhow::Result<(std::process::ExitStatus, Vec<u8>)> {
        let (bin, args) = argv.split_first().expect("argv non-empty");
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        distill_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // short-lived clipboard helper, waited on below
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to run {bin}: {e}"))?;
        // Drain stdout on a worker so the deadline wait can kill a hung tool without deadlocking on a full pipe
        // The kill EOFs the pipe and the reader exits on its own
        let mut stdout = child.stdout.take().expect("stdout piped");
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let status = super::wait_with_deadline(&mut child, deadline)?;
        let stdout = reader.join().unwrap_or_default();
        Ok((status, stdout))
    }

    /// Capture CLI stdout; a non-zero exit yields empty bytes (typed MIME absent), not Err.
    /// Spawn/timeout failures still return Err.
    /// Prefer this over [`run_capture_out_checked`] for content reads where a missing type is normal, such as a text probe of an image-only board.
    #[cfg(target_os = "linux")]
    fn run_capture_out(argv: &[&str], deadline: std::time::Duration) -> anyhow::Result<Vec<u8>> {
        let (status, stdout) = run_capture_out_with_status(argv, deadline)?;
        if !status.success() {
            return Ok(Vec::new());
        }
        Ok(stdout)
    }

    /// Capture CLI stdout; non-zero exit is Err.
    /// Use for paths that must distinguish tool failure from emptiness (e.g. the PRIMARY multi-tool scan).
    #[cfg(target_os = "linux")]
    fn run_capture_out_checked(
        argv: &[&str],
        deadline: std::time::Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let (status, stdout) = run_capture_out_with_status(argv, deadline)?;
        if !status.success() {
            let bin = argv.first().copied().unwrap_or("clipboard tool");
            anyhow::bail!("{bin} exited with status {status}");
        }
        Ok(stdout)
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug, Eq, PartialEq)]
    enum PrimaryCliRead {
        Text(String),
        Empty,
        Failed,
    }

    #[cfg(target_os = "linux")]
    fn read_x11_primary_with_tools(
        display_env_present: bool,
        mut available: impl FnMut(&ToolSpec) -> bool,
        mut capture: impl FnMut(&ToolSpec, &[&str]) -> anyhow::Result<Vec<u8>>,
    ) -> PrimaryCliRead {
        if !display_env_present {
            return PrimaryCliRead::Failed;
        }
        for spec in [&XCLIP_SPEC, &XSEL_SPEC] {
            if !available(spec) {
                continue;
            }
            let argv = spec
                .read_primary
                .expect("X11 PRIMARY tool must define read argv");
            match capture(spec, argv) {
                Ok(bytes) if bytes.is_empty() => return PrimaryCliRead::Empty,
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => return PrimaryCliRead::Text(text),
                    Err(e) => {
                        tracing::debug!(tool = spec.name, "X11 PRIMARY text was not UTF-8: {e}")
                    }
                },
                Err(e) => {
                    tracing::debug!(tool = spec.name, "X11 PRIMARY CLI read failed: {e}")
                }
            }
        }
        PrimaryCliRead::Failed
    }

    /// True if a Wayland read-back exactly matches what we wrote.
    /// `wl-paste --no-newline` returns the selection verbatim (no appended newline), so an exact byte comparison is correct.
    #[cfg(target_os = "linux")]
    fn wayland_readback_matches(readback: &[u8], text: &str) -> bool {
        readback == text.as_bytes()
    }

    /// Confirm a Wayland write landed by reading the selection back. wl-copy daemonizes and can exit 0 even when the
    /// selection ends up empty, so its exit status alone is not trustworthy. A transient miss only under-reports success
    /// (never a false "Copied!").
    #[cfg(target_os = "linux")]
    fn wayland_write_verified(spec: &ToolSpec, text: &str) -> bool {
        match run_capture_out(spec.read_text, CLI_PROBE_WAIT) {
            Ok(bytes) if wayland_readback_matches(&bytes, text) => true,
            Ok(_) => {
                tracing::debug!(
                    "clipboard read-back mismatch ({spec_name})",
                    spec_name = spec.name
                );
                false
            }
            Err(e) => {
                tracing::debug!(
                    "clipboard read-back failed ({spec_name}): {e}",
                    spec_name = spec.name
                );
                false
            }
        }
    }

    #[cfg(target_os = "linux")]
    const READBACK_ATTEMPTS: usize = 3;
    #[cfg(target_os = "linux")]
    const READBACK_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

    /// Retry `attempt` up to `attempts` times, pausing between failures.
    /// Pure over the injected closure so the retry policy is unit-testable.
    #[cfg(target_os = "linux")]
    fn readback_with_retry(
        mut attempt: impl FnMut() -> bool,
        attempts: usize,
        pause: std::time::Duration,
    ) -> bool {
        for i in 0..attempts {
            if attempt() {
                return true;
            }
            if i + 1 < attempts {
                std::thread::sleep(pause);
            }
        }
        false
    }

    /// Whether a successful Wayland CLI write still needs the wl-paste read-back to count as verified. With data-control the
    /// arboard write is authoritative and already succeeded. wl-copy then fires purely for post-exit persistence and its
    /// result no longer gates success. It stays pure so unit tests can drive it.
    #[cfg(target_os = "linux")]
    fn wayland_readback_required(
        reads_wayland_selection: bool,
        data_control: bool,
        arboard_ok: bool,
    ) -> bool {
        reads_wayland_selection && !(data_control && arboard_ok)
    }

    // -- Public API ----------------------------------------------------------

    pub fn get_text() -> anyhow::Result<Option<String>> {
        let mut arboard_error = None;
        match arboard_get_text() {
            Ok(Some(text)) => return Ok(Some(text)),
            // Wayland-only: arboard's empty answer is not authoritative, fall through to wl-paste (see `wayland_tool_selected`)
            #[cfg(target_os = "linux")]
            Ok(None) if wayland_tool_selected(linux_tool_spec()) => {}
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::debug!("arboard get_text failed: {e}");
                arboard_error = Some(e);
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(spec) = linux_tool_spec() {
            // Not checked: `wl-paste -t text` exits non-zero on image-only boards.
            let bytes = run_capture_out(spec.read_text, CLI_READ_WAIT)?;
            return Ok(if bytes.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&bytes).into_owned())
            });
        }
        if let Some(error) = arboard_error {
            return Err(error);
        }
        Ok(None)
    }

    #[cfg(target_os = "linux")]
    pub fn get_primary_text() -> anyhow::Result<Option<String>> {
        let display_env_present = x11_display_env_present();
        if !display_env_present {
            return Ok(None);
        }
        let wayland_env_present = env_present("WAYLAND_DISPLAY");

        match read_x11_primary_with_tools(
            display_env_present,
            x11_primary_tool_available,
            |_, argv| run_capture_out_checked(argv, CLI_READ_WAIT),
        ) {
            PrimaryCliRead::Text(text) => return Ok(Some(text)),
            PrimaryCliRead::Empty => return Ok(None),
            PrimaryCliRead::Failed => {}
        }

        if primary_arboard_fallback_allowed(display_env_present, wayland_env_present) {
            return arboard_get_primary_text();
        }
        Ok(None)
    }

    pub fn set_text_with_outcome(text: &str) -> super::NativeWriteOutcome {
        // Fire every viable native backend: arboard first, then the CLI tools (`wl-copy`/`xclip`/`xsel`) that match what users
        // verify manually. On X11 arboard can return Ok(()) while GNOME/VTE/KDE paste still reads Wayland wl-copy runs after
        // arboard so its daemonized process ends up owning the Wayland selection and post-exit paste keeps working.
        let mut outcome = super::NativeWriteOutcome {
            data_control: wayland_data_control_supported(),
            ..Default::default()
        };

        match arboard_set_text(text) {
            Ok(()) => outcome.arboard_ok = true,
            Err(e) => tracing::debug!("arboard set_text failed: {e}"),
        }

        #[cfg(target_os = "linux")]
        for spec in linux_write_tool_specs() {
            outcome.cli_tools_tried.push(spec.name);
            match run_pipe_in(spec.write_text, text.as_bytes()) {
                Ok(()) => {
                    // Even then it runs only while the read-back still gates success (see `wayland_readback_required`). The bounded retry
                    // covers the race against wl-copy's daemonized claim of the selection. The skip leaves one rare hole: wl-copy re-claims
                    // the selection after the good arboard write.
                    let needs_readback = wayland_readback_required(
                        spec.reads_wayland_selection,
                        outcome.data_control,
                        outcome.arboard_ok,
                    );
                    let verified = !needs_readback
                        || readback_with_retry(
                            || wayland_write_verified(spec, text),
                            READBACK_ATTEMPTS,
                            READBACK_PAUSE,
                        );
                    if verified {
                        outcome.cli_ok = true;
                        outcome.cli_ok_tools.push(spec.name);
                    }
                }
                Err(e) => tracing::debug!(
                    "CLI clipboard write failed ({spec_name}): {e}",
                    spec_name = spec.name
                ),
            }
        }

        outcome.any_ok = outcome.cli_ok || outcome.arboard_ok;
        outcome
    }

    pub fn get_image() -> anyhow::Result<ClipboardImageRead> {
        let arboard = |image| ClipboardImageRead {
            image,
            read_path: ClipboardReadPath::Arboard,
        };
        let mut arboard_error = None;
        match arboard_get_image() {
            Ok(Some(image)) => return Ok(arboard(Some(image))),
            // Wayland-only: arboard's empty answer is not authoritative, fall through to wl-paste (see `wayland_tool_selected`)
            #[cfg(target_os = "linux")]
            Ok(None) if wayland_tool_selected(linux_tool_spec()) => {}
            Ok(None) => return Ok(arboard(None)),
            Err(e) => {
                tracing::debug!("arboard get_image failed: {e}");
                arboard_error = Some(e);
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(spec) = linux_tool_spec()
            && let Some(argv) = spec.read_png
        {
            // Not checked: typed image MIME miss is an empty read, not probe failure.
            let bytes = run_capture_out(argv, CLI_READ_WAIT)?;
            let image = (!bytes.is_empty()).then(|| ImageData {
                mime_type: super::mime_from_bytes(&bytes).to_owned(),
                data: bytes,
            });
            return Ok(ClipboardImageRead {
                image,
                read_path: ClipboardReadPath::LinuxCli,
            });
        }
        if let Some(error) = arboard_error {
            return Err(error);
        }
        Ok(arboard(None))
    }

    /// Unlike `set_text`/`get_text`/`get_image`, this skips arboard and goes straight to CLI tools.
    /// arboard has no `set_image_file` API, and decoding and re-encoding through its RGBA interface is wasteful when the file is already usable.
    /// Uses the same multi-backend write list as text (`linux_write_tool_specs`).
    pub fn set_image_file(path: &std::path::Path) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let data = std::fs::read(path)?;
            let mut any_ok = false;
            let mut last_err: Option<anyhow::Error> = None;
            for spec in linux_write_tool_specs() {
                let Some(argv) = spec.write_png else {
                    continue;
                };
                match run_pipe_in(argv, &data) {
                    Ok(()) => any_ok = true,
                    Err(e) => {
                        tracing::debug!(
                            "CLI image clipboard write failed ({spec_name}): {e}",
                            spec_name = spec.name
                        );
                        last_err = Some(e);
                    }
                }
            }
            if any_ok {
                return Ok(());
            }
            if let Some(e) = last_err {
                return Err(e);
            }
            anyhow::bail!("no CLI tool supports image clipboard writes");
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            anyhow::bail!("image clipboard not supported on this platform")
        }
    }

    pub(super) fn encode_rgba_to_png(
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<Vec<u8>> {
        use image::codecs::png::PngEncoder;
        use image::{ColorType, ImageEncoder};

        let expected_len = (width as usize) * (height as usize) * 4;
        if rgba.len() < expected_len {
            anyhow::bail!(
                "RGBA buffer too short: expected at least {} bytes for {}x{}, got {}",
                expected_len,
                width,
                height,
                rgba.len()
            );
        }

        let mut png_buf = Vec::with_capacity(expected_len / 4);
        let encoder = PngEncoder::new(&mut png_buf);
        encoder.write_image(rgba, width, height, ColorType::Rgba8.into())?;
        Ok(png_buf)
    }

    pub fn get_attachments() -> anyhow::Result<super::ClipboardAttachments> {
        // `ContentNotAvailable` is already Ok(None); other file_list errors must not skip get_image when a raster is still present
        let file_urls = match get_file_urls() {
            Ok(urls) => urls,
            Err(e) => {
                tracing::debug!("clipboard file_urls read failed: {e}");
                None
            }
        };
        let (image, read_path) = if file_urls.is_none() {
            let read = get_image()?;
            (read.image, read.read_path)
        } else {
            (None, ClipboardReadPath::Arboard)
        };
        Ok(super::ClipboardAttachments {
            file_urls,
            image,
            read_path,
        })
    }

    /// Read file references via arboard's `file_list()`: `CF_HDROP` on Windows, `text/uri-list` on X11.
    /// Returns newline-joined absolute paths.
    pub fn get_file_urls() -> anyhow::Result<Option<String>> {
        arboard_read_with_deadline(|clipboard| {
            let paths = match clipboard.get().file_list() {
                Ok(p) => p,
                Err(arboard::Error::ContentNotAvailable) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            if paths.is_empty() {
                return Ok(None);
            }
            Ok(Some(
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        })
    }

    /// Contract tests for `spawn_with_deadline` (private to this module, so they live here; any non-macOS test host runs them).
    #[cfg(test)]
    mod worker_deadline_tests {
        use super::*;
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;

        #[test]
        fn fast_closure_returns_value() {
            let result = spawn_with_deadline("test-fast", Duration::from_secs(5), || 42u8);
            assert_eq!(result, Ok(42));
        }

        /// Deadline expiry abandons the worker promptly instead of waiting it out.
        #[test]
        fn slow_closure_times_out() {
            let started = std::time::Instant::now();
            let result = spawn_with_deadline("test-slow", Duration::from_millis(30), || {
                std::thread::sleep(Duration::from_secs(5));
                0u8
            });
            assert_eq!(result, Err(RecvTimeoutError::Timeout));
            assert!(started.elapsed() < Duration::from_secs(5));
        }

        /// A worker that dies before sending is distinguishable from a timeout (callers label it "worker died" rather than "timed out").
        #[test]
        fn panicking_closure_reports_disconnected() {
            let result = spawn_with_deadline("test-panic", Duration::from_secs(5), || -> u8 {
                panic!("worker panic (expected by this test)")
            });
            assert_eq!(result, Err(RecvTimeoutError::Disconnected));
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    mod linux_tests {
        use super::*;
        use crate::clipboard::WaylandDataControlProbe;

        #[test]
        fn primary_read_argv_targets_x11_primary_exactly() {
            assert_eq!(
                XCLIP_SPEC.read_primary,
                Some(["xclip", "-o", "-selection", "primary"].as_slice())
            );
            assert_eq!(
                XSEL_SPEC.read_primary,
                Some(["xsel", "--primary", "--output"].as_slice())
            );
            assert_eq!(WL_SPEC.read_primary, None);
        }

        #[test]
        fn primary_tool_selection_requires_nonempty_display_before_probing() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                false,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(b"text".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Failed);
            assert!(
                calls.borrow().is_empty(),
                "non-X11 sessions must not invoke X11 tools"
            );
        }

        #[test]
        fn successful_xclip_never_discovers_xsel() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(b"from xclip".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Text("from xclip".to_owned()));
            assert_eq!(calls.into_inner(), vec!["discover xclip", "read xclip"]);
        }

        #[test]
        fn primary_tool_discovery_caches_only_positive_results() {
            let cache = std::sync::OnceLock::new();
            let mut probes = 0;
            assert!(!cache_successful_probe(&cache, || {
                probes += 1;
                false
            }));
            assert!(cache_successful_probe(&cache, || {
                probes += 1;
                true
            }));
            assert!(cache_successful_probe(&cache, || {
                probes += 1;
                false
            }));
            assert_eq!(probes, 2, "the positive result must skip later probes");
        }

        #[test]
        fn primary_read_tries_xsel_after_xclip_runtime_failure() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    if spec.name == "xclip" {
                        anyhow::bail!("xclip runtime failure");
                    }
                    Ok(b"from xsel".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Text("from xsel".to_owned()));
            assert_eq!(
                calls.into_inner(),
                vec!["discover xclip", "read xclip", "discover xsel", "read xsel"]
            );
        }

        #[test]
        fn primary_read_treats_successful_empty_as_authoritative() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(Vec::new())
                },
            );

            assert_eq!(read, PrimaryCliRead::Empty);
            assert_eq!(calls.into_inner(), vec!["discover xclip", "read xclip"]);
        }

        #[test]
        fn absent_xclip_discovers_xsel_lazily() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    spec.name == "xsel"
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(b"from xsel".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Text("from xsel".to_owned()));
            assert_eq!(
                calls.into_inner(),
                vec!["discover xclip", "discover xsel", "read xsel"]
            );
        }

        #[test]
        fn primary_read_reports_failure_after_all_backends_fail() {
            let mut calls = Vec::new();
            let read = read_x11_primary_with_tools(
                true,
                |_| true,
                |spec, _| {
                    calls.push(spec.name);
                    anyhow::bail!("runtime failure")
                },
            );

            assert_eq!(read, PrimaryCliRead::Failed);
            assert_eq!(calls, vec!["xclip", "xsel"]);
        }

        #[test]
        fn primary_arboard_fallback_is_x11_only() {
            assert!(primary_arboard_fallback_allowed(true, false));
            assert!(!primary_arboard_fallback_allowed(true, true));
            assert!(!primary_arboard_fallback_allowed(false, false));
            assert!(!primary_arboard_fallback_allowed(false, true));
        }

        #[test]
        fn display_guard_rejects_absent_and_empty_values() {
            use std::ffi::OsStr;
            assert!(!env_value_present(None));
            assert!(!env_value_present(Some(OsStr::new(""))));
            assert!(env_value_present(Some(OsStr::new(":99"))));
        }

        /// Only the Wayland tool may override arboard's `Ok(None)`.
        #[test]
        fn wayland_tool_selected_per_spec() {
            assert!(wayland_tool_selected(Some(&WL_SPEC)));
            assert!(!wayland_tool_selected(Some(&XCLIP_SPEC)));
            assert!(!wayland_tool_selected(Some(&XSEL_SPEC)));
            assert!(!wayland_tool_selected(None));
        }

        /// Typed text/image argv: a missing MIME must not dump raw bytes.
        /// A non-zero exit becomes empty via `run_capture_out`, not a hard probe failure.
        #[test]
        fn wl_paste_content_reads_are_typed() {
            assert!(WL_SPEC.read_text.contains(&"-t"));
            assert!(WL_SPEC.read_text.contains(&"text"));
            let png = WL_SPEC.read_png.expect("wl-paste image read");
            assert!(png.contains(&"-t"));
            assert!(png.contains(&"image/png"));
        }

        #[test]
        fn write_specs_wayland_and_xclip_both_when_available() {
            let specs = collect_linux_write_specs(true, true, true, true, true);
            let names: Vec<_> = specs.iter().map(|s| s.name).collect();
            assert_eq!(names, vec!["wl-copy", "xclip"]);
        }

        #[test]
        fn write_specs_xclip_only_when_wl_missing() {
            let specs = collect_linux_write_specs(true, true, false, true, false);
            let names: Vec<_> = specs.iter().map(|s| s.name).collect();
            assert_eq!(names, vec!["xclip"]);
        }

        #[test]
        fn write_specs_wl_only_on_wayland_without_display() {
            let specs = collect_linux_write_specs(true, false, true, true, false);
            let names: Vec<_> = specs.iter().map(|s| s.name).collect();
            assert_eq!(names, vec!["wl-copy"]);
        }

        #[test]
        fn write_specs_xsel_when_xclip_unavailable() {
            let specs = collect_linux_write_specs(false, true, false, false, true);
            let names: Vec<_> = specs.iter().map(|s| s.name).collect();
            assert_eq!(names, vec!["xsel"]);
        }

        #[test]
        fn write_specs_prefers_xclip_over_xsel() {
            let specs = collect_linux_write_specs(false, true, false, true, true);
            let names: Vec<_> = specs.iter().map(|s| s.name).collect();
            assert_eq!(names, vec!["xclip"]);
        }

        #[test]
        fn readback_exact_match() {
            assert!(wayland_readback_matches(b"hello", "hello"));
            // Verbatim trailing newline must verify (wl-paste --no-newline does not strip it).
            assert!(wayland_readback_matches(b"AB\n", "AB\n"));
        }

        #[test]
        fn readback_empty() {
            assert!(wayland_readback_matches(b"", ""));
        }

        /// The original bug: an empty selection (wl-copy exited 0 but stored nothing) must fail verification.
        #[test]
        fn readback_empty_selection_rejected() {
            assert!(!wayland_readback_matches(b"", "hello"));
            assert!(!wayland_readback_matches(b"", "hello\n"));
        }

        #[test]
        fn retry_succeeds_on_first_attempt_without_retrying() {
            let mut calls = 0;
            let ok = readback_with_retry(
                || {
                    calls += 1;
                    true
                },
                3,
                std::time::Duration::ZERO,
            );
            assert!(ok);
            assert_eq!(calls, 1);
        }

        /// The race this retry exists for: the first read lands before wl-copy's daemonized claim, a later one after.
        #[test]
        fn retry_recovers_from_transient_miss() {
            let mut calls = 0;
            let ok = readback_with_retry(
                || {
                    calls += 1;
                    calls == 3
                },
                3,
                std::time::Duration::ZERO,
            );
            assert!(ok);
            assert_eq!(calls, 3);
        }

        #[test]
        fn retry_bounded_on_persistent_failure() {
            let mut calls = 0;
            let ok = readback_with_retry(
                || {
                    calls += 1;
                    false
                },
                3,
                std::time::Duration::ZERO,
            );
            assert!(!ok);
            assert_eq!(calls, 3);
        }

        /// Read-back gating, exhaustive: skipped only when data-control made the arboard write authoritative; X11 tools never need it.
        #[test]
        fn readback_required_matrix() {
            assert!(!wayland_readback_required(true, true, true));
            assert!(wayland_readback_required(true, true, false));
            assert!(wayland_readback_required(true, false, true));
            assert!(wayland_readback_required(true, false, false));
            assert!(!wayland_readback_required(false, true, true));
            assert!(!wayland_readback_required(false, true, false));
            assert!(!wayland_readback_required(false, false, true));
            assert!(!wayland_readback_required(false, false, false));
        }

        #[test]
        fn data_control_semantic_classification_is_exhaustive() {
            assert_eq!(
                classify_data_control_semantic(DataControlSemanticResult::Present),
                WaylandDataControlProbe::Available(true)
            );
            assert_eq!(
                classify_data_control_semantic(DataControlSemanticResult::MissingProtocol),
                WaylandDataControlProbe::Available(false)
            );
            assert_eq!(
                classify_data_control_semantic(DataControlSemanticResult::NoSeats),
                WaylandDataControlProbe::Unavailable
            );
            assert_eq!(
                classify_data_control_semantic(DataControlSemanticResult::ConnectionUnavailable),
                WaylandDataControlProbe::Unavailable
            );
        }

        #[test]
        fn dependency_error_mapping_pins_directly_constructible_variants() {
            use wl_clipboard_rs::utils::PrimarySelectionCheckError;
            assert_eq!(
                data_control_from_result(Ok(true)),
                WaylandDataControlProbe::Available(true)
            );
            assert_eq!(
                data_control_from_result(Ok(false)),
                WaylandDataControlProbe::Available(true)
            );
            assert_eq!(
                data_control_from_result(Err(PrimarySelectionCheckError::MissingProtocol)),
                WaylandDataControlProbe::Available(false)
            );
            assert_eq!(
                data_control_from_result(Err(PrimarySelectionCheckError::NoSeats)),
                WaylandDataControlProbe::Unavailable
            );
            assert_eq!(
                data_control_from_result(Err(PrimarySelectionCheckError::SocketOpenError(
                    std::io::Error::other("boom")
                ))),
                WaylandDataControlProbe::Unavailable
            );
        }

        /// The permanence bug this policy prevents: an unanswered probe must not decide `false` until the retry cap is exhausted.
        /// The equal-deadline lease init may succeed via data-control moments later.
        #[test]
        fn probe_cache_indefinite_stays_undecided_below_cap() {
            let mut cache = ProbeCache::new();
            for seen in 1..PROBE_INDEFINITE_MAX {
                assert!(!apply_probe_outcome(
                    &mut cache,
                    &WaylandDataControlProbe::Unavailable
                ));
                assert_eq!(cache.decided, None);
                assert_eq!(cache.indefinite_seen, seen);
            }
            // Cap reached: permanently decided false (mirrors lease degradation).
            assert!(!apply_probe_outcome(
                &mut cache,
                &WaylandDataControlProbe::Unavailable
            ));
            assert_eq!(cache.decided, Some(false));
        }

        /// A definitive answer after transient failures decides truth exactly once; the earlier indefinite attempts leave no residue in the decision.
        #[test]
        fn repeated_probe_errors_reach_permanent_false() {
            let mut cache = ProbeCache::new();
            for seen in 1..PROBE_INDEFINITE_MAX {
                assert!(!apply_probe_outcome(
                    &mut cache,
                    &WaylandDataControlProbe::Error("worker died".to_owned())
                ));
                assert_eq!(cache.decided, None);
                assert_eq!(cache.indefinite_seen, seen);
            }
            assert!(!apply_probe_outcome(
                &mut cache,
                &WaylandDataControlProbe::Error("worker died".to_owned())
            ));
            assert_eq!(cache.decided, Some(false));
        }

        #[test]
        fn probe_cache_definitive_after_indefinite_decides_truth() {
            let mut cache = ProbeCache::new();
            assert!(!apply_probe_outcome(
                &mut cache,
                &WaylandDataControlProbe::Unavailable
            ));
            assert!(apply_probe_outcome(
                &mut cache,
                &WaylandDataControlProbe::Available(true)
            ));
            assert_eq!(cache.decided, Some(true));
            assert_eq!(cache.indefinite_seen, 1);
        }

        #[test]
        fn probe_cache_definitive_false_decides_immediately() {
            let mut cache = ProbeCache::new();
            assert!(!apply_probe_outcome(
                &mut cache,
                &WaylandDataControlProbe::Available(false)
            ));
            assert_eq!(cache.decided, Some(false));
        }

        /// Once decided, callers take the fast path: the probe never runs again and a later (contradictory) probe cannot flip the answer.
        #[test]
        fn decided_cache_skips_further_probes() {
            let cache = parking_lot::Mutex::new(ProbeCache::new());
            let calls = std::cell::Cell::new(0u32);
            assert!(cached_probe_answer(&cache, || {
                calls.set(calls.get() + 1);
                WaylandDataControlProbe::Available(true)
            }));
            assert!(cached_probe_answer(&cache, || {
                calls.set(calls.get() + 1);
                WaylandDataControlProbe::Available(false)
            }));
            assert_eq!(calls.get(), 1);
        }

        /// Concurrent callers serialize on the cache lock: one probe execution, one shared answer.
        /// A burst can't stack indefinite outcomes past the cap or drop a definitive answer.
        #[test]
        fn concurrent_callers_share_one_probe() {
            use std::sync::atomic::{AtomicU32, Ordering};
            let cache = parking_lot::Mutex::new(ProbeCache::new());
            let calls = AtomicU32::new(0);
            std::thread::scope(|s| {
                for _ in 0..4 {
                    s.spawn(|| {
                        let answer = cached_probe_answer(&cache, || {
                            calls.fetch_add(1, Ordering::Relaxed);
                            // Hold the lock long enough that peers must wait.
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            WaylandDataControlProbe::Available(true)
                        });
                        assert!(answer);
                    });
                }
            });
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    mod mac_command_status {
        use std::os::unix::process::ExitStatusExt;

        fn output(code: i32, stdout: &[u8]) -> std::process::Output {
            std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.to_vec(),
                stderr: b"backend failed".to_vec(),
            }
        }

        #[test]
        fn exit_zero_empty_is_successful_emptiness() {
            assert_eq!(
                super::super::platform::checked_command_stdout("pbpaste", Ok(output(0, b"")))
                    .expect("exit-zero empty output"),
                Vec::<u8>::new()
            );
        }

        #[test]
        fn exit_zero_output_is_preserved() {
            assert_eq!(
                super::super::platform::checked_command_stdout(
                    "pbpaste",
                    Ok(output(0, b"exact\n"))
                )
                .expect("exit-zero output"),
                b"exact\n"
            );
        }

        #[test]
        fn nonzero_and_spawn_failure_are_errors() {
            assert!(
                super::super::platform::checked_command_stdout(
                    "pbpaste",
                    Ok(output(1, b"ignored"))
                )
                .is_err()
            );
            assert!(
                super::super::platform::checked_command_stdout(
                    "osascript",
                    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"))
                )
                .is_err()
            );
        }
    }

    // -----------------------------------------------------------------------
    // wait_with_deadline (real child processes; unix `sleep`)
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[allow(clippy::disallowed_methods)] // test fixture; the test kills it
    fn spawn_sleep(seconds: &str) -> std::process::Child {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg(seconds)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        distill_tty_utils::detach_std_command(&mut cmd);
        cmd.spawn().expect("spawn sleep")
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_deadline_returns_status_of_fast_child() {
        let mut child = spawn_sleep("0");
        let status = wait_with_deadline(&mut child, std::time::Duration::from_secs(5))
            .expect("fast child must be reaped in time");
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_deadline_kills_hung_child_and_reports_timeout() {
        let mut child = spawn_sleep("30");
        let started = std::time::Instant::now();
        let err = wait_with_deadline(&mut child, std::time::Duration::from_millis(50))
            .expect_err("deadline must expire");
        assert!(
            err.downcast_ref::<WaitTimeout>().is_some(),
            "timeout must be distinguishable, got: {err}"
        );
        // Killed and reaped, not waited for the full 30 s.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(child.try_wait().expect("child reaped").is_some());
    }

    // -----------------------------------------------------------------------
    // spool_for_stdin (unlink-then-read contract)
    // -----------------------------------------------------------------------

    /// The returned fd must stay readable after the temp file is unlinked on return.
    /// A payload well past the ~64 KiB pipe buffer (the reason the spool exists) must round-trip intact.
    #[test]
    fn spool_for_stdin_round_trips_large_payload_after_unlink() {
        use std::io::Read;
        let payload: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let mut file = spool_for_stdin(&payload).expect("spool payload");
        let mut read_back = Vec::new();
        file.read_to_end(&mut read_back).expect("read spooled file");
        assert_eq!(read_back, payload);
    }

    // -----------------------------------------------------------------------
    // OSC 52 sequence construction (pure; base64("hi") == "aGk=")
    // -----------------------------------------------------------------------

    #[test]
    fn osc52_sequence_plain() {
        assert_eq!(osc52_sequence("hi", false), b"\x1b]52;c;aGk=\x07".to_vec());
    }

    #[test]
    fn osc52_sequence_tmux_passthrough() {
        assert_eq!(
            osc52_sequence("hi", true),
            b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\".to_vec()
        );
    }

    // -----------------------------------------------------------------------
    // MIME detection from magic bytes
    // -----------------------------------------------------------------------

    #[test]
    fn mime_from_bytes_png() {
        let header = b"\x89PNG\r\n\x1a\nrest";
        assert_eq!(mime_from_bytes(header), "image/png");
    }

    #[test]
    fn mime_from_bytes_jpeg() {
        let header = b"\xff\xd8\xffrest";
        assert_eq!(mime_from_bytes(header), "image/jpeg");
    }

    #[test]
    fn mime_from_bytes_tiff_le() {
        let header = b"II\x2a\x00rest";
        assert_eq!(mime_from_bytes(header), "image/tiff");
    }

    #[test]
    fn mime_from_bytes_tiff_be() {
        let header = b"MM\x00\x2arest";
        assert_eq!(mime_from_bytes(header), "image/tiff");
    }

    #[test]
    fn mime_from_bytes_gif() {
        assert_eq!(mime_from_bytes(b"GIF89a"), "image/gif");
        assert_eq!(mime_from_bytes(b"GIF87a"), "image/gif");
    }

    #[test]
    fn mime_from_bytes_webp() {
        let header = b"RIFF\x00\x00\x00\x00WEBP";
        assert_eq!(mime_from_bytes(header), "image/webp");
    }

    #[test]
    fn mime_from_bytes_bmp() {
        assert_eq!(mime_from_bytes(b"BM\x00\x00"), "image/bmp");
    }

    #[test]
    fn mime_from_bytes_unknown() {
        assert_eq!(mime_from_bytes(b""), "application/octet-stream");
        assert_eq!(mime_from_bytes(b"\x00\x01"), "application/octet-stream");
    }

    // -----------------------------------------------------------------------
    // MIME to extension mapping
    // -----------------------------------------------------------------------

    #[test]
    fn mime_to_extension_known() {
        assert_eq!(mime_to_extension("image/png"), "png");
        assert_eq!(mime_to_extension("image/jpeg"), "jpg");
        assert_eq!(mime_to_extension("image/tiff"), "tiff");
        assert_eq!(mime_to_extension("image/gif"), "gif");
        assert_eq!(mime_to_extension("image/webp"), "webp");
        assert_eq!(mime_to_extension("image/bmp"), "bmp");
    }

    #[test]
    fn mime_to_extension_unknown() {
        assert_eq!(mime_to_extension("application/octet-stream"), "bin");
        assert_eq!(mime_to_extension("text/plain"), "bin");
    }

    // -----------------------------------------------------------------------
    // Linux RGBA-to-PNG encoding (only compiled on non-macOS)
    // -----------------------------------------------------------------------

    #[cfg(not(target_os = "macos"))]
    mod linux_encoding {
        use super::super::platform::encode_rgba_to_png;
        use super::*;

        #[test]
        fn encode_1x1_red_pixel() {
            // 1x1 RGBA: fully opaque red
            let rgba = [255u8, 0, 0, 255];
            let png = encode_rgba_to_png(&rgba, 1, 1).expect("encoding failed");
            assert_eq!(mime_from_bytes(&png), "image/png");
            assert!(png.len() > 8, "PNG output too short");
        }

        #[test]
        fn encode_2x2_pixels() {
            // 2x2 RGBA
            let rgba = [0u8; 16];
            let png = encode_rgba_to_png(&rgba, 2, 2).expect("encoding failed");
            assert_eq!(mime_from_bytes(&png), "image/png");
        }

        #[test]
        fn encode_buffer_too_short() {
            // A 2x2 image needs 16 bytes; this provides only 8
            let rgba = [0u8; 8];
            let result = encode_rgba_to_png(&rgba, 2, 2);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("RGBA buffer too short"),
                "unexpected error: {msg}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // macOS unified attachments osascript stdout protocol (pure parsing)
    // -----------------------------------------------------------------------

    mod attachments_protocol_tests {
        use super::super::attachments_protocol::{
            FURL_MARKER, IMAGE_MARKER, RasterClass, parse_attachments_output,
            parse_osascript_furl_output,
        };

        fn sample_output(furl_body: &str, image: &str) -> String {
            format!("{FURL_MARKER}\n{furl_body}\n{IMAGE_MARKER}\nIMAGE:{image}")
        }

        // -----------------------------------------------------------
        // parse_osascript_furl_output: deterministic parsing surface
        // -----------------------------------------------------------

        #[test]
        fn parse_furl_empty_inputs_are_none() {
            for raw in ["", "\n", "   \n\t  ", "\r\n", "   \n  \t  \n"] {
                assert!(
                    parse_osascript_furl_output(raw).is_none(),
                    "expected None for {raw:?}"
                );
            }
        }

        #[test]
        fn parse_furl_none_sentinel_is_none() {
            for raw in ["none", "none\n", "  none  ", "none\r\n"] {
                assert!(
                    parse_osascript_furl_output(raw).is_none(),
                    "expected None for {raw:?}"
                );
            }
        }

        #[test]
        fn parse_furl_single_path_trailing_newline() {
            assert_eq!(
                parse_osascript_furl_output("/tmp/foo.txt\n"),
                Some("/tmp/foo.txt".to_owned()),
            );
        }

        #[test]
        fn parse_furl_multi_path_trailing_newline() {
            // The AppleScript list path appends `\n` after every entry, so multi-file output ends with a trailing newline that `trim` strips
            // Internal newlines are preserved verbatim; the drop-path parser splits on them
            assert_eq!(
                parse_osascript_furl_output("/a\n/b\n"),
                Some("/a\n/b".to_owned()),
            );
        }

        #[test]
        fn parse_furl_multi_path_no_trailing_newline() {
            assert_eq!(
                parse_osascript_furl_output("/a\n/b"),
                Some("/a\n/b".to_owned()),
            );
        }

        // -----------------------------------------------------------
        // parse_attachments_output: unified osascript stdout protocol
        // -----------------------------------------------------------

        #[test]
        fn parse_attachments_none_none() {
            let (urls, class) = parse_attachments_output(&sample_output("none", "NONE"));
            assert!(urls.is_none());
            assert!(class.is_none());
        }

        #[test]
        fn parse_attachments_furl_only_skips_image_probe() {
            let raw = sample_output("/tmp/a.png\n", "NONE");
            let (urls, class) = parse_attachments_output(&raw);
            assert_eq!(urls.as_deref(), Some("/tmp/a.png"));
            assert!(class.is_none());
        }

        #[test]
        fn parse_attachments_multi_furl() {
            let raw = sample_output("/a\n/b\n", "NONE");
            let (urls, class) = parse_attachments_output(&raw);
            assert_eq!(urls.as_deref(), Some("/a\n/b"));
            assert!(class.is_none());
        }

        #[test]
        fn parse_attachments_image_classes() {
            for expected in RasterClass::ALL {
                let raw = sample_output("none", expected.code());
                let (urls, class) = parse_attachments_output(&raw);
                assert!(urls.is_none(), "furl should be none for {expected:?}");
                assert_eq!(class, Some(expected));
            }
        }

        #[test]
        fn parse_attachments_unknown_image_class_is_none() {
            for image in ["WEBP", "GIFf", "unknown", "JPEGAufs", ""] {
                let raw = sample_output("none", image);
                let (urls, class) = parse_attachments_output(&raw);
                assert!(urls.is_none(), "furl should be none for {image}");
                assert!(class.is_none(), "unknown class {image:?} should not parse");
            }
        }

        #[test]
        fn parse_attachments_furl_and_pngf_combo() {
            let raw = sample_output("/tmp/a.png\n", "PNGf");
            let (urls, class) = parse_attachments_output(&raw);
            assert_eq!(urls.as_deref(), Some("/tmp/a.png"));
            assert_eq!(class, Some(RasterClass::Png));
        }

        #[test]
        fn parse_attachments_empty_stdout() {
            assert_eq!(parse_attachments_output(""), (None, None));
            assert_eq!(parse_attachments_output("   \n\t  "), (None, None));
        }

        #[test]
        fn parse_attachments_furl_only_without_image_marker() {
            let raw = format!("{FURL_MARKER}\n/a\n/b\n");
            let (urls, class) = parse_attachments_output(&raw);
            assert_eq!(urls.as_deref(), Some("/a\n/b"));
            assert!(class.is_none());
        }

        #[test]
        fn parse_attachments_crlf_markers() {
            let raw = format!("{FURL_MARKER}\r\n/tmp/a.png\r\n{IMAGE_MARKER}\r\nIMAGE:PNGf");
            let (urls, class) = parse_attachments_output(&raw);
            assert_eq!(urls.as_deref(), Some("/tmp/a.png"));
            assert_eq!(class, Some(RasterClass::Png));
        }

        #[test]
        fn parse_attachments_whitespace_only_furl_body() {
            let (urls, class) = parse_attachments_output(&sample_output("   \n  \t  ", "NONE"));
            assert!(urls.is_none());
            assert!(class.is_none());
        }

        #[test]
        fn parse_attachments_malformed_returns_none() {
            assert_eq!(parse_attachments_output("garbage"), (None, None));
            // FURL section without IMAGE marker still yields paths.
            assert_eq!(
                parse_attachments_output("<<<FURL>>>\n/tmp/foo.txt"),
                (Some("/tmp/foo.txt".to_owned()), None),
            );
        }
    }

    // -----------------------------------------------------------------------
    // osascript raster hand-off
    // -----------------------------------------------------------------------

    mod probe_temps_tests {
        use super::super::attachments_protocol::{
            FURL_MARKER, IMAGE_MARKER, ProbeTemps, RasterClass, applescript_string,
            attachments_osascript, image_osascript, osascript_attachments, raster_try_chain,
        };

        /// Pins the generated chain: `osascript` runs only on macOS, so a generator slip would pass Linux CI and fail every fallback paste.
        #[test]
        fn raster_try_chain_matches_its_golden_script() {
            let temps = ProbeTemps::new().expect("probe temps");
            let path = applescript_string(&temps.path().display().to_string());
            let write = |class: RasterClass| {
                format!(
                    "try\n\
                     set imgData to the clipboard as \u{00AB}class {code}\u{00BB}\n\
                     set filePath to POSIX file {path} as text\n\
                     set fRef to open for access file filePath with write permission\n\
                     set eof of fRef to 0\n\
                     write imgData to fRef\n\
                     close access fRef\n\
                     set imageOut to \"{code}\"\n\
                     on error\n",
                    code = class.code(),
                )
            };
            let expected = format!(
                "{png}{tiff}{jpeg}end try\nend try\nend try\n",
                png = write(RasterClass::Png),
                tiff = write(RasterClass::Tiff),
                jpeg = write(RasterClass::Jpeg),
            );
            assert_eq!(expected, raster_try_chain(&temps));
        }

        /// The image script returns what the chain set; the attachments script gates the same chain on no file URL.
        #[test]
        fn both_osascripts_embed_the_raster_chain() {
            let temps = ProbeTemps::new().expect("probe temps");
            let chain = raster_try_chain(&temps);
            assert_eq!(
                format!("set imageOut to \"none\"\n{chain}return imageOut"),
                image_osascript(&temps)
            );
            let attachments = attachments_osascript(Some(&temps));
            assert!(
                attachments.contains(&format!("if furlOut is \"none\" then\n{chain}end if\n")),
                "{attachments}"
            );
        }

        #[test]
        fn applescript_string_escapes_quotes_and_backslashes() {
            assert_eq!(applescript_string("plain"), "\"plain\"");
            assert_eq!(
                applescript_string(r#"a "quoted" \ path"#),
                r#""a \"quoted\" \\ path""#
            );
        }

        fn attachments_stdout(furl: &str, image: &str) -> Vec<u8> {
            format!("{FURL_MARKER}\n{furl}\n{IMAGE_MARKER}\nIMAGE:{image}").into_bytes()
        }

        /// The class on stdout selects the MIME; the bytes come from this probe's file.
        #[test]
        fn osascript_attachments_reads_the_reported_class_from_the_probe_file() {
            let temps = ProbeTemps::new().expect("probe temps");
            std::fs::write(temps.path(), b"MM\x00\x2a").expect("write");
            let read = osascript_attachments(Ok(temps), |_| Ok(attachments_stdout("none", "TIFF")))
                .expect("raster read");
            let image = read.image.expect("raster attached");
            assert_eq!("image/tiff", image.mime_type);
            assert_eq!(b"MM\x00\x2a".to_vec(), image.data);
            assert!(read.file_urls.is_none());
        }

        /// Without a scratch dir the furl half still answers a file paste; a board that answers nothing is unreadable, not empty.
        #[test]
        fn osascript_attachments_without_a_scratch_dir_answers_file_urls_or_fails() {
            let no_dir = || Err(anyhow::anyhow!("no scratch dir"));
            let furl_only = |stdout: Vec<u8>| {
                move |script: &str| {
                    assert!(
                        !script.contains("open for access"),
                        "raster half must be skipped"
                    );
                    Ok(stdout)
                }
            };
            let file = osascript_attachments(
                no_dir(),
                furl_only(attachments_stdout("/tmp/a.png", "NONE")),
            )
            .expect("a file paste still answers");
            assert_eq!(Some("/tmp/a.png"), file.file_urls.as_deref());
            assert!(file.image.is_none());

            let err =
                osascript_attachments(no_dir(), furl_only(attachments_stdout("none", "NONE")))
                    .expect_err("no scratch dir and no file URL is not an empty board");
            assert!(err.to_string().contains("no scratch dir"), "{err}");
        }

        /// Without a scratch dir the script still answers file URLs and skips the raster half.
        #[test]
        fn attachments_osascript_without_temps_is_furl_only() {
            let script = attachments_osascript(None);
            assert!(script.contains("class furl"));
            assert!(script.contains(FURL_MARKER) && script.contains(IMAGE_MARKER));
            assert!(
                !script.contains("class PNGf") && !script.contains("open for access"),
                "raster coercion must be skipped without a scratch dir"
            );
        }

        /// The pipe readers hand back the script's stdout, and a script error's stderr reaches the error message.
        #[cfg(target_os = "macos")]
        #[test]
        fn osascript_stdout_and_stderr_are_collected() {
            let run = |script: &str| {
                super::super::platform::run_osascript_with_deadline(
                    script,
                    std::time::Duration::from_secs(20),
                )
            };
            assert_eq!(
                b"PNGf\n".to_vec(),
                run("return \"PNGf\"").expect("script ran")
            );
            let err = run("error \"pasteboard busy\"").expect_err("script error");
            assert!(err.to_string().contains("pasteboard busy"), "{err}");
        }

        /// A script that never returns is killed at the deadline and the kill reaches the caller as [`WaitTimeout`].
        #[cfg(target_os = "macos")]
        #[test]
        fn hung_osascript_is_killed_at_the_deadline() {
            let started = std::time::Instant::now();
            let err = super::super::platform::run_osascript_with_deadline(
                "delay 30",
                std::time::Duration::from_millis(100),
            )
            .expect_err("the script outlives the deadline");
            assert!(
                err.downcast_ref::<super::super::WaitTimeout>().is_some(),
                "{err}"
            );
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
        }

        /// Two live probes never share a file; the dir is owner-only and gone on drop.
        #[test]
        fn each_probe_gets_a_private_dir_removed_on_drop() {
            let a = ProbeTemps::new().expect("probe temps");
            let b = ProbeTemps::new().expect("probe temps");
            assert_ne!(a.path(), b.path(), "probe file shared between probes");
            let dir = a.path().parent().expect("probe dir").to_path_buf();
            assert!(dir.is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&dir)
                    .expect("probe dir")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o700, "probe dir must be owner-only, got {mode:o}");
            }
            drop(a);
            assert!(
                !dir.exists(),
                "dropping the probe must remove its scratch dir"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Fast-probe type-list classification (clipboard_image_snapshot)
    // -----------------------------------------------------------------------

    fn types<'a>(list: &'a [&'static [u8]]) -> impl Iterator<Item = &'static [u8]> + 'a {
        list.iter().copied()
    }

    /// A screenshot/image copy advertises raster types with no file URLs: pasteable.
    #[test]
    fn image_copy_is_pasteable() {
        assert!(image_pasteable_from_types(types(&[
            b"public.png",
            b"public.tiff"
        ])));
        assert!(image_pasteable_from_types(types(&[b"public.jpeg"])));
    }

    /// A Finder file copy advertises a file-icon raster ALONGSIDE file URLs.
    /// Paste routes those through path handling («class furl» wins in `get_attachments`), so the board does NOT hold a pasteable image.
    /// Type order does not matter.
    #[test]
    fn finder_file_copy_is_not_pasteable_image() {
        assert!(!image_pasteable_from_types(types(&[
            b"public.file-url",
            b"public.tiff",
        ])));
        assert!(!image_pasteable_from_types(types(&[
            b"public.tiff",
            b"public.file-url",
        ])));
        // Pre-UTI spelling of the same advertisement.
        assert!(!image_pasteable_from_types(types(&[
            b"public.png",
            b"NSFilenamesPboardType",
        ])));
    }

    /// No raster types at all (plain text, etc.): not an image.
    #[test]
    fn text_only_board_is_not_image() {
        assert!(!image_pasteable_from_types(types(&[
            b"public.utf8-plain-text"
        ])));
        assert!(!image_pasteable_from_types(types(&[])));
    }

    // -----------------------------------------------------------------------
    // Native paste-time read type selection (native_image_type_from_types)
    // -----------------------------------------------------------------------

    /// The native read requests raster types in the osascript coercion order (PNG first, TIFF, then JPEG), regardless of advertised order.
    #[test]
    fn native_type_priority_matches_osascript_order() {
        assert_eq!(
            native_image_type_from_types(&[b"public.tiff".as_slice(), b"public.png"]),
            Some((b"public.png".as_slice(), "image/png")),
        );
        assert_eq!(
            native_image_type_from_types(&[b"public.jpeg".as_slice(), b"public.tiff"]),
            Some((b"public.tiff".as_slice(), "image/tiff")),
        );
        assert_eq!(
            native_image_type_from_types(&[b"public.jpeg".as_slice()]),
            Some((b"public.jpeg".as_slice(), "image/jpeg")),
        );
    }

    /// File-URL advertisements force the osascript furl path (`None`): the native read must never swallow a Finder file copy as raster.
    #[test]
    fn native_type_defers_to_furl_path() {
        assert_eq!(
            native_image_type_from_types(&[b"public.file-url".as_slice(), b"public.png"]),
            None,
        );
        assert_eq!(
            native_image_type_from_types(&[b"public.png".as_slice(), b"NSFilenamesPboardType"]),
            None,
        );
    }

    /// Raster-less boards (text, empty) never native-read.
    #[test]
    fn native_type_none_without_raster() {
        assert_eq!(
            native_image_type_from_types(&[b"public.utf8-plain-text".as_slice()]),
            None,
        );
        let empty: [&[u8]; 0] = [];
        assert_eq!(native_image_type_from_types(&empty), None);
    }
}
