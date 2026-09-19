//! OSC 11 terminal background detection.
//!
//! Queries the terminal's background color via the OSC 11 escape sequence:
//!   Query:  `\x1b]11;?\x07`
//!   Reply:  `\x1b]11;rgb:RRRR/GGGG/BBBB\x07`  (or ST terminator `\x1b\\`)
//!
//! tmux 3.2 and later intercepts a *bare* OSC 11 query from a pane and answers it (3.4 also snapshots the outer terminal colour on attach).
//! Always send the bare query first.
//! DCS wrapping is a short fallback only.
//! `allow-passthrough` is off by default, and even when on the outer reply lands on tmux's tty rather than the pane.
//! Do not wrap inside an editor `:terminal`: libvterm would paint the envelope as garbage.
//!
//! This is a **startup-only** fallback: it must NOT be called once crossterm's `EventStream` is active, as both compete for stdin in raw mode.
//! The live `SystemAppearanceWatcher` uses only desktop and env detection.

use super::system_appearance::SystemAppearance;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::io::RawFd;

/// Luminance threshold: backgrounds with Y < 0.5 are considered dark.
const LUMINANCE_THRESHOLD: f64 = 0.5;

const OSC11_TIMEOUT: Duration = Duration::from_millis(500);

/// The wrap is usually swallowed, so it must not add another full [`OSC11_TIMEOUT`] to startup.
const OSC11_WRAP_TIMEOUT: Duration = Duration::from_millis(80);

const OSC11_QUERY: &[u8] = b"\x1b]11;?\x07";

/// `None` if stdin is not a TTY, the terminal misses `OSC11_TIMEOUT`, or the reply cannot be parsed.
/// Must run before the event stream. Termios is local; the query takes the shared stderr lock so it cannot interleave with the writer.
pub fn detect_via_osc11() -> Option<SystemAppearance> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return None;
    }

    // Bare first: tmux answers OSC 11 from the pane
    // Wrapping first is a regression when passthrough is off (default) or the reply never re-enters the pane
    if let Some(appearance) = query_osc11(OSC11_QUERY, OSC11_TIMEOUT) {
        return Some(appearance);
    }

    if crate::terminal::should_wrap_osc11(crate::terminal::terminal_context()) {
        let wrapped = crate::terminal::tmux_passthrough(OSC11_QUERY);
        query_osc11(&wrapped, OSC11_WRAP_TIMEOUT)
    } else {
        None
    }
}

fn query_osc11(query: &[u8], timeout: Duration) -> Option<SystemAppearance> {
    if !crate::terminal::probe::write_query(query) {
        return None;
    }
    let response = read_osc_response(timeout)?;
    let (r, g, b) = parse_osc11_rgb(&response)?;
    Some(classify_luminance(r, g, b))
}

/// Uses ITU-R BT.709 luminance coefficients with sRGB gamma correction.
pub(crate) fn classify_luminance(r: u8, g: u8, b: u8) -> SystemAppearance {
    let luminance =
        0.2126 * srgb_to_linear(r) + 0.7152 * srgb_to_linear(g) + 0.0722 * srgb_to_linear(b);

    if luminance < LUMINANCE_THRESHOLD {
        SystemAppearance::Dark
    } else {
        SystemAppearance::Light
    }
}

/// Handles both 4-digit (`rgb:RRRR/GGGG/BBBB`) and 2-digit (`rgb:RR/GG/BB`) hex formats.
pub(crate) fn parse_osc11_rgb(response: &str) -> Option<(u8, u8, u8)> {
    let rgb_start = response.find("rgb:")? + 4;
    let rgb_part = response.get(rgb_start..)?;

    // Split on channel separator `/` and terminators (BEL, ESC).
    let parts: Vec<&str> = rgb_part.split(['/', '\x07', '\x1b']).take(3).collect();
    let [r, g, b] = parts.as_slice() else {
        return None;
    };

    Some((parse_channel(r)?, parse_channel(g)?, parse_channel(b)?))
}

/// For 3- or 4-digit values, extracts the high byte (`>> 8`) to map to 0-255.
/// For 1- or 2-digit values, uses the value directly as 0-255.
fn parse_channel(s: &str) -> Option<u8> {
    let trimmed = s.trim();
    let val = u16::from_str_radix(trimmed, 16).ok()?;
    Some(if trimmed.len() > 2 {
        (val >> 8) as u8
    } else {
        val as u8
    })
}

/// Applies the sRGB transfer function inverse (IEC 61966-2-1).
fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Restores the original termios on drop without touching crossterm's process-wide `TERMINAL_MODE_PRIOR_RAW_MODE`.
/// Calling `crossterm::disable_raw_mode` here would restore the shell's pre-pager cooked termios, breaking the pager's own raw mode.
#[cfg(unix)]
struct TermiosGuard {
    fd: RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl Drop for TermiosGuard {
    fn drop(&mut self) {
        // SAFETY: fd was valid at construction; original was populated
        // by a successful tcgetattr.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// POSIX-portable subset of `cfmakeraw(3)`.
/// Clears the lflags that would block a single-byte read (canonical mode, echo, signal interpretation, extended processing).
#[cfg(unix)]
fn make_raw_termios(snapshot: &libc::termios) -> libc::termios {
    let mut raw = *snapshot;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    raw
}

#[cfg(unix)]
fn read_osc_response(timeout: Duration) -> Option<String> {
    use std::os::unix::io::AsRawFd;
    read_osc_response_with_fd(std::io::stdin().as_raw_fd(), timeout)
}

#[cfg(not(unix))]
fn read_osc_response(_timeout: Duration) -> Option<String> {
    None
}

/// The `fd` parameter exists so tests can pass `/dev/null` to exercise the non-TTY path.
/// The guard is constructed before `tcsetattr` to keep the restore atomic with the switch.
/// POSIX guarantees `tcsetattr` is atomic on failure, so a redundant restore on the early-return path is harmless.
#[cfg(unix)]
fn read_osc_response_with_fd(fd: RawFd, timeout: Duration) -> Option<String> {
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: caller passes a valid fd; original is a valid owned buffer.
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return None;
    }
    let raw = make_raw_termios(&original);
    let _guard = TermiosGuard { fd, original };
    // SAFETY: raw is a valid owned buffer.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    read_with_timeout(timeout)
}

/// Read bytes from stdin until a terminator is found or timeout expires.
// Only invoked from `read_osc_response_with_fd`, which is Unix-only.
#[cfg(unix)]
fn read_with_timeout(timeout: Duration) -> Option<String> {
    unix_read_with_timeout(timeout)
}

/// Unix implementation: the shared probe read loop with the OSC terminators (BEL, or ST as `ESC \`) as the stop predicate.
#[cfg(unix)]
fn unix_read_with_timeout(timeout: Duration) -> Option<String> {
    let buf = crate::terminal::probe::read_tty_reply(timeout, |buf, byte| {
        byte == 0x07 || (buf.len() >= 2 && buf.get(buf.len() - 2) == Some(&0x1b) && byte == 0x5c)
    })?;
    // Reject partial buffers: a reply truncated mid-channel would mis-parse, since channel width is inferred from digit count
    if !ends_with_osc_terminator(&buf) {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// True when the buffer ends with BEL or ST (`ESC \`).
#[cfg(any(unix, test))]
fn ends_with_osc_terminator(buf: &[u8]) -> bool {
    buf.last() == Some(&0x07) || buf.ends_with(b"\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ends_with_osc_terminator ---------------------------------------------

    #[test]
    fn unterminated_reply_is_rejected() {
        // A truncated channel would mis-parse and could flip dark/light.
        assert!(!ends_with_osc_terminator(b"\x1b]11;rgb:ffff/ffff/00"));
        assert!(ends_with_osc_terminator(b"\x1b]11;rgb:ffff/ffff/ffff\x07"));
        assert!(ends_with_osc_terminator(
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"
        ));
        assert!(!ends_with_osc_terminator(b""));
    }

    // -- parse_osc11_rgb -----------------------------------------------------

    #[test]
    fn parse_4digit_white() {
        // xterm-style: rgb:ffff/ffff/ffff
        let response = "\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_rgb(response), Some((255, 255, 255)));
    }

    #[test]
    fn parse_4digit_black() {
        let response = "\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(parse_osc11_rgb(response), Some((0, 0, 0)));
    }

    #[test]
    fn parse_2digit_dark() {
        // Some terminals use 2-digit hex: rgb:1a/1b/26
        let response = "\x1b]11;rgb:1a/1b/26\x07";
        assert_eq!(parse_osc11_rgb(response), Some((0x1a, 0x1b, 0x26)));
    }

    #[test]
    fn parse_2digit_light() {
        let response = "\x1b]11;rgb:f0/f0/f0\x07";
        assert_eq!(parse_osc11_rgb(response), Some((0xf0, 0xf0, 0xf0)));
    }

    #[test]
    fn parse_4digit_midrange() {
        let response = "\x1b]11;rgb:8080/8080/8080\x07";
        assert_eq!(parse_osc11_rgb(response), Some((128, 128, 128)));
    }

    #[test]
    fn parse_st_terminator() {
        // Some terminals use ESC \ (ST) instead of BEL as terminator.
        let response = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        assert_eq!(parse_osc11_rgb(response), Some((255, 255, 255)));
    }

    #[test]
    fn parse_missing_rgb_prefix() {
        let response = "\x1b]11;color:ffff/ffff/ffff\x07";
        assert!(parse_osc11_rgb(response).is_none());
    }

    #[test]
    fn parse_too_few_channels() {
        let response = "\x1b]11;rgb:ffff/ffff\x07";
        assert!(parse_osc11_rgb(response).is_none());
    }

    #[test]
    fn parse_empty_response() {
        assert!(parse_osc11_rgb("").is_none());
    }

    #[test]
    fn parse_invalid_hex() {
        let response = "\x1b]11;rgb:gggg/hhhh/iiii\x07";
        assert!(parse_osc11_rgb(response).is_none());
    }

    #[test]
    fn parse_1digit_channel() {
        // Edge case: single digit per channel (treated as 2-digit path).
        let response = "\x1b]11;rgb:f/f/f\x07";
        assert_eq!(parse_osc11_rgb(response), Some((15, 15, 15)));
    }

    #[test]
    fn parse_3digit_channel() {
        // 3-digit hex is uncommon but possible; with more than 2 digits the high byte is extracted
        let response = "\x1b]11;rgb:fff/fff/fff\x07";
        assert_eq!(parse_osc11_rgb(response), Some((15, 15, 15)));
    }

    // -- parse_channel -------------------------------------------------------

    #[test]
    fn channel_4digit_max() {
        assert_eq!(parse_channel("ffff"), Some(255));
    }

    #[test]
    fn channel_4digit_zero() {
        assert_eq!(parse_channel("0000"), Some(0));
    }

    #[test]
    fn channel_2digit_max() {
        assert_eq!(parse_channel("ff"), Some(255));
    }

    #[test]
    fn channel_2digit_zero() {
        assert_eq!(parse_channel("00"), Some(0));
    }

    #[test]
    fn channel_with_whitespace() {
        assert_eq!(parse_channel(" ff "), Some(255));
    }

    // -- classify_luminance --------------------------------------------------

    #[test]
    fn classify_pure_black_is_dark() {
        assert_eq!(classify_luminance(0, 0, 0), SystemAppearance::Dark);
    }

    #[test]
    fn classify_pure_white_is_light() {
        assert_eq!(classify_luminance(255, 255, 255), SystemAppearance::Light);
    }

    #[test]
    fn classify_dark_gray_is_dark() {
        // Typical dark terminal background: #1a1b26 (TokyoNight)
        assert_eq!(classify_luminance(0x1a, 0x1b, 0x26), SystemAppearance::Dark);
    }

    #[test]
    fn classify_light_gray_is_light() {
        // Typical light terminal background: #f0f0f0
        assert_eq!(
            classify_luminance(0xf0, 0xf0, 0xf0),
            SystemAppearance::Light
        );
    }

    #[test]
    fn classify_mid_gray_boundary() {
        // sRGB (186, 186, 186) has luminance about 0.497, just below the 0.5 threshold
        // sRGB (188, 188, 188) has luminance about 0.508, just above it
        assert_eq!(classify_luminance(186, 186, 186), SystemAppearance::Dark);
        assert_eq!(classify_luminance(188, 188, 188), SystemAppearance::Light);
    }

    #[test]
    fn classify_solarized_dark_is_dark() {
        // Solarized Dark base03: #002b36
        assert_eq!(classify_luminance(0x00, 0x2b, 0x36), SystemAppearance::Dark);
    }

    #[test]
    fn classify_solarized_light_is_light() {
        // Solarized Light base3: #fdf6e3
        assert_eq!(
            classify_luminance(0xfd, 0xf6, 0xe3),
            SystemAppearance::Light
        );
    }

    // -- srgb_to_linear ------------------------------------------------------

    #[test]
    fn srgb_to_linear_zero() {
        assert!((srgb_to_linear(0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn srgb_to_linear_max() {
        assert!((srgb_to_linear(255) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn srgb_to_linear_low_value() {
        // 10/255 is about 0.0392, below the 0.04045 cutoff, so it takes the linear branch
        let result = srgb_to_linear(10);
        let expected = (10.0 / 255.0) / 12.92;
        assert!((result - expected).abs() < 1e-10);
    }

    #[test]
    fn srgb_to_linear_high_value() {
        // 128/255 is about 0.502, above the 0.04045 cutoff, so it takes the gamma branch
        let result = srgb_to_linear(128);
        let s: f64 = 128.0 / 255.0;
        let expected = ((s + 0.055) / 1.055).powf(2.4);
        assert!((result - expected).abs() < 1e-10);
    }

    #[test]
    fn wrap_retry_timeout_is_shorter_than_primary() {
        assert!(OSC11_WRAP_TIMEOUT < OSC11_TIMEOUT);
    }

    #[test]
    fn wrapped_osc11_query_matches_tmux_passthrough() {
        assert_eq!(
            crate::terminal::tmux_passthrough(OSC11_QUERY),
            b"\x1bPtmux;\x1b\x1b]11;?\x07\x1b\\"
        );
    }
    #[cfg(unix)]
    #[test]
    fn read_osc_response_with_fd_returns_none_for_non_tty_fd() {
        // tcgetattr on /dev/null returns ENOTTY; we must bail without panicking and without touching crossterm's process-wide state
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open("/dev/null").unwrap();
        let result = read_osc_response_with_fd(f.as_raw_fd(), Duration::from_millis(10));
        assert_eq!(result, None);
    }

    #[cfg(unix)]
    #[test]
    fn make_raw_termios_clears_only_canonical_echo_signal_extended() {
        // Pre-populate with cleared bits AND preserved bits, then assert the result is exactly the preserved set
        // Catches regressions that widen the mask
        let mut snapshot: libc::termios = unsafe { std::mem::zeroed() };
        snapshot.c_lflag =
            libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN | libc::TOSTOP | libc::NOFLSH;
        let raw = make_raw_termios(&snapshot);
        assert_eq!(raw.c_lflag, libc::TOSTOP | libc::NOFLSH);
    }

    #[cfg(unix)]
    #[test]
    fn make_raw_termios_preserves_other_flag_words() {
        let mut snapshot: libc::termios = unsafe { std::mem::zeroed() };
        snapshot.c_lflag = libc::TOSTOP | libc::ICANON;
        snapshot.c_iflag = libc::ICRNL;
        snapshot.c_oflag = libc::OPOST;
        snapshot.c_cflag = libc::CS8;
        let raw = make_raw_termios(&snapshot);
        assert_eq!(raw.c_lflag & libc::TOSTOP, libc::TOSTOP);
        assert_eq!(raw.c_iflag, snapshot.c_iflag);
        assert_eq!(raw.c_oflag, snapshot.c_oflag);
        assert_eq!(raw.c_cflag, snapshot.c_cflag);
    }
}
