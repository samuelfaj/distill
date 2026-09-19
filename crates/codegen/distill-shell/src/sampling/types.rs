// Modified for Distill by Samuel Fajreldines, 2026.
// This re-export keeps all existing `crate::sampling::types::*` imports working
pub use distill_sampling_types::types::*;

// `CreateResponseWrapper` and `MessagesRequestWrapper` live in `distill-sampling-types::types`, re-exported above via the wildcard
// That placement lets the `distill-sampler` crate reference them without a circular dep on `distill-shell`

// Tests for the types live in the distill-sampling-types crate

use distill_tools::types::output::ImageContent as ToolsImageContent;

/// Which flow is applying a reasoning-effort hint, for logging and gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum EffortTarget {
    NewSession,
    ModelSwitch,
    #[strum(serialize = "summary")]
    SummaryClient,
}

/// Render an `ImageContent` produced by the read-file tool as a URL string suitable for an `image_url` content block.
/// Passes the explicit `uri` through if present, otherwise builds a `data:<mime>;base64,<data>` URI.
/// Lives in the shell (rather than `distill-sampling-types` or `distill-tools`) so neither crate needs a dep on `agent-client-protocol`.
pub fn get_image_content_url(image_content: &ToolsImageContent) -> String {
    if let Some(uri) = &image_content.uri {
        uri.clone()
    } else {
        format!(
            "data:{};base64,{}",
            image_content.mime_type, image_content.data
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_image_content_url_returns_uri_when_present() {
        let img = ToolsImageContent {
            data: "AAA".to_string(),
            mime_type: "image/png".to_string(),
            uri: Some("https://example.com/image.png".to_string()),
            annotations: None,
            meta: None,
        };
        assert_eq!(get_image_content_url(&img), "https://example.com/image.png");
    }

    #[test]
    fn get_image_content_url_builds_data_uri_when_no_uri() {
        let img = ToolsImageContent {
            data: "VGVzdA==".to_string(),
            mime_type: "image/png".to_string(),
            uri: None,
            annotations: None,
            meta: None,
        };
        assert_eq!(
            get_image_content_url(&img),
            "data:image/png;base64,VGVzdA=="
        );
    }
}
