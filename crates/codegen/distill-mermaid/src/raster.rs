//! SVG to PNG rasterization via `resvg`/`usvg`/`tiny-skia`.
//!
//! Configured for untrusted input.
//! Shaping is pinned to the bundled font face; system fonts serve only as glyph fallback for non-ASCII text.
//! No external image resolvers: a crafted SVG cannot read `file://`, `http://`, or local paths.
//! The PNG is produced by tiny-skia's own encoder; the `image` crate's codecs are not used on the render path.

use std::sync::Arc;
use std::sync::OnceLock;

// `usvg` comes from `resvg`'s re-export so its version matches `resvg::render` exactly (the workspace `usvg` pin is an older, incompatible line)
// `tiny_skia` is the workspace dep, which resolves to the same version `resvg` links, so the types unify
use resvg::usvg;

use crate::{MermaidError, RenderParams, RenderedDiagram, Rgba};

/// Bundled primary sans face (Roboto Regular, Apache-2.0); system fonts are consulted only as glyph fallback for characters it lacks.
/// The vendored layout engine measures text with fixed char-width metrics (no font file), so there is no layout/raster font to keep in sync.
/// This face is used purely to rasterize glyphs.
pub(crate) const BUNDLED_FONT: &[u8] = include_bytes!("../assets/Roboto-Regular.ttf");

/// Hard ceiling on output area, applied regardless of requested size, to bound memory over huge untrusted diagrams (32 MP is about 5657x5657).
pub const MAX_OUTPUT_MEGAPIXELS: f32 = 32.0;

/// Hard ceiling on either output axis, so an extreme-aspect diagram can't pin one dimension to a huge value even when the area cap leaves headroom.
const MAX_OUTPUT_DIMENSION: u32 = 16_384;

struct FontSet {
    db: Arc<fontdb::Database>,
    family: String,
    bundled_id: fontdb::ID,
}

fn build_font_set(with_system_fonts: bool) -> FontSet {
    let mut db = fontdb::Database::new();
    db.load_font_data(BUNDLED_FONT.to_vec());
    let (bundled_id, family) = db
        .faces()
        .next()
        .map(|face| {
            (
                face.id,
                face.families
                    .first()
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| "sans-serif".to_string()),
            )
        })
        .expect("the bundled font must parse to at least one face");
    if with_system_fonts {
        db.load_system_fonts();
    }
    // The engine emits font-family lists ending in a generic.
    // None of the named families are loaded, so resolution falls to the generic
    db.set_serif_family(&family);
    db.set_sans_serif_family(&family);
    db.set_monospace_family(&family);
    db.set_cursive_family(&family);
    db.set_fantasy_family(&family);
    FontSet {
        db: Arc::new(db),
        family,
        bundled_id,
    }
}

/// Parse and index the bundled font once; the `Arc` is shared into every `Options` per render with an O(1) refcount bump (no per-render clone).
fn bundled_font() -> &'static FontSet {
    static FONT: OnceLock<FontSet> = OnceLock::new();
    FONT.get_or_init(|| build_font_set(false))
}

fn font_with_system_fallback() -> &'static FontSet {
    static FONT: OnceLock<FontSet> = OnceLock::new();
    FONT.get_or_init(|| build_font_set(true))
}

fn font_set_for(svg: &str) -> &'static FontSet {
    if svg.is_ascii() {
        bundled_font()
    } else {
        font_with_system_fallback()
    }
}

fn pinned_resolver(bundled_id: fontdb::ID) -> usvg::FontResolver<'static> {
    usvg::FontResolver {
        select_font: Box::new(move |_font, _db| Some(bundled_id)),
        select_fallback: usvg::FontResolver::default_fallback_selector(),
    }
}

fn rgba_to_color(c: Rgba) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// Rasterize `svg` to a PNG using `params`.
/// A [`RenderParams::background`] of `Some` fills the canvas opaquely; `None` leaves it transparent.
/// Returns [`MermaidError::Rasterize`] if the SVG cannot be parsed, has zero size, or cannot be encoded to PNG.
pub fn rasterize(svg: &str, params: &RenderParams) -> Result<RenderedDiagram, MermaidError> {
    rasterize_with_font(svg, params, font_set_for(svg))
}

fn rasterize_with_font(
    svg: &str,
    params: &RenderParams,
    font: &FontSet,
) -> Result<RenderedDiagram, MermaidError> {
    let mut opt = usvg::Options {
        fontdb: Arc::clone(&font.db),
        font_family: font.family.clone(),
        font_resolver: pinned_resolver(font.bundled_id),
        ..Default::default()
    };
    // SECURITY: usvg's default string resolver reads image hrefs off disk (`std::fs::read`)
    // Replace it with a no-op so a crafted SVG can never read local files or reach the network
    // In-memory data-URLs stay supported
    opt.image_href_resolver.resolve_string = Box::new(|_href, _opt| None);

    let tree =
        usvg::Tree::from_str(svg, &opt).map_err(|e| MermaidError::Rasterize(e.to_string()))?;

    let size = tree.size();
    let (base_w, base_h) = (size.width(), size.height());
    if base_w <= 0.0 || base_h <= 0.0 {
        return Err(MermaidError::Rasterize("diagram has zero size".to_string()));
    }

    let scale = effective_scale(base_w, base_h, params);
    let (width_px, height_px) = clamp_dimensions(base_w * scale, base_h * scale);

    let mut pixmap = tiny_skia::Pixmap::new(width_px, height_px).ok_or_else(|| {
        MermaidError::Rasterize(format!("invalid pixmap size {width_px}x{height_px}"))
    })?;
    if let Some(bg) = params.background {
        pixmap.fill(rgba_to_color(bg));
    }

    // Scale each axis to fill the chosen pixmap exactly
    // After the integer clamps the two axis scales can differ by up to ~1/base_dim, a sub-pixel aspect skew for typical sizes
    // We prefer an exact fill (no transparent margins) over perfect aspect preservation
    let transform =
        tiny_skia::Transform::from_scale(width_px as f32 / base_w, height_px as f32 / base_h);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let png = pixmap
        .encode_png()
        .map_err(|e| MermaidError::Rasterize(e.to_string()))?;

    tracing::debug!(target: "mermaid", width_px, height_px, png_bytes = png.len(), "rasterized svg");
    Ok(RenderedDiagram {
        png,
        width_px,
        height_px,
    })
}

/// Compute the scale factor applied to the SVG's intrinsic size.
/// Honors the target width (or fallback scale) and the optional minimum width, then clamps by max height and max output area.
fn effective_scale(base_w: f32, base_h: f32, params: &RenderParams) -> f32 {
    let mut scale = if params.target_width_px > 0 {
        params.target_width_px as f32 / base_w
    } else {
        params.scale
    };
    if !scale.is_finite() || scale <= 0.0 {
        scale = 1.0;
    }

    // Upscale small diagrams so OS viewers get a usable pixel budget
    // This runs before the height/area clamps, so a min-width request can still shrink to fit
    if params.min_width_px > 0 {
        let min_scale = params.min_width_px as f32 / base_w;
        if min_scale.is_finite() && min_scale > scale {
            scale = min_scale;
        }
    }

    if params.max_height_px > 0 {
        let max_h = params.max_height_px as f32;
        if base_h * scale > max_h {
            scale = max_h / base_h;
        }
    }

    let max_area = MAX_OUTPUT_MEGAPIXELS * 1_000_000.0;
    let area = (base_w * scale) * (base_h * scale);
    if area > max_area {
        scale *= (max_area / area).sqrt();
    }

    scale.max(f32::MIN_POSITIVE)
}

/// Convert floored float dimensions into the final integer pixmap size, enforcing the hard caps.
/// So after the floor and `max(1)`, cap each axis at [`MAX_OUTPUT_DIMENSION`] and shrink the larger axis until `width * height` fits the area cap.
fn clamp_dimensions(width_f: f32, height_f: f32) -> (u32, u32) {
    let mut width_px = (width_f.floor() as u32).clamp(1, MAX_OUTPUT_DIMENSION);
    let mut height_px = (height_f.floor() as u32).clamp(1, MAX_OUTPUT_DIMENSION);

    let max_area = (MAX_OUTPUT_MEGAPIXELS * 1_000_000.0) as u64;
    if width_px as u64 * height_px as u64 > max_area {
        if width_px >= height_px {
            width_px = ((max_area / height_px as u64) as u32).max(1);
        } else {
            height_px = ((max_area / width_px as u64) as u32).max(1);
        }
    }
    (width_px, height_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MermaidTheme;

    // A 100x50 SVG with a small 10x10 blue square in the top-left; the rest of the canvas is empty (so the background shows through there)
    const SVG_100X50: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50" viewBox="0 0 100 50"><rect x="0" y="0" width="10" height="10" fill="#0000ff"/></svg>"##;

    fn params(target_width_px: u32, max_height_px: u32) -> RenderParams {
        RenderParams {
            theme: MermaidTheme::Light,
            target_width_px,
            max_height_px,
            scale: 1.0,
            min_width_px: 0,
            background: None,
        }
    }

    #[test]
    fn min_width_raises_scale_before_clamps() {
        // A 100-wide SVG at scale 1.0 would be 100px; min_width 400 forces 4x, so 400x200
        let mut p = params(0, 10_000);
        p.min_width_px = 400;
        let out = rasterize(SVG_100X50, &p).expect("rasterize");
        assert_eq!(out.width_px, 400);
        assert_eq!(out.height_px, 200);
    }

    #[test]
    fn for_os_viewer_uses_2x_or_min_width() {
        // Small SVG: min_width 2560 wins over 2x (which would give 200)
        let p = RenderParams::for_os_viewer(MermaidTheme::Light, 2560, 8192);
        let out = rasterize(SVG_100X50, &p).expect("rasterize");
        assert_eq!(out.width_px, 2560);
        assert_eq!(out.height_px, 1280);

        // Wide SVG: 2x intrinsic (target_width 0, scale 2) when already at least min_width
        let wide = r##"<svg xmlns="http://www.w3.org/2000/svg" width="2000" height="500" viewBox="0 0 2000 500"><rect width="2000" height="500" fill="#00ff00"/></svg>"##;
        let out2 = rasterize(wide, &p).expect("rasterize");
        assert_eq!(out2.width_px, 4000);
        assert_eq!(out2.height_px, 1000);
    }

    #[test]
    fn rasterize_decodes_to_expected_scaled_dimensions() {
        // Target width 200 against a 100-wide SVG doubles it to 200x100
        let out = rasterize(SVG_100X50, &params(200, 10_000)).expect("rasterize");
        assert_eq!(out.width_px, 200);
        assert_eq!(out.height_px, 100);
        let img = image::load_from_memory(&out.png).expect("decode png");
        assert_eq!(img.width(), 200);
        assert_eq!(img.height(), 100);
    }

    #[test]
    fn rasterize_fallback_scale_when_no_target_width() {
        // target_width_px == 0 falls back to `scale`.
        let mut p = params(0, 10_000);
        p.scale = 3.0;
        let out = rasterize(SVG_100X50, &p).expect("rasterize");
        assert_eq!(out.width_px, 300);
        assert_eq!(out.height_px, 150);
    }

    #[test]
    fn rasterize_is_deterministic() {
        let a = rasterize(SVG_100X50, &params(200, 10_000)).expect("a");
        let b = rasterize(SVG_100X50, &params(200, 10_000)).expect("b");
        assert_eq!(
            a.png, b.png,
            "same svg+params must yield identical png bytes"
        );
    }

    #[test]
    fn opaque_background_fills_empty_region() {
        let mut p = params(0, 10_000); // 1x gives 100x50 so pixel coords are exact
        p.background = Some(Rgba::new(255, 0, 0, 255));
        let out = rasterize(SVG_100X50, &p).expect("rasterize");
        let img = image::load_from_memory(&out.png)
            .expect("decode")
            .to_rgba8();
        // Bottom-right is outside the blue square, so it shows the red background, fully opaque
        let px = img.get_pixel(95, 45);
        assert_eq!(px.0, [255, 0, 0, 255]);
    }

    #[test]
    fn no_background_leaves_empty_region_transparent() {
        let out = rasterize(SVG_100X50, &params(0, 10_000)).expect("rasterize");
        let img = image::load_from_memory(&out.png)
            .expect("decode")
            .to_rgba8();
        let px = img.get_pixel(95, 45);
        assert_eq!(px.0[3], 0, "empty region must be transparent without a bg");
    }

    #[test]
    fn invalid_svg_is_rasterize_error() {
        let err = rasterize("this is definitely not svg", &params(200, 10_000))
            .expect_err("garbage must not parse");
        assert!(matches!(err, MermaidError::Rasterize(_)));
    }

    #[test]
    fn max_height_clamps_tall_diagram() {
        let tall = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="1000" viewBox="0 0 100 1000"><rect width="100" height="1000" fill="#00ff00"/></svg>"##;
        // scale fallback 2.0 would give height 2000; clamp to 200.
        let mut p = params(0, 200);
        p.scale = 2.0;
        let out = rasterize(tall, &p).expect("rasterize");
        assert!(out.height_px <= 200, "height {} exceeds cap", out.height_px);
        // Aspect ratio preserved: 100/1000 means width is about height/10
        assert!(out.width_px <= 40);
    }

    #[test]
    fn megapixel_cap_bounds_huge_request() {
        let big = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="1000" viewBox="0 0 1000 1000"><rect width="1000" height="1000" fill="#123456"/></svg>"##;
        // Absurd target width; without the area cap this would be ~1e10 px.
        let out = rasterize(big, &params(200_000, u32::MAX)).expect("rasterize");
        let area = out.width_px as u64 * out.height_px as u64;
        assert!(
            area <= (MAX_OUTPUT_MEGAPIXELS as u64) * 1_000_000,
            "area {area} exceeds megapixel cap"
        );
        assert!(image::load_from_memory(&out.png).is_ok());
    }

    #[test]
    fn extreme_aspect_svg_respects_area_and_axis_caps() {
        // A pathological 3.2e9 x 1 SVG: the float area cap leaves one axis huge
        // The floor and `max(1)` on the sub-1px axis would otherwise inflate the integer area far past the cap
        // Verify both the area cap and the per-axis cap hold
        let wide = r##"<svg xmlns="http://www.w3.org/2000/svg" width="3200000000" height="1" viewBox="0 0 3200000000 1"><rect width="3200000000" height="1" fill="#abcdef"/></svg>"##;
        let out = rasterize(wide, &params(0, u32::MAX)).expect("rasterize");
        let area = out.width_px as u64 * out.height_px as u64;
        assert!(
            area <= (MAX_OUTPUT_MEGAPIXELS as u64) * 1_000_000,
            "area {area} exceeds megapixel cap"
        );
        assert!(
            out.width_px <= MAX_OUTPUT_DIMENSION && out.height_px <= MAX_OUTPUT_DIMENSION,
            "axis exceeds per-dimension cap: {}x{}",
            out.width_px,
            out.height_px
        );
        assert!(out.width_px >= 1 && out.height_px >= 1);
        assert!(image::load_from_memory(&out.png).is_ok());
    }

    #[test]
    fn clamp_dimensions_shrinks_larger_axis_to_area_cap() {
        // Direct coverage of the area-shrink branch, which is unreachable via `rasterize` because `effective_scale` pre-caps the float area
        // Exercises both arms of the inner `if`
        let max_area = (MAX_OUTPUT_MEGAPIXELS as u64) * 1_000_000;

        // Both axes hit the per-axis cap; since they are equal, the `width >= height` arm shrinks width
        let (w, h) = clamp_dimensions(20_000.0, 20_000.0);
        assert_eq!((w, h), (1953, MAX_OUTPUT_DIMENSION));
        assert!(w as u64 * h as u64 <= max_area);

        // Width below the per-axis cap and height at it, so the `else` arm shrinks height
        let (w2, h2) = clamp_dimensions(10_000.0, 20_000.0);
        assert_eq!((w2, h2), (10_000, 3200));
        assert!(w2 as u64 * h2 as u64 <= max_area);
    }

    #[test]
    fn narrow_tall_svg_clamps_width_to_one() {
        // Width 1, height 1000, clamped to height 5: width 1*0.005 floors to 0 and is bumped to 1 (the `.max(1)` boundary)
        let narrow = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1000" viewBox="0 0 1 1000"><rect width="1" height="1000" fill="#00ff00"/></svg>"##;
        let out = rasterize(narrow, &params(0, 5)).expect("rasterize");
        assert_eq!(out.width_px, 1, "narrow width must clamp to 1");
        assert!(out.height_px <= 5);
        assert!(image::load_from_memory(&out.png).is_ok());
    }

    #[test]
    fn zero_size_svg_is_rasterize_error() {
        // A zero-dimension SVG either fails to parse or hits the zero-size guard; both return Rasterize, never a panic or a 0x0 pixmap
        let zero = r##"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" viewBox="0 0 0 0"></svg>"##;
        let r = rasterize(zero, &params(200, 10_000));
        assert!(matches!(r, Err(MermaidError::Rasterize(_))), "got {r:?}");
    }

    #[test]
    fn non_finite_or_non_positive_scale_clamps_to_valid_png() {
        for bad_scale in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            let mut p = params(0, 10_000); // target_width_px == 0 takes the scale path
            p.scale = bad_scale;
            let out = rasterize(SVG_100X50, &p)
                .unwrap_or_else(|e| panic!("scale {bad_scale} should clamp to 1.0, got {e:?}"));
            assert_eq!(
                (out.width_px, out.height_px),
                (100, 50),
                "scale {bad_scale} should clamp to 1.0"
            );
            assert!(image::load_from_memory(&out.png).is_ok());
        }
    }

    #[test]
    fn malformed_svgs_never_panic_and_only_rasterize_error() {
        // No engine is involved; this checks rasterize alone against untrusted input
        // For malformed or partial SVG it must always return (no panic) and only ever produce Rasterize errors
        let inputs = [
            "",
            "<svg",
            "<svg></svg>",
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="abc" height="xyz"></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1e999" height="1"></svg>"#,
            "<><><>",
            "\u{0}\u{1}\u{2}\u{3}",
        ];
        for input in inputs {
            let r = rasterize(input, &params(200, 10_000));
            assert!(
                matches!(r, Ok(_) | Err(MermaidError::Rasterize(_))),
                "input {input:?} produced unexpected {r:?}"
            );
        }
        // A clearly-wrong root element must error, not silently succeed.
        assert!(matches!(
            rasterize("<not-svg/>", &params(200, 10_000)),
            Err(MermaidError::Rasterize(_))
        ));
    }

    #[test]
    fn text_with_engine_font_family_actually_renders_glyphs() {
        // Regression: the engine themes set font-family lists like "Inter, ..., sans-serif", none of which name the bundled Roboto face usvg resolves the generic `sans-serif` via fontdb's generic-family map (default "Arial"), which isn't loaded
        // Unless the generics point at the bundled face, the glyphs are silently dropped and node labels render blank
        // Black text on white: assert dark pixels exist
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="60" viewBox="0 0 200 60"><text x="10" y="38" font-family="Inter, ui-sans-serif, system-ui, -apple-system, &quot;Segoe UI&quot;, sans-serif" font-size="28" fill="#000000">Hello</text></svg>"##;
        let mut p = params(0, 10_000); // scale 1.0 => 200x60
        p.background = Some(Rgba::new(255, 255, 255, 255));
        let out = rasterize(svg, &p).expect("rasterize");
        let img = image::load_from_memory(&out.png)
            .expect("decode")
            .to_rgba8();
        let dark = img
            .pixels()
            .filter(|px| px.0[0] < 64 && px.0[1] < 64 && px.0[2] < 64)
            .count();
        assert!(
            dark > 50,
            "node-label glyphs were dropped (found {dark} dark px): the bundled \
             font is not wired to usvg's generic families",
        );
    }

    #[test]
    fn bundled_font_family_is_resolved() {
        // The bundled face exposes a usable family name (not the generic fallback), which the SVG font-family resolves against
        assert_ne!(bundled_font().family, "sans-serif");
        assert!(!bundled_font().family.is_empty());
    }

    #[test]
    fn ascii_svg_uses_bundled_only_database() {
        let set = font_set_for(SVG_100X50);
        assert!(std::ptr::eq(set, bundled_font()));
        assert_eq!(set.db.faces().count(), 1);
        assert_eq!(set.db.faces().next().map(|f| f.id), Some(set.bundled_id));
    }

    #[test]
    fn non_ascii_svg_uses_system_fallback_database() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><text x="5" y="30" font-size="20">中</text></svg>"##;
        let set = font_set_for(svg);
        assert!(std::ptr::eq(set, font_with_system_fallback()));
        assert_eq!(set.db.faces().next().map(|f| f.id), Some(set.bundled_id));
        assert_eq!(set.family, bundled_font().family);
    }

    #[test]
    fn named_system_family_cannot_hijack_shaping() {
        let named = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="60" viewBox="0 0 200 60"><text x="10" y="38" font-family="Arial, Helvetica, Verdana" font-size="28" fill="#000000">Hello é</text></svg>"##;
        let mut p = params(0, 10_000);
        p.background = Some(Rgba::new(255, 255, 255, 255));
        let via_fallback_db =
            rasterize_with_font(named, &p, font_with_system_fallback()).expect("system-db render");
        let via_bundled_db =
            rasterize_with_font(named, &p, bundled_font()).expect("bundled render");
        assert_eq!(
            via_fallback_db.png, via_bundled_db.png,
            "Latin text must shape identically with and without system fonts loaded"
        );
    }
}
