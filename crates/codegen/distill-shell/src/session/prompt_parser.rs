// Modified for Distill by Samuel Fajreldines, 2026.
use crate::session::user_message::user_query;
use agent_client_protocol::{self as acp, ImageContent};
use distill_workspace::file_system::{
    FileReference, render_embedded_resource, render_file_reference,
};
use serde::Deserialize;
use std::ops::Range;
use std::path::PathBuf;
/// Some templates put `<user_query>` last (context first); Grok puts it first.
/// Keeping them separate lets the caller truncate context without searching for the query boundary in a flat string.
#[derive(Debug, Clone)]
pub struct ParsedPrompt {
    /// Context blocks: `<attached_files>` payloads and resource-link sections.
    /// Grok mode may include editor open/focus metadata; the compat mode does not.
    /// Empty string when there is no context.
    pub context: String,
    /// The user's query, already wrapped in `<user_query>` tags (or raw when verbatim).
    pub query: String,
    /// Skill information block: `<skill_information>` envelope with expanded skill content.
    /// Empty string when no skills were invoked.
    pub skill_information: String,
    pub images: Vec<ImageContent>,
    /// Whether the prompt was parsed in query-last mode.
    pub is_cursor: bool,
}
impl ParsedPrompt {
    pub fn assemble(&self) -> String {
        Self::assemble_parts_with_skills(
            &self.context,
            &self.query,
            &self.skill_information,
            self.is_cursor,
        )
    }
    /// Grok mode: `<user_query>`, then `<skill_information>`, then context.
    /// Query-last mode: context, then `<user_query>`, then `<skill_information>`.
    /// The `<skill_information>` block always follows `<user_query>` immediately.
    pub fn assemble_parts_with_skills(
        context: &str,
        query: &str,
        skill_information: &str,
        is_cursor: bool,
    ) -> String {
        Self::assemble_with_layout(context, query, skill_information, is_cursor).0
    }
    /// [`Self::assemble_parts_with_skills`] plus the byte range of each part in the result,
    /// recorded as the parts are pushed, so a part-relative cut maps back to bytes of the assembled
    /// message.
    pub fn assemble_with_layout(
        context: &str,
        query: &str,
        skill_information: &str,
        is_cursor: bool,
    ) -> (String, PromptLayout) {
        let _ = is_cursor;
        let context_first = false;
        let mut out = String::new();
        let mut push = |part: &str| {
            let start = out.len();
            out.push_str(part);
            start..out.len()
        };
        let mut context_range = None;
        if context_first {
            context_range = Some(push(context));
            push("\n\n");
        }
        let query_range = push(query);
        let skill = if skill_information.is_empty() {
            None
        } else {
            push("\n");
            Some(push(skill_information))
        };
        if !context_first && !context.is_empty() {
            push("\n\n");
            context_range = Some(push(context));
        }
        let layout = PromptLayout {
            query: query_range,
            skill,
            context: context_range,
        };
        (out, layout)
    }
}
/// Byte ranges of each non-empty part inside the string [`ParsedPrompt::assemble_with_layout`]
/// returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLayout {
    pub query: Range<usize>,
    pub skill: Option<Range<usize>>,
    pub context: Option<Range<usize>>,
}
/// When `is_cursor` is true, produces query-last format output.
/// `<attached_files>` (bare), resource links, then `<user_query>` last.
/// `<user_query>` first, then `<system-reminder>` wrapped `<attached_files>` and resource links.
pub async fn parse_prompt(
    prompt: &[acp::ContentBlock],
    working_directory: PathBuf,
    _session_info: &crate::session::info::Info,
    verbatim: bool,
    is_cursor: bool,
) -> Result<ParsedPrompt, acp::Error> {
    parse_prompt_with_skills(
        prompt,
        working_directory,
        _session_info,
        super::InputAuthority::HumanIntent,
        verbatim,
        is_cursor,
        String::new(),
    )
    .await
}
/// This is the full-featured entry point. `parse_prompt` delegates here with an empty `skill_information` string for backward compatibility.
pub(crate) async fn parse_prompt_with_skills(
    prompt: &[acp::ContentBlock],
    working_directory: PathBuf,
    _session_info: &crate::session::info::Info,
    authority: super::InputAuthority,
    verbatim: bool,
    is_cursor: bool,
    skill_information: String,
) -> Result<ParsedPrompt, acp::Error> {
    let parse_span =
        distill_telemetry::region::Region::from_span(tracing::info_span!("prompt.parse"));
    let allows_file_expansion = authority != super::InputAuthority::ModelAuthoredUntrusted;
    let mut message_parts: Vec<String> = Vec::new();
    let mut image_parts = Vec::new();
    let mut resource_links = Vec::new();
    let mut embedded_resources = Vec::new();
    for block in prompt {
        match block {
            acp::ContentBlock::Text(text) => message_parts.push(text.text.clone()),
            acp::ContentBlock::Image(image_content) => image_parts.push(image_content.clone()),
            acp::ContentBlock::ResourceLink(link) => {
                resource_links.push(link.clone());
                if allows_file_expansion && link.meta.is_none() {
                    let path = extract_path_from_uri(link);
                    message_parts.push(format!("@{path}"));
                }
            }
            acp::ContentBlock::Resource(resource) => embedded_resources.push(resource.clone()),
            other => {
                return Err(acp::Error::invalid_params()
                    .data(format!("unsupported content block in prompt: {other:?}")));
            }
        }
    }
    let message = message_parts.join(" ");
    let file_ref_tokens = if allows_file_expansion {
        collect_file_references(&message)
    } else {
        Vec::new()
    };
    let mut file_ref_contents = Vec::new();
    let mut at_mention_bytes: usize = 0;
    let at_mention_span = (!file_ref_tokens.is_empty()).then(|| {
        distill_telemetry::region::Region::from_span(tracing::info_span!(
            parent: parse_span.span(),
            "prompt.at_mention_resolve",
            file_count = tracing::field::Empty,
            bytes = tracing::field::Empty,
        ))
    });
    for token in file_ref_tokens {
        let Some(mut file_ref) = FileReference::parse(&token) else {
            continue;
        };
        file_ref.path = working_directory.join(&file_ref.path);
        let rendered_file = render_file_reference(file_ref, is_cursor).await;
        let success = rendered_file.is_some();
        distill_telemetry::event_span!("prompt.at_mention", mention_type = "file", success);
        if let Some(rendered_file) = rendered_file {
            at_mention_bytes += rendered_file.len();
            file_ref_contents.push(rendered_file);
        }
    }
    if let Some(at_mention_span) = at_mention_span {
        at_mention_span
            .span()
            .record("file_count", file_ref_contents.len() as i64);
        at_mention_span
            .span()
            .record("bytes", at_mention_bytes as i64);
        at_mention_span.close();
    }
    let mut embedded_contents = Vec::new();
    for resource in &embedded_resources {
        if let Some(rendered) = render_embedded_resource(resource, is_cursor).await {
            embedded_contents.push(rendered);
        }
    }
    let parsed = render_message(
        message,
        embedded_contents,
        file_ref_contents,
        &resource_links,
        verbatim,
        is_cursor,
    );
    Ok(ParsedPrompt {
        context: parsed.0,
        query: parsed.1,
        skill_information,
        images: image_parts,
        is_cursor,
    })
}
/// Returns `(context, query)`, the two halves of the prompt kept separate.
/// The caller can truncate context without searching for the query boundary.
fn render_message(
    message: String,
    embedded_contents: Vec<String>,
    file_ref_contents: Vec<String>,
    resource_links: &[acp::ResourceLink],
    verbatim: bool,
    is_cursor: bool,
) -> (String, String) {
    let all_attached_contents: Vec<String> = embedded_contents
        .into_iter()
        .chain(file_ref_contents)
        .collect();
    let wrap = |msg: String| -> String { if verbatim { msg } else { user_query(msg) } };
    let query = wrap(message);
    let _ = is_cursor;
    let mut context = String::new();
    if !all_attached_contents.is_empty() {
        context.push_str(&format!(
            r#"<system-reminder>
Below are some potentially helpful/relevant pieces of information for figuring out how to respond

<attached_files>

{}

</attached_files>

</system-reminder>"#,
            all_attached_contents.join("\n\n"),
        ));
    }
    if !resource_links.is_empty() {
        if !context.is_empty() {
            context.push_str("\n\n");
        }
        context.push_str(&render_resource_links_grok(resource_links));
    }
    (context, query)
}
fn collect_file_references(message: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut i = 0;
    while i < message.len() {
        if !message.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let Some(at_symbol_offset) = message.get(i..).and_then(|s| s.find('@')) else {
            break;
        };
        let at = i + at_symbol_offset;
        if !message.is_char_boundary(at) {
            i = at.saturating_add(1);
            continue;
        }
        let start = at + '@'.len_utf8();
        if start > message.len() || !message.is_char_boundary(start) {
            break;
        }
        if let Some(ch) = message.get(..at).and_then(|s| s.chars().next_back())
            && (ch.is_alphanumeric() || ch == '_')
        {
            i = start;
            continue;
        }
        let Some(rest) = message.get(start..) else {
            break;
        };
        let token = rest.split_whitespace().next().unwrap_or("");
        if !token.is_empty() {
            paths.push(token.to_string());
        }
        i = start + token.len().max(1);
        while i < message.len() && !message.is_char_boundary(i) {
            i += 1;
        }
    }
    paths
}
#[derive(Debug, Deserialize)]
struct CursorPosition {
    line: u64,
    column: u64,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "fileState")]
enum FileState {
    /// The file is currently visible in the editor
    #[serde(rename = "focused")]
    Focused { cursor: CursorPosition },
    /// The file is open in a tab but not currently visible
    #[serde(rename = "open")]
    Open,
}
#[derive(Debug, Deserialize)]
struct EditorMeta {
    source: String,
    #[serde(flatten)]
    file_state: FileState,
}
fn parse_editor_meta(link: &acp::ResourceLink) -> Option<EditorMeta> {
    let meta_value = link.meta.as_ref()?;
    let editor_meta: EditorMeta =
        serde_json::from_value(serde_json::Value::Object(meta_value.clone())).ok()?;
    if editor_meta.source != "editor" {
        return None;
    }
    Some(editor_meta)
}
fn extract_path_from_uri(link: &acp::ResourceLink) -> String {
    if let Some(path) = link.uri.strip_prefix("file://") {
        path.to_string()
    } else {
        link.name.clone()
    }
}
fn render_regular_links(links: &[&acp::ResourceLink]) -> String {
    let mut s =
        String::from("Below is data for the files mentioned by the user\nReferenced resources:\n");
    for (idx, link) in links.iter().enumerate() {
        let label = link
            .title
            .as_deref()
            .or(link.description.as_deref())
            .unwrap_or(&link.name);
        if let Some(size) = link.size {
            s.push_str(&format!("{idx}. {label} -> {} (~{size} bytes)\n", link.uri));
        } else {
            s.push_str(&format!("{idx}. {label} -> {}\n", link.uri));
        }
    }
    s.trim_end_matches('\n').to_string()
}
/// Grok-format resource links: `<focused_files>` / `<open_files>` with metadata inside a `<system-reminder>` wrapper.
fn render_resource_links_grok(resource_links: &[acp::ResourceLink]) -> String {
    let mut regular_links = Vec::new();
    let mut focused_files = Vec::new();
    let mut open_files = Vec::new();
    for link in resource_links {
        match parse_editor_meta(link) {
            Some(EditorMeta {
                file_state: FileState::Focused { cursor },
                ..
            }) => {
                focused_files.push((extract_path_from_uri(link), cursor));
            }
            Some(EditorMeta {
                file_state: FileState::Open,
                ..
            }) => {
                open_files.push(extract_path_from_uri(link));
            }
            None => {
                regular_links.push(link);
            }
        }
    }
    let mut sections: Vec<String> = Vec::new();
    if !regular_links.is_empty() {
        sections.push(render_regular_links(&regular_links));
    }
    if !focused_files.is_empty() {
        sections.push(render_focused_files(&focused_files));
    }
    if !open_files.is_empty() {
        sections.push(render_open_files(&open_files));
    }
    format!(
        "<system-reminder>\n{}\n</system-reminder>",
        sections.join("\n\n")
    )
}
fn render_focused_files(files: &[(String, CursorPosition)]) -> String {
    let mut s = String::from(
        "Below is data for the file(s) the user is currently actively looking at while making their query\n<focused_files>\n",
    );
    for (path, cursor) in files {
        s.push_str(&format!(
            "<file path=\"{path}\" cursor_line=\"{}\" cursor_column=\"{}\"/>\n",
            cursor.line, cursor.column
        ));
    }
    s.push_str("</focused_files>");
    s
}
fn render_open_files(paths: &[String]) -> String {
    let mut s = String::from(
        "Below is data for the file(s) the user has previously opened but are not currently visible to the user\n<open_files>\n",
    );
    for path in paths {
        s.push_str(&format!("<file path=\"{path}\"/>\n"));
    }
    s.push_str("</open_files>");
    s
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Assemble a `render_message` result into a flat string for test assertions.
    fn assemble(parts: (String, String), is_cursor: bool) -> String {
        let (context, query) = parts;
        if context.is_empty() {
            return query;
        }
        if is_cursor {
            format!("{context}\n\n{query}")
        } else {
            format!("{query}\n\n{context}")
        }
    }
    /// Shorthand: render and assemble for grok mode.
    fn render_grok(
        message: &str,
        embedded: Vec<String>,
        file_refs: Vec<String>,
        links: &[acp::ResourceLink],
        verbatim: bool,
    ) -> String {
        assemble(
            render_message(message.into(), embedded, file_refs, links, verbatim, false),
            false,
        )
    }
    #[test]
    fn test_collect_single_reference() {
        let tokens = collect_file_references("look at @src/main.rs please");
        assert_eq!(tokens, vec!["src/main.rs"]);
    }
    #[test]
    fn test_collect_multiple_references() {
        let tokens = collect_file_references("check @foo.rs and @bar/baz.rs");
        assert_eq!(tokens, vec!["foo.rs", "bar/baz.rs"]);
    }
    #[test]
    fn test_collect_reference_with_line_range() {
        let tokens = collect_file_references("see @lib.rs:10-20 for details");
        assert_eq!(tokens, vec!["lib.rs:10-20"]);
    }
    #[test]
    fn test_collect_no_references() {
        let tokens = collect_file_references("no file references here");
        assert!(tokens.is_empty());
    }
    #[test]
    fn test_collect_at_end_of_message() {
        let tokens = collect_file_references("check @README.md");
        assert_eq!(tokens, vec!["README.md"]);
    }
    #[test]
    fn test_collect_trailing_at_ignored() {
        let tokens = collect_file_references("trailing @");
        assert!(tokens.is_empty());
    }
    #[test]
    fn test_collect_at_with_space() {
        let tokens = collect_file_references("email me @ work");
        assert_eq!(tokens, vec!["work"]);
    }
    #[test]
    fn test_collect_adjacent_references() {
        let tokens = collect_file_references("@a.rs @b.rs");
        assert_eq!(tokens, vec!["a.rs", "b.rs"]);
    }
    #[test]
    fn test_collect_skips_email_addresses() {
        let tokens = collect_file_references("email foo@bar.com and also @src/main.rs");
        assert_eq!(tokens, vec!["src/main.rs"]);
    }
    #[test]
    fn test_collect_email_and_at_ref_with_multibyte() {
        let tokens = collect_file_references("連絡先 foo@bar.com と @src/main.rs を見て");
        assert_eq!(tokens, vec!["src/main.rs"]);
    }
    fn make_link(meta: Option<serde_json::Value>) -> acp::ResourceLink {
        let mut link = acp::ResourceLink::new("test.rs", "file:///project/test.rs");
        if let Some(m) = meta.and_then(|v| v.as_object().cloned()) {
            link = link.meta(m);
        }
        link
    }
    #[test]
    fn test_parse_editor_meta_focused_with_cursor() {
        let link = make_link(Some(serde_json::json!({
            "source": "editor",
            "fileState": "focused",
            "cursor": { "line": 10, "column": 3 }
        })));
        let meta = parse_editor_meta(&link).expect("should parse");
        assert!(matches!(
            meta.file_state,
            FileState::Focused {
                cursor: CursorPosition {
                    line: 10,
                    column: 3
                }
            }
        ));
    }
    #[test]
    fn test_parse_editor_meta_focused_without_cursor_fails() {
        let link = make_link(Some(serde_json::json!({
            "source": "editor",
            "fileState": "focused"
        })));
        assert!(parse_editor_meta(&link).is_none());
    }
    #[test]
    fn test_parse_editor_meta_open() {
        let link = make_link(Some(serde_json::json!({
            "source": "editor",
            "fileState": "open"
        })));
        let meta = parse_editor_meta(&link).expect("should parse");
        assert!(matches!(meta.file_state, FileState::Open));
    }
    #[test]
    fn test_parse_editor_meta_non_editor_source_returns_none() {
        let link = make_link(Some(serde_json::json!({
            "source": "something_else",
            "fileState": "focused"
        })));
        assert!(parse_editor_meta(&link).is_none());
    }
    #[test]
    fn test_parse_editor_meta_no_meta_returns_none() {
        let link = make_link(None);
        assert!(parse_editor_meta(&link).is_none());
    }
    #[test]
    fn test_parse_editor_meta_unknown_file_state_returns_none() {
        let link = make_link(Some(serde_json::json!({
            "source": "editor",
            "fileState": "minimized"
        })));
        assert!(parse_editor_meta(&link).is_none());
    }
    #[tokio::test]
    async fn test_runtime_control_prompt_expands_file_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("status.log"), "RUNTIME_FILE_MARKER").unwrap();
        let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            "inspect @status.log",
        ))];
        let info = crate::session::info::Info {
            id: agent_client_protocol::SessionId::new("test"),
            cwd: dir.path().display().to_string(),
        };
        let parsed = parse_prompt_with_skills(
            &blocks,
            dir.path().to_path_buf(),
            &info,
            super::super::InputAuthority::RuntimeControl,
            false,
            false,
            String::new(),
        )
        .await
        .unwrap();
        assert!(parsed.assemble().contains("RUNTIME_FILE_MARKER"));
    }
    #[tokio::test]
    async fn test_model_authored_prompt_does_not_expand_file_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.txt"), "TEXT_SECRET_MARKER").unwrap();
        std::fs::write(dir.path().join("linked-secret.txt"), "LINK_SECRET_MARKER").unwrap();
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("inspect @secret.txt")),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                "linked-secret.txt",
                "file://linked-secret.txt",
            )),
        ];
        let info = crate::session::info::Info {
            id: agent_client_protocol::SessionId::new("test"),
            cwd: dir.path().display().to_string(),
        };
        let parsed = parse_prompt_with_skills(
            &blocks,
            dir.path().to_path_buf(),
            &info,
            super::super::InputAuthority::ModelAuthoredUntrusted,
            false,
            false,
            String::new(),
        )
        .await
        .unwrap();
        let assembled = parsed.assemble();
        assert!(parsed.query.contains("inspect @secret.txt"));
        assert!(!parsed.query.contains("@linked-secret.txt"));
        assert!(assembled.contains("file://linked-secret.txt"));
        for forbidden in [
            "TEXT_SECRET_MARKER",
            "LINK_SECRET_MARKER",
            "<attached_files>",
            "<file_contents",
        ] {
            assert!(
                !assembled.contains(forbidden),
                "unexpected {forbidden}: {assembled}"
            );
        }
    }
    #[test]
    fn test_grok_render_plain_message() {
        let result = render_grok("hello", vec![], vec![], &[], false);
        assert_eq!(result, "<user_query>\nhello\n</user_query>");
    }
    #[test]
    fn test_grok_render_with_attachments_uses_system_reminder_wrapper() {
        let result = render_grok(
            "check this",
            vec!["embedded content".into()],
            vec![],
            &[],
            false,
        );
        assert!(
            result.contains("<system-reminder>"),
            "expected system-reminder wrapper, got: {result}"
        );
        assert!(result.contains("<attached_files>"));
        assert!(result.contains("embedded content"));
        assert!(
            result.starts_with("<user_query>"),
            "Grok should start with <user_query>, got: {result}"
        );
    }
    #[test]
    fn test_grok_render_user_query_first() {
        let link = acp::ResourceLink::new("doc.md", "file:///doc.md")
            .title(Some("My Doc".into()))
            .size(Some(1024));
        let result = render_grok("hello", vec![], vec![], &[link], false);
        let uq_pos = result.find("<user_query>").unwrap();
        let rr_pos = result.find("Referenced resources:").unwrap();
        assert!(
            uq_pos < rr_pos,
            "Grok: <user_query> ({uq_pos}) should come before resource links ({rr_pos})\ngot: {result}"
        );
        assert!(result.contains("<system-reminder>"));
    }
    #[test]
    fn test_grok_render_resource_links_use_focused_files_format() {
        let links = vec![
            acp::ResourceLink::new("main.rs", "file:///project/src/main.rs").meta(
                serde_json::json!({ "source" : "editor", "fileState" : "focused",
            "cursor" : { "line" : 42, "column" : 10 } })
                .as_object()
                .cloned(),
            ),
            acp::ResourceLink::new("Cargo.toml", "file:///project/Cargo.toml").meta(
                serde_json::json!({ "source" : "editor", "fileState" : "open" })
                    .as_object()
                    .cloned(),
            ),
        ];
        let result = render_grok("hello", vec![], vec![], &links, false);
        assert!(result.contains("<system-reminder>"), "got: {result}");
        assert!(result.contains("<focused_files>"), "got: {result}");
        assert!(result.contains("<open_files>"), "got: {result}");
        assert!(
            !result.contains("<open_and_recently_viewed_files>"),
            "got: {result}"
        );
    }
    /// For every empty/non-empty combination of the parts, in both layouts, the assembled string
    /// matches an independently built expectation and the layout slices back to the parts.
    #[test]
    fn assemble_with_layout_matches_expectation_for_all_part_combinations() {
        let check = |is_cursor: bool| {
            for context in ["", "<attached_files>ctx</attached_files>"] {
                for query in ["", "<user_query>q</user_query>"] {
                    for skill in ["", "<skill_information>s</skill_information>"] {
                        let skill_joiner = if skill.is_empty() { "" } else { "\n" };
                        let block = format!("{query}{skill_joiner}{skill}");
                        let context_joiner = if context.is_empty() { "" } else { "\n\n" };
                        let expected = if is_cursor {
                            format!("{context}{context_joiner}{block}")
                        } else {
                            format!("{block}{context_joiner}{context}")
                        };
                        let (assembled, layout) =
                            ParsedPrompt::assemble_with_layout(context, query, skill, is_cursor);
                        assert_eq!(expected, assembled);
                        assert_eq!(query, &assembled[layout.query]);
                        let skill_slice = layout.skill.map(|r| &assembled[r]);
                        assert_eq!((!skill.is_empty()).then_some(skill), skill_slice);
                        let context_slice = layout.context.map(|r| &assembled[r]);
                        assert_eq!((!context.is_empty()).then_some(context), context_slice);
                    }
                }
            }
        };
        check(false);
    }
}
