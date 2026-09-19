// Modified for Distill by Samuel Fajreldines, 2026.
//! Terminal inline image rendering (Kitty / iTerm2 protocols).
//!
//! Provides escape-sequence helpers for rendering images inside the existing preview overlay.
//! The text-fallback path in [`crate::render::image_overlay`] remains the primary preview.
//! This module adds pixel-level rendering for supported terminals.
//!
//! # Supported protocols
//!
//! - **Kitty graphics protocol**: used by Kitty, Ghostty, WezTerm, Warp
//! - **iTerm2 inline images**: helpers exist but are currently gated off in [`protocol_for_brand()`] (see there for why).
//!   The text fallback is used for iTerm2 instead.
//!
//! # Usage
//!
//! 1. Call [`detect_graphics_protocol()`] once (cached).
//! 2. During draw, if an image preview is active, call [`render_kitty_image()`] or [`render_iterm2_image()`] to build the escape sequence.
//! 3. Write the escape sequence to stderr **after** the ratatui cell flush but inside the synchronized-output block.
//! 4. Coordinate shared ID-1 ownership through [`super::overlay`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{TerminalName, terminal_context};

// -------------------------------------------------------------------------
// Graphics protocol detection
// -------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphicsProtocol {
    /// Kitty graphics protocol (also used by Ghostty, WezTerm).
    Kitty,
    /// iTerm2 inline images protocol.
    ITerm2,
    /// No graphics protocol available; the text fallback is used instead.
    #[default]
    None,
}

impl GraphicsProtocol {
    pub fn supports_images(self) -> bool {
        !matches!(self, Self::None)
    }
}

static GRAPHICS_PROTOCOL: OnceLock<GraphicsProtocol> = OnceLock::new();

/// Minimal mode never runs the draw loop, so overlays stay off and media uses the text affordance instead of blank image rows.
static INLINE_OVERLAY_FORCE_OFF: AtomicBool = AtomicBool::new(false);

/// Force scrollback inline-media overlays off (`off = true`) or restore the capability-based default (`off = false`) process-wide.
/// Called once at startup by the pager when minimal mode is active.
pub fn set_inline_overlay_force_off(off: bool) {
    INLINE_OVERLAY_FORCE_OFF.store(off, Ordering::Relaxed);
}

/// Whether scrollback inline-media overlays are currently forced off, i.e. the process is in minimal/scrollback-native mode.
/// That mode commits static text and never runs the interactive draw loop.
/// Also used to suppress affordances the draw loop paints (e.g. the mermaid button row) that would otherwise commit as blank rows.
pub fn scrollback_inline_overlay_forced_off() -> bool {
    INLINE_OVERLAY_FORCE_OFF.load(Ordering::Relaxed)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Per-test override so tests don't depend on the host terminal or the process-wide `GRAPHICS_PROTOCOL` cache.
    static TEST_PROTOCOL_OVERRIDE: std::cell::Cell<Option<GraphicsProtocol>> =
        const { std::cell::Cell::new(None) };
}

pub fn detect_graphics_protocol() -> GraphicsProtocol {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(p) = TEST_PROTOCOL_OVERRIDE.with(|c| c.get()) {
        return p;
    }
    *GRAPHICS_PROTOCOL.get_or_init(|| {
        let ctx = terminal_context();
        if ctx.graphics_protocol_skip_reason().is_some() {
            return GraphicsProtocol::None;
        }
        protocol_for_brand(ctx.brand, cfg!(target_os = "windows"))
    })
}

/// Narrower than Kitty graphics support. Warp accepts some escapes but not the placement/scrollback model, leaving stale pixels.
pub fn scrollback_inline_overlay_active() -> bool {
    // Minimal mode forces this off process-wide: it never paints inline images, so media must always use the text affordance
    if INLINE_OVERLAY_FORCE_OFF.load(Ordering::Relaxed) {
        return false;
    }
    let protocol = detect_graphics_protocol();
    if test_protocol_override_active() {
        return protocol == GraphicsProtocol::Kitty;
    }
    scrollback_inline_overlay_active_for_brand(protocol, terminal_context().brand)
}

#[cfg(any(test, feature = "test-support"))]
fn test_protocol_override_active() -> bool {
    TEST_PROTOCOL_OVERRIDE.with(|c| c.get().is_some())
}

#[cfg(not(any(test, feature = "test-support")))]
fn test_protocol_override_active() -> bool {
    false
}

/// Pure capability helper for scrollback inline-media overlays.
fn scrollback_inline_overlay_active_for_brand(
    protocol: GraphicsProtocol,
    brand: TerminalName,
) -> bool {
    matches!(
        (protocol, brand),
        (
            GraphicsProtocol::Kitty,
            TerminalName::Kitty | TerminalName::Ghostty | TerminalName::WezTerm
        )
    )
}

/// Set a per-thread protocol override for tests.
/// Returns a guard that clears it on drop.
#[cfg(any(test, feature = "test-support"))]
pub fn set_protocol_for_test(p: GraphicsProtocol) -> TestProtocolGuard {
    TEST_PROTOCOL_OVERRIDE.with(|c| c.set(Some(p)));
    TestProtocolGuard
}

/// RAII guard that clears the test protocol override on drop.
#[cfg(any(test, feature = "test-support"))]
pub struct TestProtocolGuard;

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestProtocolGuard {
    fn drop(&mut self) {
        TEST_PROTOCOL_OVERRIDE.with(|c| c.set(None));
    }
}

/// Returns `None` on Windows because ConPTY strips the Kitty/iTerm2 APC escape sequences before they reach the host terminal.
///
/// Parameterised by `is_windows` so unit tests can exercise both paths on any OS.
pub fn protocol_for_brand(brand: TerminalName, is_windows: bool) -> GraphicsProtocol {
    if is_windows {
        return GraphicsProtocol::None;
    }
    match brand {
        TerminalName::Kitty => GraphicsProtocol::Kitty,
        TerminalName::Ghostty => GraphicsProtocol::Kitty,
        TerminalName::WezTerm => GraphicsProtocol::Kitty,
        TerminalName::WarpTerminal => GraphicsProtocol::Kitty,
        // OSC 1337 lacks placement/clear primitives, so scrollback images do not track the grid. Preview opts in separately.
        TerminalName::Iterm2 => GraphicsProtocol::None,
        _ => GraphicsProtocol::None,
    }
}

static PROMPT_PREVIEW_PROTOCOL: OnceLock<GraphicsProtocol> = OnceLock::new();

/// iTerm2 is allowed only here: the box never sits on the bottom row, and closing it repaints every occupied cell so no clear escape is needed.
pub fn prompt_preview_graphics_protocol() -> GraphicsProtocol {
    let base = detect_graphics_protocol();
    if base != GraphicsProtocol::None || test_protocol_override_active() {
        return base;
    }
    *PROMPT_PREVIEW_PROTOCOL.get_or_init(|| {
        let ctx = terminal_context();
        if ctx.graphics_protocol_skip_reason().is_some() {
            return GraphicsProtocol::None;
        }
        prompt_preview_protocol_for_brand(
            ctx.brand,
            cfg!(target_os = "windows"),
            ctx.term_features.as_deref(),
            ctx.is_ssh,
        )
    })
}

/// iTerm2 needs `TERM_FEATURES` `F` or OSC 1337 leaks base64. That var does not cross SSH, so absence there is not a denial.
pub fn prompt_preview_protocol_for_brand(
    brand: TerminalName,
    is_windows: bool,
    term_features: Option<&str>,
    is_ssh: bool,
) -> GraphicsProtocol {
    if !is_windows && brand == TerminalName::Iterm2 {
        let display_allowed = match term_features {
            Some(features) => features.contains('F'),
            None => is_ssh,
        };
        if display_allowed {
            return GraphicsProtocol::ITerm2;
        }
    }
    protocol_for_brand(brand, is_windows)
}

// -------------------------------------------------------------------------
// Kitty graphics protocol
// -------------------------------------------------------------------------

/// Shared placement ID; every renderer must coordinate through [`super::overlay`].
pub(super) const KITTY_PLACEMENT_ID: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KittyImageFormat {
    /// PNG image data (`f=100`).
    Png,
}

impl KittyImageFormat {
    fn code(self) -> u16 {
        match self {
            Self::Png => 100,
        }
    }
}

pub fn kitty_format_from_bytes(image_data: &[u8]) -> Option<KittyImageFormat> {
    match distill_shared::clipboard::mime_from_bytes(image_data) {
        "image/png" => Some(KittyImageFormat::Png),
        _ => None,
    }
}

/// Whether Kitty can directly render this encoded MIME type in raw-byte mode.
pub fn kitty_mime_is_directly_supported(mime_type: &str) -> bool {
    mime_type == "image/png"
}

/// Kitty `f=100` accepts PNG only. Keep off the draw path. macOS uses `sips` so ICC profiles survive; elsewhere the `image` crate.
pub fn prepare_kitty_overlay_image_bytes(image_data: &[u8]) -> Option<Vec<u8>> {
    if kitty_format_from_bytes(image_data).is_some() {
        return Some(image_data.to_vec());
    }

    // On macOS, convert via `sips` through a temp file
    // CoreGraphics handles ICC colour profiles correctly, avoiding the artifacts that the `image` crate's JPEG-to-PNG path can produce
    if cfg!(target_os = "macos")
        && let Some(png) = convert_via_sips(image_data)
    {
        return Some(png);
    }

    let img = image::ImageReader::new(std::io::Cursor::new(image_data))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;

    let mut png = Vec::new();
    {
        use image::ExtendedColorType;
        use image::ImageEncoder;
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};

        let rgba = img.to_rgba8();
        let encoder =
            PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive);
        encoder
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                ExtendedColorType::Rgba8,
            )
            .ok()?;
    }
    Some(png)
}

/// Convert image bytes to PNG via macOS `sips` using temp files.
fn convert_via_sips(image_data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;

    let tmp_dir = std::env::temp_dir();
    let id = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let src = tmp_dir.join(format!("grok-sips-{id}-{ts}.dat"));
    let dst = tmp_dir.join(format!("grok-sips-{id}-{ts}.png"));

    // Write source bytes to temp file.
    let mut f = std::fs::File::create(&src).ok()?;
    f.write_all(image_data).ok()?;
    f.sync_all().ok()?;
    drop(f);

    let mut sips_cmd = std::process::Command::new("sips");
    sips_cmd
        .args(["-s", "format", "png"])
        .arg(&src)
        .arg("--out")
        .arg(&dst)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    distill_tty_utils::detach_std_command(&mut sips_cmd);
    let status = sips_cmd.status().ok()?;

    let _ = std::fs::remove_file(&src);

    if !status.success() || !dst.is_file() {
        let _ = std::fs::remove_file(&dst);
        return None;
    }

    let png = std::fs::read(&dst).ok()?;
    let _ = std::fs::remove_file(&dst);
    Some(png)
}

pub fn prepare_overlay_image_bytes(image_data: &[u8]) -> Option<Vec<u8>> {
    match detect_graphics_protocol() {
        GraphicsProtocol::Kitty => prepare_kitty_overlay_image_bytes(image_data),
        GraphicsProtocol::ITerm2 => Some(image_data.to_vec()),
        GraphicsProtocol::None => None,
    }
}

/// Terminal scales to the cell rect (it knows HiDPI). `C=1` preserves the cursor, `z=1` draws above text, `q=2` suppresses replies.
pub fn render_kitty_image(
    image_data: &[u8],
    format: KittyImageFormat,
    cols: u16,
    rows: u16,
) -> String {
    render_kitty_image_z(image_data, format, cols, rows, 1)
}

/// `z=1`: above text (modal overlays).
/// `z=-1`: below text, above background (inline scrollback media; dropdowns render on top).
pub fn render_kitty_image_z(
    image_data: &[u8],
    format: KittyImageFormat,
    cols: u16,
    rows: u16,
    z: i32,
) -> String {
    let header = format!(
        "a=T,f={},t=d,q=2,C=1,z={},i={},p={},c={},r={}",
        format.code(),
        z,
        KITTY_PLACEMENT_ID,
        KITTY_PLACEMENT_ID,
        cols,
        rows,
    );
    kitty_chunked_escape(image_data, &header)
}

/// Transmit image data to the terminal without displaying it (`a=t`).
/// Use `place_kitty_image` to display it at a position.
pub fn transmit_kitty_image(image_data: &[u8], format: KittyImageFormat, image_id: u32) -> String {
    let header = format!("a=t,f={},t=d,q=2,i={}", format.code(), image_id);
    kitty_chunked_escape(image_data, &header)
}

/// `first_chunk_header` is the metadata for the first chunk (action, format, etc.).
fn kitty_chunked_escape(image_data: &[u8], first_chunk_header: &str) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(image_data);

    let chunk_size = 4096;
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        let m = if is_last { 0 } else { 1 };
        if i == 0 {
            out.push_str(&format!("\x1b_G{first_chunk_header},m={m};{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gq=2,m={m};{chunk}\x1b\\"));
        }
    }
    out
}

/// Place an already-transmitted image at the cursor position (`a=p`).
///
/// Tiny escape (~50 bytes): no image data, just placement metadata.
pub fn place_kitty_image(image_id: u32, cols: u16, rows: u16, z: i32) -> String {
    format!(
        "\x1b_Ga=p,i={},p={},c={},r={},z={},C=1,q=2\x1b\\",
        image_id, image_id, cols, rows, z,
    )
}

/// Place an already-transmitted image with source cropping (`a=p`).
///
/// `src_x, src_y, src_w, src_h`: pixel region of the source image to display.
#[allow(clippy::too_many_arguments)]
pub fn place_kitty_image_cropped(
    image_id: u32,
    cols: u16,
    rows: u16,
    z: i32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
) -> String {
    format!(
        "\x1b_Ga=p,i={},p={},c={},r={},z={},x={},y={},w={},h={},C=1,q=2\x1b\\",
        image_id, image_id, cols, rows, z, src_x, src_y, src_w, src_h,
    )
}

/// Build a Kitty escape sequence to delete a specific image by ID.
pub fn clear_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={image_id},q=2\x1b\\")
}

// -------------------------------------------------------------------------
// iTerm2 inline images protocol
// -------------------------------------------------------------------------

/// `preserveAspectRatio=0`: callers already sized the rect. Re-preserving against real cell geometry letterboxes blank bands.
pub fn render_iterm2_image(image_data: &[u8], cols: u16, rows: u16) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(image_data);
    format!(
        "\x1b]1337;File=inline=1;width={cols}cells;height={rows}cells;preserveAspectRatio=0:{b64}\x07",
    )
}

// -------------------------------------------------------------------------
// Shared overlay helpers
// -------------------------------------------------------------------------

/// Delete-then-transmit: Warp ignores placement replace, so `a=p` would stack duplicates. Unchanged placements must not call this.
pub(super) fn build_overlay_image_escapes_for_protocol(
    protocol: GraphicsProtocol,
    image_data: &[u8],
    cols: u16,
    rows: u16,
    cell_x: u16,
    cell_y: u16,
) -> Option<String> {
    if protocol == GraphicsProtocol::None {
        return None;
    }

    let mut esc = String::new();
    // ANSI cursor positioning is 1-based.
    match protocol {
        GraphicsProtocol::Kitty => {
            let format = kitty_format_from_bytes(image_data)?;
            esc.push_str(&clear_kitty_image(KITTY_PLACEMENT_ID));
            esc.push_str(&format!("\x1b[{};{}H", cell_y + 1, cell_x + 1));
            esc.push_str(&render_kitty_image_z(
                image_data, format, cols, rows, 1, // above text (modal overlays)
            ));
        }
        GraphicsProtocol::ITerm2 => {
            // Unlike Kitty (`C=1`), OSC 1337 advances the cursor past the image, so save/restore it (DECSC/DECRC) around the write
            // These escapes are written post-flush, after ratatui parked the caret for the frame
            esc.push_str("\x1b7");
            esc.push_str(&format!("\x1b[{};{}H", cell_y + 1, cell_x + 1));
            esc.push_str(&render_iterm2_image(image_data, cols, rows));
            esc.push_str("\x1b8");
        }
        GraphicsProtocol::None => unreachable!(),
    }
    Some(esc)
}

/// Kitty: uploads with the given `image_id`.
/// iTerm2: no-op; the data is sent with each placement instead.
pub fn transmit_inline_image(image_data: &[u8], image_id: u32) -> Option<String> {
    match detect_graphics_protocol() {
        GraphicsProtocol::Kitty => {
            let format = kitty_format_from_bytes(image_data)?;
            Some(transmit_kitty_image(image_data, format, image_id))
        }
        GraphicsProtocol::ITerm2 => Some(String::new()),
        GraphicsProtocol::None => None,
    }
}

/// For Kitty: ~80 bytes (no image data, just placement with crop).
/// For iTerm2: sends full image data only when `emit_iterm_data` is true (no crop support).
/// Pass `false` after the first placement to avoid re-decoding the same image on every TUI frame.
#[allow(clippy::too_many_arguments)]
pub fn place_inline_image(
    image_data: &[u8],
    img_w: u32,
    img_h: u32,
    area: ratatui::layout::Rect,
    full_rows: u16,
    top_crop_rows: u16,
    image_id: u32,
    emit_iterm_data: bool,
) -> Option<String> {
    let protocol = detect_graphics_protocol();
    if protocol == GraphicsProtocol::None {
        return None;
    }

    // Compute fit dimensions as if the full image were visible.
    let (fit_cols, fit_rows) = fit_image_to_cells(img_w, img_h, area.width, full_rows);
    let pad_x = area.width.saturating_sub(fit_cols) / 2;
    let img_x = area.x + pad_x;
    let img_y = area.y;

    let mut esc = String::new();
    esc.push_str(&format!("\x1b[{};{}H", img_y + 1, img_x + 1));
    match protocol {
        GraphicsProtocol::Kitty => {
            let visible_rows = area.height.min(fit_rows);
            if top_crop_rows > 0 || visible_rows < fit_rows {
                let src_y = if fit_rows > 0 {
                    (top_crop_rows as u32 * img_h) / fit_rows as u32
                } else {
                    0
                };
                let src_h = if fit_rows > 0 {
                    (visible_rows as u32 * img_h) / fit_rows as u32
                } else {
                    img_h
                };
                esc.push_str(&place_kitty_image_cropped(
                    image_id,
                    fit_cols,
                    visible_rows,
                    -1,
                    0,
                    src_y,
                    img_w,
                    src_h.max(1),
                ));
            } else {
                esc.push_str(&place_kitty_image(image_id, fit_cols, fit_rows, -1));
            }
        }
        GraphicsProtocol::ITerm2 => {
            if emit_iterm_data {
                esc.push_str(&render_iterm2_image(image_data, fit_cols, area.height));
            }
        }
        GraphicsProtocol::None => unreachable!(),
    }
    Some(esc)
}

/// Fallback cell width/height ratio (typical monospace cell ~8×16 px), used when the terminal does not report its pixel size.
const DEFAULT_CELL_ASPECT: f64 = 0.5;

/// Protocols fill the cell rect, so an assumed 1:2 cell stretches other fonts. Implausible reports (tmux, Windows, non-tty) use the default.
/// Once per process: zoom scales both axes. Tests pin the fallback.
fn cell_aspect() -> f64 {
    #[cfg(any(test, feature = "test-support"))]
    {
        DEFAULT_CELL_ASPECT
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        static CELL_ASPECT: OnceLock<f64> = OnceLock::new();
        *CELL_ASPECT.get_or_init(|| {
            crossterm::terminal::window_size()
                .map(|ws| cell_aspect_from(&ws))
                .unwrap_or(DEFAULT_CELL_ASPECT)
        })
    }
}

/// Pure part of [`cell_aspect`]: the ratio math and the plausibility band.
// Dead only in the test-support feature build, where `cell_aspect` pins the fallback; production calls it and the unit tests exercise it directly
#[cfg_attr(feature = "test-support", allow(dead_code))]
fn cell_aspect_from(ws: &crossterm::terminal::WindowSize) -> f64 {
    if ws.columns == 0 || ws.rows == 0 || ws.width == 0 || ws.height == 0 {
        return DEFAULT_CELL_ASPECT;
    }
    let aspect =
        (f64::from(ws.width) / f64::from(ws.columns)) / (f64::from(ws.height) / f64::from(ws.rows));
    // Real monospace cells live in this band; anything outside means the report is bogus (e.g. display size instead of window size).
    if (0.3..=0.8).contains(&aspect) {
        aspect
    } else {
        DEFAULT_CELL_ASPECT
    }
}

/// Uses measured [`cell_aspect`] so a 1:1 image stays visually square; cells are not square.
pub fn fit_image_to_cells(img_w: u32, img_h: u32, max_cols: u16, max_rows: u16) -> (u16, u16) {
    if img_w == 0 || img_h == 0 || max_cols == 0 || max_rows == 0 {
        return (max_cols.max(1), max_rows.max(1));
    }

    let cell_aspect = cell_aspect();

    let img_aspect = img_w as f64 / img_h as f64;

    // Convert image aspect to cell-space: how many columns per row the image needs to look correct
    // A cell is `cell_aspect` times as wide as it is tall, so we divide by cell_aspect
    let cols_per_row = img_aspect / cell_aspect;

    // Try fitting by width first.
    let cols_by_width = max_cols;
    let rows_by_width = (cols_by_width as f64 / cols_per_row).round() as u16;

    // Try fitting by height.
    let rows_by_height = max_rows;
    let cols_by_height = (rows_by_height as f64 * cols_per_row).round() as u16;

    // Pick whichever fit stays within bounds.
    if rows_by_width <= max_rows {
        (cols_by_width, rows_by_width.max(1))
    } else {
        (cols_by_height.min(max_cols).max(1), rows_by_height)
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests;
