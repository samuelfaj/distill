// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared URL-opening and scheme validation utilities.
//!
//! Keyboard navigation, mouse clicks, and action dispatch all open links through these helpers.

use std::collections::HashMap;

use crate::terminal::hyperlinks::SchemeFilter;

/// Outcome of attempting to open a URL in the system browser/handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenUrlResult {
    /// Opener was launched (or `GROK_TEST_OPEN_URL_FILE` recorded the URL).
    Opened,
    /// Scheme was rejected by the safety filter.
    RejectedScheme,
    /// Browser cannot run here (headless / no display) or the opener failed to spawn.
    /// Callers should show the URL so the user can open it manually.
    BrowserUnavailable,
}

/// Linux/BSD needs non-empty `DISPLAY`, `WAYLAND_DISPLAY`, or `BROWSER`; macOS/Windows always return true.
/// Pure for a fixed env map. A true result is not a successful spawn.
pub fn browser_open_likely_available_from_env(env: &HashMap<String, String>) -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    // Explicit BROWSER override: allow even without a display server so scripted/headless setups that point at a CLI browser still try
    if env.get("BROWSER").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    env.get("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || env.get("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Whether this process likely has a GUI browser available right now.
pub fn browser_open_likely_available() -> bool {
    let env = crate::host::collect_unicode_env();
    browser_open_likely_available_from_env(&env)
}

const BROWSER_UNAVAILABLE_NOTICE: &str = "Could not open a browser. Open this URL manually";

/// Multi-line copy for agent scrollback: notice, then the full URL alone so it is easy to select/copy in the TUI.
pub fn browser_unavailable_message(url: &str) -> String {
    format!("{BROWSER_UNAVAILABLE_NOTICE}:\n{url}")
}

/// Single-line welcome toast: URL first so prefix truncation keeps the destination.
/// `copied` is true only when clipboard delivery reported success, so the toast never claims a copy that did not happen.
pub fn browser_unavailable_line(url: &str, copied: bool) -> String {
    if copied {
        format!("{url} \u{00b7} {BROWSER_UNAVAILABLE_NOTICE} (URL copied)")
    } else {
        format!("{url} \u{00b7} {BROWSER_UNAVAILABLE_NOTICE}")
    }
}

/// Native opener with detached stdio so a GUI helper cannot block the pager.
/// `false` on headless or spawn failure — show the URL. Untrusted input must use [`is_safe_to_open`] or [`try_open_url`].
pub fn open_url(url: &str) -> bool {
    // PTY e2e tests must see the open without launching a real browser
    // When this env var is set, append the URL to the file and skip the OS opener
    if let Ok(path) = std::env::var("GROK_TEST_OPEN_URL_FILE") {
        use std::io::Write;
        // Report the failed write: swallowing it leaves the PTY test failing with a generic timeout and no clue why
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{url}"))
        {
            tracing::warn!(error = %e, path, "GROK_TEST_OPEN_URL_FILE write failed");
            return false;
        }
        return true;
    }

    // Skip the doomed spawn on headless Linux VMs (no DISPLAY or Wayland)
    // Billing Upgrade and Buy-credits clicks then fall back to showing the URL instead of silently doing nothing
    if !browser_open_likely_available() {
        tracing::info!("skipping browser open: no display server / BROWSER");
        return false;
    }

    let opened = spawn_url_opener(url);
    if !opened {
        // Redact URL to avoid leaking sensitive query params to logs.
        let redacted = url::Url::parse(url)
            .map(|mut u| {
                u.set_query(None);
                u.set_fragment(None);
                u.to_string()
            })
            .unwrap_or_else(|_| "<unparseable>".to_string());
        tracing::warn!(url = %redacted, "failed to open URL");
    }
    opened
}

/// Hand the URL to the default browser via `ShellExecuteW`, which takes it as a single argument.
/// The URL must never pass through `cmd.exe`: `start` splits on `&` and expands `%VAR%`.
/// A server-supplied URL such as `https://example.com/&calc.exe` would execute a command.
#[cfg(target_os = "windows")]
fn spawn_url_opener(url: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let verb = wide("open");
    let target = wide(url);
    // SAFETY: `verb` and `target` are NUL-terminated UTF-16 buffers that outlive the call;
    // optional hwnd/params/directory are null. The pseudo-HINSTANCE is only compared
    // (>32 means success), never dereferenced.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    result as usize > 32
}

#[cfg(not(target_os = "windows"))]
#[allow(clippy::disallowed_methods)] // fire and forget; the child is reaped when this process exits
fn spawn_url_opener(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";

    let mut command = std::process::Command::new(cmd);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    distill_tools::util::detach_std_command(&mut command);
    match command.spawn() {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(error = %e, "URL opener failed to spawn");
            false
        }
    }
}

/// `detach_std_command` keeps the GUI helper off the TUI's `/dev/tty`. Path is one argv, never a shell string.
/// Windows uses [`reveal_in_explorer`]. Split out so tests can inspect the command without spawning.
#[cfg(not(target_os = "windows"))]
fn build_open_path_command(path: &std::path::Path) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(target_os = "macos"))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    distill_tty_utils::detach_std_command(&mut command);
    command
}

/// Trusted filesystem path (no scheme check, unlike [`open_url`]).
/// Windows avoids `cmd /c start`: `%VAR%` expansion corrupts percent-encoded session paths.
#[allow(clippy::disallowed_methods)] // fire and forget; the child is reaped when this process exits
pub fn open_path(path: &std::path::Path) -> bool {
    // Never launch a real GUI app in tests.
    #[cfg(test)]
    {
        !path.as_os_str().is_empty()
    }
    #[cfg(all(not(test), target_os = "windows"))]
    {
        reveal_in_explorer(path)
    }
    #[cfg(all(not(test), not(target_os = "windows")))]
    {
        match build_open_path_command(path).spawn() {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to open file natively");
                false
            }
        }
    }
}

/// `raw_arg` keeps Explorer's `/select,"<path>"` quoting; no `cmd`, so `%` in urlencoded paths is not expanded.
/// A missing file opens the parent folder (no `/select`) instead of Home.
#[cfg(all(not(test), target_os = "windows"))]
#[allow(clippy::disallowed_methods)] // fire and forget; the child is reaped when this process exits
fn reveal_in_explorer(path: &std::path::Path) -> bool {
    use std::os::windows::process::CommandExt;

    // Prefer the real on-disk location (absolute)
    // Fall back to the parent when the file was deleted so Explorer does not dump the user in Home
    let target = if path.is_file() || path.is_dir() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent().filter(|p| p.is_dir()) {
        tracing::debug!(
            path = %path.display(),
            parent = %parent.display(),
            "media path missing; opening parent folder in Explorer"
        );
        parent.to_path_buf()
    } else {
        path.to_path_buf()
    };

    let select_file = target.is_file();
    let mut command = std::process::Command::new("explorer");
    // Escape embedded double-quotes in the path so the `/select,"<path>"` quoting does not break
    // Windows file-system paths cannot legally contain `"`, but percent-decoded display paths or future user-chosen filenames could, so be defensive
    let escaped = target.display().to_string().replace('"', "\"\"");
    if select_file {
        command.raw_arg(format!("/select,\"{}\"", escaped));
    } else {
        // Open the folder itself (no /select); this works for dirs and as a fallback when we only have a parent path
        command.raw_arg(format!("\"{}\"", escaped));
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    distill_tty_utils::detach_std_command(&mut command);
    // explorer.exe returns exit code 1 even on success, so a successful spawn is the best signal we have
    match command.spawn() {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(path = %target.display(), error = %e, "failed to reveal file in Explorer");
            false
        }
    }
}

/// Check if a URL's scheme is safe to open.
///
/// Uses the `url` crate for scheme extraction, falling back to prefix matching for non-standard URLs that `url::Url::parse` rejects.
pub fn is_safe_to_open(url: &str, filter: SchemeFilter) -> bool {
    let url = url.trim();
    if let Ok(parsed) = url::Url::parse(url) {
        return filter.allows(parsed.scheme());
    }
    // Fallback: check for scheme via "://" prefix, lowercasing for case-insensitive comparison (SchemeFilter matches lowercase literals)
    if let Some((scheme, _)) = url.split_once("://") {
        return filter.allows(&scheme.to_ascii_lowercase());
    }
    // Defensive: url::Url::parse handles well-formed mailto, but guard against edge cases where the parser rejects a mailto-like string
    if let Some((scheme, _)) = url.split_once(':')
        && scheme.eq_ignore_ascii_case("mailto")
    {
        return filter.allows(&scheme.to_ascii_lowercase());
    }
    false
}

/// `true` only when the scheme is allowed and the opener launched.
/// Use [`try_open_url`] to tell scheme rejection from a missing browser.
pub fn open_url_if_safe(url: &str, filter: SchemeFilter) -> bool {
    matches!(try_open_url(url, filter), OpenUrlResult::Opened)
}

/// Validate scheme and attempt to open.
/// Prefer this when the caller needs to show the URL for manual opening on [`OpenUrlResult::BrowserUnavailable`].
pub fn try_open_url(url: &str, filter: SchemeFilter) -> OpenUrlResult {
    if !is_safe_to_open(url, filter) {
        tracing::debug!(url, "URL scheme not permitted");
        return OpenUrlResult::RejectedScheme;
    }
    if open_url(url) {
        OpenUrlResult::Opened
    } else {
        OpenUrlResult::BrowserUnavailable
    }
}

/// Leave an existing parameter untouched; on parse failure return the original so untrusted opener input stays safe.
/// SuperGrok upsell stamps the param regardless of the remote `gate_url`.
pub fn ensure_query_param(url: &str, key: &str, value: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let already_present = parsed.query_pairs().any(|(k, _)| k == key);
    if already_present {
        return parsed.to_string();
    }
    parsed.query_pairs_mut().append_pair(key, value);
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_path_command_passes_path_as_a_single_arg() {
        // Path with spaces must be one argument, never shell-interpolated.
        let path = std::path::Path::new("/tmp/grok session/image 1.jpg");
        let command = build_open_path_command(path);
        let args: Vec<_> = command.get_args().map(|a| a.to_os_string()).collect();
        assert!(args.contains(&path.as_os_str().to_os_string()));
    }

    #[test]
    fn standard_http_schemes_allowed() {
        assert!(is_safe_to_open(
            "http://example.com",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "https://example.com/path?q=1",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn mailto_allowed() {
        assert!(is_safe_to_open(
            "mailto:user@example.com",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn file_scheme_blocked_by_standard() {
        // file:// removed from Standard to prevent local file / SSRF attacks.
        assert!(!is_safe_to_open(
            "file:///home/user/doc.pdf",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "file:///home/user/doc.pdf",
            SchemeFilter::EditorExtended
        ));
    }

    #[test]
    fn javascript_scheme_blocked() {
        assert!(!is_safe_to_open(
            "javascript:alert(1)",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn data_scheme_blocked() {
        assert!(!is_safe_to_open(
            "data:text/html,<h1>hi</h1>",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn empty_and_garbage_rejected() {
        assert!(!is_safe_to_open("", SchemeFilter::Standard));
        assert!(!is_safe_to_open("not-a-url", SchemeFilter::Standard));
        assert!(!is_safe_to_open(
            "://missing-scheme",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn editor_schemes_with_extended_filter() {
        assert!(is_safe_to_open(
            "vscode://file/path",
            SchemeFilter::EditorExtended
        ));
        assert!(is_safe_to_open(
            "cursor://open",
            SchemeFilter::EditorExtended
        ));
        assert!(is_safe_to_open("idea://open", SchemeFilter::EditorExtended));
        assert!(is_safe_to_open("zed://open", SchemeFilter::EditorExtended));
    }

    #[test]
    fn editor_schemes_blocked_by_standard_filter() {
        assert!(!is_safe_to_open(
            "vscode://file/path",
            SchemeFilter::Standard
        ));
        assert!(!is_safe_to_open("cursor://open", SchemeFilter::Standard));
    }

    #[test]
    fn scheme_case_sensitivity() {
        // url::Url normalizes to lowercase
        assert!(is_safe_to_open(
            "HTTP://EXAMPLE.COM",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "HTTPS://EXAMPLE.COM",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn url_with_fragment_and_query() {
        assert!(is_safe_to_open(
            "https://example.com/page?key=val#section",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn ftp_scheme_blocked() {
        assert!(!is_safe_to_open(
            "ftp://files.example.com/pub",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn fallback_colon_slash_slash_path() {
        // A custom scheme that url::Url may reject but has ://
        assert!(!is_safe_to_open(
            "custom://something",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn non_mailto_colon_without_slashes_rejected() {
        assert!(!is_safe_to_open("tel:+1234567890", SchemeFilter::Standard));
    }

    #[test]
    fn whitespace_trimmed_before_parse() {
        assert!(is_safe_to_open(
            "  https://example.com  ",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "\thttps://example.com\n",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn ensure_query_param_appends_when_missing() {
        let out = ensure_query_param("https://grok.com/supergrok", "referrer", "distill");
        assert_eq!(out, "https://grok.com/supergrok?referrer=distill");
    }

    #[test]
    fn ensure_query_param_preserves_existing_value() {
        let out = ensure_query_param(
            "https://grok.com/supergrok?referrer=other",
            "referrer",
            "distill",
        );
        assert_eq!(out, "https://grok.com/supergrok?referrer=other");
    }

    #[test]
    fn ensure_query_param_keeps_other_query_pairs() {
        let out = ensure_query_param(
            "https://grok.com/supergrok?heavy=1",
            "referrer",
            "distill",
        );
        assert_eq!(
            out,
            "https://grok.com/supergrok?heavy=1&referrer=distill"
        );
    }

    #[test]
    fn ensure_query_param_preserves_fragment() {
        // The current remote settings value uses a hash fragment for client-side routing (`grok.com/#supergrok`)
        // We still want the referrer attached
        let out = ensure_query_param("https://grok.com/#supergrok", "referrer", "distill");
        assert_eq!(out, "https://grok.com/?referrer=distill#supergrok");
    }

    #[test]
    fn ensure_query_param_returns_unchanged_on_parse_failure() {
        let out = ensure_query_param("not a url", "referrer", "distill");
        assert_eq!(out, "not a url");
    }

    #[test]
    fn ensure_query_param_url_encodes_value() {
        let out = ensure_query_param("https://grok.com/supergrok", "referrer", "grok build");
        assert_eq!(out, "https://grok.com/supergrok?referrer=grok+build");
    }

    #[test]
    fn fallback_scheme_case_insensitive() {
        // Uppercase scheme that url::Url::parse rejects triggers fallback path; the fallback must lowercase before matching SchemeFilter
        assert!(!is_safe_to_open(
            "CUSTOM://something",
            SchemeFilter::Standard
        ));
        // Ensure mailto fallback is case-insensitive too.
        assert!(is_safe_to_open(
            "MAILTO:user@example.com",
            SchemeFilter::Standard
        ));
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn browser_available_with_x11_display() {
        assert!(browser_open_likely_available_from_env(&env(&[(
            "DISPLAY", ":0"
        )])));
    }

    #[test]
    fn browser_available_with_wayland() {
        assert!(browser_open_likely_available_from_env(&env(&[(
            "WAYLAND_DISPLAY",
            "wayland-0"
        )])));
    }

    #[test]
    fn browser_available_with_browser_env_override() {
        // Headless boxes can still open via BROWSER=… even without DISPLAY.
        assert!(browser_open_likely_available_from_env(&env(&[(
            "BROWSER", "firefox"
        )])));
    }

    #[test]
    fn browser_unavailable_when_display_vars_empty_or_missing() {
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            // Desktop OSes do not gate on DISPLAY.
            assert!(browser_open_likely_available_from_env(&env(&[])));
            return;
        }
        assert!(!browser_open_likely_available_from_env(&env(&[])));
        assert!(!browser_open_likely_available_from_env(&env(&[
            ("DISPLAY", ""),
            ("WAYLAND_DISPLAY", ""),
            ("BROWSER", ""),
        ])));
    }

    #[test]
    fn browser_unavailable_message_includes_full_url() {
        let url = "https://grok.com/supergrok?referrer=distill";
        assert_eq!(
            browser_unavailable_message(url),
            format!("{BROWSER_UNAVAILABLE_NOTICE}:\n{url}")
        );
    }

    #[test]
    fn browser_unavailable_line_is_url_first_single_line() {
        let url = "https://grok.com/supergrok?referrer=distill";
        let plain = browser_unavailable_line(url, false);
        assert!(plain.starts_with(url), "{plain}");
        assert!(!plain.contains('\n'), "{plain}");
        assert!(
            !plain.to_ascii_lowercase().contains("copied"),
            "must not claim copy on failure: {plain}"
        );
        assert!(
            plain.contains(BROWSER_UNAVAILABLE_NOTICE),
            "shares notice stem with multi-line form: {plain}"
        );

        let with_copy = browser_unavailable_line(url, true);
        assert!(with_copy.starts_with(url), "{with_copy}");
        assert!(!with_copy.contains('\n'), "{with_copy}");
        assert!(
            with_copy.contains("URL copied"),
            "copy claim only when copied=true: {with_copy}"
        );
    }

    #[test]
    fn try_open_url_rejects_unsafe_scheme_without_opening() {
        assert_eq!(
            try_open_url("javascript:alert(1)", SchemeFilter::Standard),
            OpenUrlResult::RejectedScheme
        );
    }
}
