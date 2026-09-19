//! Compact JSON to append for an MCP `tools/call` result's `structuredContent`.

use serde::Deserialize;
use serde_json::Value;

/// The compact JSON to append for `structuredContent`, or `None` when a rendered `content` part
/// already carries it: the document at the part's first `{`/`[` equals the payload, or for a
/// scalar (spec >= 2026-07-28) the whole part is its JSON or the string itself. The spec only says
/// servers SHOULD inline it, so a structured-first server would otherwise leave the model with the
/// summary line alone. Callers append the result last so truncation cuts it first.
pub fn render_structured_content<'a>(
    structured: Option<&Value>,
    parts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let structured = structured.filter(|v| !v.is_null())?;
    let is_inlined = parts.into_iter().any(|text| {
        let carried = match structured {
            Value::Object(_) => document_at(text, '{'),
            Value::Array(_) => document_at(text, '['),
            Value::String(s) if s == text => return true,
            _ => serde_json::from_str(text).ok(),
        };
        carried.as_ref() == Some(structured)
    });
    (!is_inlined).then(|| structured.to_string())
}

fn document_at(text: &str, open: char) -> Option<Value> {
    let start = text.find(open)?;
    Value::deserialize(&mut serde_json::Deserializer::from_str(&text[start..])).ok()
}

#[cfg(test)]
#[path = "mcp_structured_content_tests.rs"]
mod tests;
