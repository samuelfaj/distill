// Modified for Distill by Samuel Fajreldines, 2026.
//! ReadFile — new-architecture implementation.
//!
//! Reuses the core logic (`extract_file_content_lines`, `bytes_to_metadata`,
//! constants) from the old `implementations::read_file` module.
//! State:
//! - Notifications emitted via `NotificationHandle` from Resources.
//!
//! Reminders are NOT implemented here (Phase 5).
use crate::implementations::read_file::{
    handle_pdf, is_pdf_file, raw_text_to_file_content, run_document_extraction,
};
use crate::types::context::TruncationConfig;
use crate::types::output::{FileContent, ReadFileOutput};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::Params;
#[allow(unused_imports)]
use crate::types::resources::{
    Cwd, DisplayCwd, FileSystem, GitignoreFilter, PathNotFoundHints, RespectGitignore,
    SharedResources, TruncationCfg, display_cwd_or_cwd, resolve_model_path,
};
use crate::types::skill_discovery_tracker::SkillManager;
use crate::types::template_renderer::TemplateRenderer;
use crate::types::tool::{ToolKind, ToolNamespace};
use std::sync::LazyLock;
mod versions;
use crate::types::schema::GrokIntegerSchema;
/// Configuration for the ReadFile tool, stored as `Params<ReadFileParams>` in Resources.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileParams {
    #[serde(default)]
    pub cursor_rules_on_read: bool,
    /// Byte budget for the formatted text window. When the window exceeds it, only the leading whole
    /// lines that fit are returned (at least one) plus a continuation marker naming the next offset.
    /// `None` keeps the token cap ([`READ_FILE_MAX_TOKENS`]) as the only size limit. Skill markdown
    /// returned whole (under the token cap) is exempt; windowed reads are budgeted.
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}
crate::register_resource!("distill", "ReadFile", ReadFileParams);
/// Internal version discriminant for read_file. `read_file` has cross-cutting version divergence:
/// gitignore enforcement and error mapping. If extracting into version modules, this tool is the
/// highest-risk candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFileVersion {
    Current,
    Legacy0_4_10,
}
impl ReadFileVersion {
    pub(crate) fn from_contract(v: Option<&str>) -> Self {
        match v {
            Some("legacy-0.4.10") => Self::Legacy0_4_10,
            _ => Self::Current,
        }
    }
    pub(crate) fn is_legacy(self) -> bool {
        self == Self::Legacy0_4_10
    }
}
/// Per-call token cap on the returned window.
pub const READ_FILE_MAX_TOKENS: usize = 25_000;
pub const READ_FILE_MAX_BYTES: usize =
    READ_FILE_MAX_TOKENS * distill_token_estimation::BYTES_PER_TOKEN as usize;
pub const MAX_LINES_READ: usize = 1_000;
/// True when `text` is over the per-call token cap; the shell uses the same check to decide when a
/// prompt must be offloaded.
pub fn exceeds_read_cap(text: &str) -> bool {
    distill_token_estimation::estimate_tokens(text) > READ_FILE_MAX_TOKENS as u64
}
pub use crate::implementations::read_file::{
    FileMetadata, PDF_MAX_PAGES_PER_READ, bytes_to_metadata, parse_page_range,
};
/// Max size of one streamed delta: strictly below `stream_chunk`'s 16 KiB
/// cap (so a delta is never capped/gapped) and char-aligned (so concatenated
/// deltas reproduce the terminal `content` byte-for-byte).
const STREAM_DELTA_TARGET_BYTES: usize = 4 * 1024;
/// ReadFile's capabilities incl. its streaming spec (single source of
/// truth). Streams the formatted projection (not raw bytes) as inert
/// `PlainText` / `Append`.
static READ_FILE_CAPABILITIES: LazyLock<distill_tool_protocol::ToolCapabilities> =
    LazyLock::new(|| distill_tool_protocol::ToolCapabilities {
        is_read_only: true,
        tool_scope: Some(distill_tool_protocol::ToolScope::Read),
        streaming: Some(distill_tool_protocol::StreamingSpec {
            subkind: "read_file_chunk".to_owned(),
            max_delta_bytes: None,
        }),
        ..Default::default()
    });
const MAX_PPTX_BYTES: usize = 50 * 1024 * 1024;
const PPTX_PROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
async fn handle_pptx(
    file_bytes: Vec<u8>,
    path: &std::path::Path,
) -> Result<ReadFileOutput, distill_tool_runtime::ToolError> {
    run_document_extraction(
        file_bytes,
        path,
        "PPTX",
        MAX_PPTX_BYTES,
        PPTX_PROCESS_TIMEOUT,
        extract_pptx_text,
    )
    .await
}
/// Extract text from a PPTX file (zip + DrawingML text runs). Returns line-numbered text via the
/// shared `raw_text_to_file_content` helper.
fn extract_pptx_text(file_bytes: Vec<u8>) -> Result<ReadFileOutput, String> {
    let text = crate::implementations::read_file::pptx::extract_pptx_text_from_bytes(&file_bytes)
        .map_err(|e| format!("Failed to extract text from PPTX: {e}"))?;
    Ok(raw_text_to_file_content(text))
}
/// Description for default toolset (full/non-concise)
pub(crate) const DESCRIPTION_FULL: &str = r#"Read a file.

Usage:
- The ${{ params.read.target_file }} parameter can be a relative path in the workspace or an absolute path
- By default, it reads up to {max_lines_read} lines starting from the beginning of the file
- Line numbers (1-based) appear as anchors in the format LINE_NUMBER→LINE_CONTENT on the first returned line and on every 10th line of the file; the lines in between show content only. Count from the nearest anchor when referring to a specific line
- This tool can read PDF files (.pdf), PowerPoint files (.pptx), Jupyter notebooks (.ipynb files), and image files (e.g. PNG, JPG, etc).
- When reading an image file the contents are presented visually as this tool uses multimodal LLMs."#;
/// Schema-only advertised default (runtime still treats omit as line 1 via unwrap_or).
fn schema_default_offset() -> Option<i64> {
    Some(1)
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileInput {
    #[serde(rename = "target_file")]
    #[schemars(
        description = "The path of the file to read. You can use either a relative path in the workspace or an absolute path. If an absolute path is provided, it will be preserved as is."
    )]
    pub path: String,
    #[serde(
        default,
        deserialize_with = "crate::types::schema::deserialize_lenient_i64",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(
        with = "GrokIntegerSchema",
        default = "schema_default_offset",
        description = "The line number to start reading from. Only provide if the file is too large to read at once."
    )]
    pub offset: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        with = "GrokIntegerSchema",
        description = "The number of lines to read. Only provide if the file is too large to read at once."
    )]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Page range for PDF files (e.g. '1-5', '3', '10-'). Required for PDFs with more than 10 pages. Max 20 pages per call. Ignored for non-PDF files."
    )]
    pub pages: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Output format for PDF files. 'image' (default) renders pages as images. 'text' extracts text content. Ignored for non-PDF files."
    )]
    pub format: Option<String>,
}
async fn cursor_rules_on_read_enabled(resources: &SharedResources) -> bool {
    let res = resources.lock().await;
    res.get::<Params<ReadFileParams>>()
        .is_some_and(|p| p.0.cursor_rules_on_read)
}
async fn max_output_bytes(resources: &SharedResources) -> Option<usize> {
    let res = resources.lock().await;
    res.get::<Params<ReadFileParams>>()
        .and_then(|p| p.0.max_output_bytes)
}
/// Number of leading whole lines of `content` (formatted output, `\n`-joined) whose cumulative length
/// fits in `budget` bytes. Always at least 1 so a window whose first line alone is over budget still
/// makes progress instead of returning an empty body.
fn lines_within_byte_budget(content: &str, budget: usize) -> usize {
    let mut used = 0usize;
    let mut kept = 0usize;
    for line in content.split_inclusive('\n') {
        used += line.len();
        if used > budget && kept > 0 {
            break;
        }
        kept += 1;
    }
    kept.max(1)
}
/// Cut the window to whole lines within `budget` and tell the model where to resume. Re-extracts so
/// `raw_output` and `extracted_images` match the returned lines; the marker is appended to the
/// formatted text only.
fn apply_byte_budget(
    extracted: ExtractedContent,
    budget: usize,
    file_content: &str,
    offset: Option<i64>,
    total_lines: usize,
    offset_param: &str,
) -> ExtractedContent {
    if extracted.content.len() <= budget {
        return extracted;
    }
    let full_len = extracted.content.len();
    let kept = lines_within_byte_budget(&extracted.content, budget);
    let start_line = resolve_read_start_line(file_content, offset);
    let mut bounded = extract_file_content_lines(file_content, offset, Some(kept), total_lines);
    let end_line = start_line + kept - 1;
    let next_offset = start_line + kept;
    let truncated_bytes = full_len.saturating_sub(bounded.content.len());
    let marker = format!(
        "\n... [{truncated_bytes} characters truncated; file has {total_lines} total lines; \
         showing lines {start_line}-{end_line}; rerun with {offset_param}={next_offset}] ..."
    );
    bounded.content.push_str(&marker);
    bounded.content_concise.push_str(&marker);
    bounded
}
/// Harness-compatible negative offset resolution (1-indexed start line). Negatives use the reference `split('\n')` field count plus a phantom
/// field when the file is non-empty and has no trailing `\n`. Extraction still uses `split_inclusive`, so a start that lands on the
/// phantom-only field yields an empty window (harness-aligned; not a Grok-line clamp).
fn resolve_read_start_line(file_content: &str, offset: Option<i64>) -> usize {
    let offset_raw = offset.unwrap_or(1);
    if offset_raw == 0 {
        return 1;
    }
    if offset_raw > 0 {
        return offset_raw as usize;
    }
    let mut total_fields = file_content.split('\n').count();
    if !file_content.is_empty() && !file_content.ends_with('\n') {
        total_fields += 1;
    }
    let computed = (total_fields as i64) + offset_raw + 1;
    computed.max(1) as usize
}
/// Only non-negative raw offsets are stored on `FileContent`. Negatives become
/// `None` ("from beginning"); we mirror only the signed input wire type, not
/// the resolved start line.
fn stored_read_offset(offset: Option<i64>) -> Option<usize> {
    offset.filter(|&o| o >= 0).map(|o| o as usize)
}
/// Files read in full (offset/limit ignored) when under `READ_FILE_MAX_TOKENS`, honoring offset/limit like any other file above it: any file named
/// exactly `SKILL.md`, plus any Markdown file with a `skills` path component so docs a `SKILL.md` references come back whole. Intentionally broader
/// than skill discovery's dir check: any `skills` segment (plugin/bundled/user roots), matched exactly (not case-folded) so `skills-cursor` does not qualify.
fn is_skill_markdown(path: &std::path::Path) -> bool {
    if path.file_name().is_some_and(|n| n == "SKILL.md") {
        return true;
    }
    let is_md = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    if !is_md {
        return false;
    }
    use std::path::Component;
    let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                stack.pop();
            }
            Component::Normal(c) => stack.push(c),
        }
    }
    stack.into_iter().any(|c| c == "skills")
}
/// Result of extracting file content lines with both default and concise formats
pub struct ExtractedContent {
    /// Default format: line numbers with → separator (no padding)
    pub content: String,
    /// Concise format: identical to content (kept for backward compatibility)
    pub content_concise: String,
    /// Raw unformatted content
    pub raw_output: String,
    /// Base64 images captured per-line before truncation. Plumbed through
    /// `FileContent.extracted_images` and turned into multimodal
    /// `ContentPart::Image` follow-ups by the session layer.
    pub extracted_images: Vec<crate::util::base64_images::ExtractedImage>,
}
pub fn extract_file_content_lines(
    file_content: &str,
    offset: Option<i64>,
    limit: Option<usize>,
    total_lines: usize,
) -> ExtractedContent {
    fn strip(s: &str) -> &str {
        let Some(s) = s.strip_suffix('\n') else {
            return s;
        };
        let Some(line) = s.strip_suffix('\r') else {
            return s;
        };
        line
    }
    use std::borrow::Cow;
    use std::fmt::Write as _;
    let mut output = String::new();
    let mut output_concise = String::new();
    let (mut start, mut end) = (0, 0);
    let mut first_line: Option<usize> = None;
    let mut extracted_images: Vec<crate::util::base64_images::ExtractedImage> = Vec::new();
    let split_count = file_content.split_inclusive('\n').count();
    let has_trailing_empty = !file_content.is_empty() && file_content.ends_with('\n');
    let skip = resolve_read_start_line(file_content, offset).saturating_sub(1);
    let take = limit.unwrap_or(usize::MAX);
    if file_content.is_empty() && total_lines > 0 && skip == 0 && take > 0 {
        _ = write!(&mut output, "1→").ok();
        _ = write!(&mut output_concise, "1→").ok();
        first_line = Some(1);
    }
    for (i, (pos, line_len, line)) in file_content
        .split_inclusive('\n')
        .scan(0, |pos, line| {
            let out = *pos;
            let line_len = line.len();
            *pos += line_len;
            Some((out, line_len, strip(line)))
        })
        .enumerate()
        .skip(skip)
        .take(take)
    {
        let is_first_visible = first_line.is_none();
        if is_first_visible {
            start = pos;
            first_line = Some(i + 1);
        } else {
            output.push('\n');
            output_concise.push('\n');
        }
        end = pos + line_len;
        let line: Cow<'_, str> = match crate::util::base64_images::try_extract_base64_images(line) {
            Some(result) => {
                extracted_images.extend(result.images);
                Cow::Owned(result.text)
            }
            None => Cow::Borrowed(line),
        };
        let line_num = i + 1;
        if is_first_visible || line_num.is_multiple_of(10) {
            _ = write!(&mut output, "{line_num}→{line}").ok();
            _ = write!(&mut output_concise, "{line_num}→{line}").ok();
        } else {
            output.push_str(&line);
            output_concise.push_str(&line);
        }
    }
    if has_trailing_empty {
        let trailing_line_idx = split_count;
        if trailing_line_idx >= skip && trailing_line_idx < skip.saturating_add(take) {
            let line_num = trailing_line_idx + 1;
            let is_first_visible = first_line.is_none();
            if is_first_visible {
                first_line = Some(line_num);
            } else {
                output.push('\n');
                output_concise.push('\n');
            }
            if is_first_visible || line_num.is_multiple_of(10) {
                _ = write!(&mut output, "{line_num}→").ok();
                _ = write!(&mut output_concise, "{line_num}→").ok();
            }
        }
    }
    let mut raw_output = if first_line.is_none() || file_content.is_empty() {
        String::new()
    } else {
        file_content.get(start..end).unwrap_or("").to_owned()
    };
    if raw_output.ends_with("\r\n") {
        raw_output.truncate(raw_output.len().saturating_sub(2));
        raw_output.push('\n');
    }
    ExtractedContent {
        content: output,
        content_concise: output_concise,
        raw_output,
        extracted_images,
    }
}
/// Core read-file logic shared by `ReadFileTool` and `ReadFileConciseTool`. Always uses the padded
/// `content` field. Concise post-processing (swapping in `content_concise`) is done by
/// `ReadFileConciseTool` after this returns.
pub(crate) async fn run_read_file(
    input: ReadFileInput,
    cwd_override: Option<std::path::PathBuf>,
    contract_version: Option<&str>,
    resources: SharedResources,
    streamable_out: Option<&mut bool>,
    invoking_param_names: &crate::types::resources::InvokingToolParamNames,
) -> Result<ReadFileOutput, distill_tool_runtime::ToolError> {
    let (cwd, display_cwd, fs, hints_enabled);
    {
        let res = resources.lock().await;
        cwd = match cwd_override {
            Some(ref dir) => dir.clone(),
            None => res.require::<Cwd>()?.0.clone(),
        };
        display_cwd = res.get::<DisplayCwd>().map(|d| d.0.clone());
        fs = res.require::<FileSystem>()?.0.clone();
        hints_enabled = res.get::<PathNotFoundHints>().is_some_and(|h| h.0);
    }
    let joined_path = resolve_model_path(&cwd, display_cwd.as_deref(), &input.path);
    let is_skill_markdown = is_skill_markdown(&joined_path);
    let policy_path = joined_path.clone();
    let (path, _unicode_note) = match crate::util::fs::try_canonicalize(&joined_path).await {
        Ok(p) => (p, None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match crate::util::try_resolve_unicode_filename(&joined_path).await {
                Some(m) => (m.resolved_path, Some(m.note)),
                None => (joined_path, None),
            }
        }
        Err(_) => (joined_path, None),
    };
    if let Err(error) =
        crate::types::memory_v2::validate_memory_v2_read(&resources, &policy_path).await
    {
        return Ok(ReadFileOutput::FileReadError(error));
    }
    let version = ReadFileVersion::from_contract(contract_version);
    let is_legacy = version.is_legacy();
    let skip_gitignore = is_legacy && versions::legacy_0_4_10::allows_gitignored_reads();
    if !skip_gitignore {
        let res = resources.lock().await;
        let respect_gitignore = res.get::<RespectGitignore>().is_some_and(|r| r.0);
        if respect_gitignore
            && let Some(filter) = res.get::<GitignoreFilter>()
            && filter.is_ignored(&path)
        {
            let display_dcwd = display_cwd_or_cwd(&cwd, display_cwd.as_deref());
            return Ok(ReadFileOutput::FileReadError(format!(
                "Error: {} is ignored by .gitignore and cannot be read.",
                display_dcwd.join(&input.path).display()
            )));
        }
    }
    let mut file_bytes = match fs.read_file(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!(?e, "Failed to read file");
            if is_legacy {
                return Ok(ReadFileOutput::FileReadError(
                    versions::legacy_0_4_10::render_read_error(&path),
                ));
            }
            let display_dcwd = display_cwd_or_cwd(&cwd, display_cwd.as_deref());
            let display_path = display_dcwd.join(&input.path);
            return Ok(match e.io_error_kind() {
                Some(std::io::ErrorKind::NotFound) => {
                    let skill_suggestion = {
                        let res = resources.lock().await;
                        res.get::<SkillManager>()
                            .and_then(|manager| manager.suggest_skill_path(&path))
                    };
                    let verified_skill_suggestion = if let Some(suggestion) = skill_suggestion
                        && fs.file_exists(&suggestion.path).await.unwrap_or(false)
                    {
                        Some(suggestion)
                    } else {
                        None
                    };
                    let mut msg = crate::util::format_not_found_error(
                        &display_path,
                        &path,
                        &cwd,
                        &display_dcwd,
                        hints_enabled && verified_skill_suggestion.is_none(),
                    )
                    .await;
                    if let Some(suggestion) = verified_skill_suggestion {
                        msg.push_str("\nThe skill you are looking for is registered at:\n");
                        msg.push_str(&suggestion.display_path.to_string_lossy());
                    }
                    ReadFileOutput::FileNotFound(msg)
                }
                Some(std::io::ErrorKind::IsADirectory) => ReadFileOutput::IsADirectory(format!(
                    "Error: {} is a directory, not a file.",
                    display_path.display()
                )),
                Some(std::io::ErrorKind::PermissionDenied) => ReadFileOutput::PermissionDenied(
                    format!("Permission denied: {}", display_path.display()),
                ),
                _ => ReadFileOutput::FileReadError(format!(
                    "Failed to read file: {}, {e}",
                    display_path.display()
                )),
            });
        }
    };
    if let Err(error) =
        crate::types::memory_v2::record_memory_v2_read(&resources, &policy_path, &file_bytes).await
    {
        return Ok(ReadFileOutput::FileReadError(error));
    }
    if let Ok(metadata) = bytes_to_metadata(&file_bytes)
        && metadata.is_image()
    {
        if crate::implementations::read_file::should_embed_as_conversation_image(
            &path,
            &file_bytes,
            &metadata.mime_type,
        ) {
            return Ok(crate::implementations::read_file::image::image_read_output(
                file_bytes,
                metadata.mime_type,
            )
            .await);
        }
        if let Some(svg_text) = crate::implementations::read_file::extract_svg_text(&file_bytes) {
            file_bytes = svg_text.into_bytes();
        } else {
            return Ok(
                ReadFileOutput::ImageSizeError(
                    "Could not embed image in conversation: SVG or incomplete PNG preview cannot be sent as an image"
                        .to_owned(),
                ),
            );
        }
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if is_pdf_file(&file_bytes, &extension) {
        let mut output =
            handle_pdf(file_bytes, &path, input.pages, input.format.as_deref()).await?;
        if let ReadFileOutput::FileContent(ref mut fc) = output {
            crate::implementations::cursor_rules_on_read::append_cursor_rules_for_read(
                cursor_rules_on_read_enabled(&resources).await,
                resources.clone(),
                &cwd,
                &path,
                &mut fc.content,
                &mut fc.content_concise,
            )
            .await;
        }
        return Ok(output);
    }
    if extension == "pptx" {
        return handle_pptx(file_bytes, &path).await;
    }
    if crate::util::binary::is_binary(&extension, &file_bytes) {
        tracing::info!(
            path = %path.display(),
            extension = %extension,
            detected_by = if crate::util::binary::BINARY_EXTENSIONS
                .binary_search(&extension.as_str()).is_ok() { "extension" } else { "content_inspection" },
            "binary file rejected by read_file"
        );
        return Ok(ReadFileOutput::FileReadError(format!(
            "Cannot read binary file: {}",
            path.display()
        )));
    }
    let file_content = String::from_utf8_lossy(&file_bytes).into_owned();
    if file_content.is_empty() {
        let stored_offset = stored_read_offset(input.offset);
        return Ok(ReadFileOutput::FileContent(FileContent {
            content: String::new(),
            content_concise: None,
            absolute_path: path,
            offset: stored_offset,
            limit: input.limit,
            raw_output: String::new(),
            total_lines: 0,
            extracted_images: Vec::new(),
        }));
    }
    let total_lines = file_content.matches('\n').count() + 1;
    let max_lines = {
        let res = resources.lock().await;
        res.get::<TruncationCfg>()
            .map(|t| t.0.max_lines_read())
            .unwrap_or_else(|| TruncationConfig::default().max_lines_read())
    };
    let skill_full = is_skill_markdown
        .then(|| extract_file_content_lines(&file_content, None, None, total_lines))
        .filter(|full| !exceeds_read_cap(&full.content));
    let windowed = skill_full.is_none();
    let (mut extracted, stored_offset, stored_limit) = match skill_full {
        Some(full) => (full, None, None),
        None => (
            extract_file_content_lines(
                &file_content,
                input.offset,
                Some(input.limit.unwrap_or(usize::MAX).min(max_lines)),
                total_lines,
            ),
            stored_read_offset(input.offset),
            input.limit,
        ),
    };
    if windowed && let Some(budget) = max_output_bytes(&resources).await {
        extracted = apply_byte_budget(
            extracted,
            budget,
            &file_content,
            input.offset,
            total_lines,
            invoking_param_names.resolve("offset"),
        );
    }
    if exceeds_read_cap(&extracted.content) {
        let token_count = crate::util::truncate::estimate_tokens(&extracted.content);
        let (grep_name, execute_name);
        {
            let res = resources.lock().await;
            let renderer = res.require::<TemplateRenderer>()?;
            grep_name = renderer
                .render("${{ tools.by_kind.search }}")
                .map_err(|e| distill_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;
            execute_name = renderer
                .render("${{ tools.by_kind.execute }}")
                .map_err(|e| distill_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;
        }
        let offset_param = invoking_param_names.resolve("offset");
        let limit_param = invoking_param_names.resolve("limit");
        let single_content_line = extracted.raw_output.lines().count() <= 1;
        let single_line_hint = if single_content_line && !execute_name.is_empty() {
            format!(
                "\nNote: the requested read is a single very long line, so \
                 line-based {offset_param}/{limit_param} cannot narrow it further. Use the \
                 '{execute_name}' tool to extract the parts you need (e.g. \
                 `jq`, `python3`, or `cut -c`)."
            )
        } else {
            String::new()
        };
        let range_specified = input.offset.is_some() || input.limit.is_some();
        let msg = if range_specified {
            let off = input
                .offset
                .map_or_else(|| "1".to_string(), |v| v.to_string());
            let lim = input
                .limit
                .map_or_else(|| "to end".to_string(), |v| v.to_string());
            format!(
                "The requested line range ({offset_param}={off}, {limit_param}={lim}) contains {token_count} tokens, \
                 which exceeds the maximum allowed tokens ({READ_FILE_MAX_TOKENS} tokens).\n\
                 Try a smaller `{limit_param}`, a different starting `{offset_param}`, \
                 or use the '{grep_name}' tool to search for specific content.{single_line_hint}"
            )
        } else {
            format!(
                "File content ({token_count} tokens) exceeds maximum allowed tokens ({READ_FILE_MAX_TOKENS} tokens).\n\
                 Please use {offset_param} and {limit_param} parameters to read a shorter range, \
                 or use the '{grep_name}' to search for specific content.{single_line_hint}"
            )
        };
        return Ok(ReadFileOutput::FileTooLarge(msg));
    }
    if let Some(flag) = streamable_out {
        *flag = true;
    }
    let mut content = extracted.content;
    let mut content_concise = Some(extracted.content_concise);
    let extracted_images = extracted.extracted_images;
    crate::implementations::cursor_rules_on_read::append_cursor_rules_for_read(
        cursor_rules_on_read_enabled(&resources).await,
        resources.clone(),
        &cwd,
        &path,
        &mut content,
        &mut content_concise,
    )
    .await;
    Ok(ReadFileOutput::FileContent(FileContent {
        content,
        content_concise,
        absolute_path: path,
        offset: stored_offset,
        limit: stored_limit,
        raw_output: extracted.raw_output,
        total_lines,
        extracted_images,
    }))
}
/// New-architecture `ReadFile` tool. Params: `()` — no per-tool configuration. Notifications: Emits
/// `FileRead` via `NotificationHandle`.
#[derive(Default, Debug)]
pub struct ReadFileTool;
impl crate::types::tool_metadata::ToolMetadata for ReadFileTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }
    fn description_template(&self) -> &str {
        DESCRIPTION_FULL
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}
impl distill_tool_runtime::Tool for ReadFileTool {
    type Args = ReadFileInput;
    type Output = ReadFileOutput;
    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("read_file").expect("valid tool id")
    }
    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            "read_file",
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }
    fn capabilities(&self) -> distill_tool_protocol::ToolCapabilities {
        READ_FILE_CAPABILITIES.clone()
    }
    /// Streaming entry point. Only the line-oriented text path streams: the final `content` is
    /// replayed as char-aligned deltas whose concatenation reproduces the card byte-for-byte;
    /// image/PDF/PPTX stay terminal-only. Gated by `WorkspaceViewerContext::stream_tool_progress`.
    async fn execute(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: ReadFileInput,
    ) -> distill_tool_runtime::ToolStream<ReadFileOutput> {
        let admitted_spec = ctx
            .get::<distill_tool_runtime::WorkspaceViewerContext>()
            .zip(READ_FILE_CAPABILITIES.streaming.as_ref())
            .filter(|(vctx, _)| vctx.stream_tool_progress)
            .map(|(_, spec)| spec);
        let Some(spec) = admitted_spec else {
            let this = ReadFileTool;
            return Box::pin(async_stream::stream! {
                yield distill_tool_runtime::ToolStreamItem::Terminal(this.run(ctx, input).await);
            });
        };
        Box::pin(async_stream::stream! {
            // `streamable` is call-local to this read.
            match ReadFileTool::read_with_streamability(&ctx, input).await {
                Ok((output, streamable)) => {
                    if streamable
                        && let ReadFileOutput::FileContent(fc) = &output
                        && !fc.content.is_empty()
                    {
                        // Replay char-aligned slices of the final `content`
                        // (each below the 16 KiB cap; see
                        // STREAM_DELTA_TARGET_BYTES).
                        let content = fc.content.as_bytes();
                        let mut last_total: u64 = 0;
                        let mut window_start = 0usize;
                        while window_start < content.len() {
                            let mut window_end =
                                (window_start + STREAM_DELTA_TARGET_BYTES).min(content.len());
                            // Align DOWN to a char boundary (a char is ≤ 4
                            // bytes vs the 4 KiB target: never a zero-width
                            // window).
                            while window_end > window_start
                                && !fc.content.is_char_boundary(window_end)
                            {
                                window_end -= 1;
                            }
                            if let Some(p) = distill_tool_runtime::stream_chunk(
                                spec,
                                content.get(..window_end).unwrap_or(&[]),
                                window_end as u64,
                                &mut last_total,
                                // Full replay, no streaming loss ⇒ never truncated.
                                false,
                            ) {
                                yield distill_tool_runtime::ToolStreamItem::Progress(p);
                            }
                            window_start = window_end;
                        }
                    }
                    yield distill_tool_runtime::ToolStreamItem::Terminal(Ok(output));
                }
                Err(e) => yield distill_tool_runtime::ToolStreamItem::Terminal(Err(e)),
            }
        })
    }
    #[tracing::instrument(name = "tool.read_file", skip_all, fields(path = %input.path))]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: ReadFileInput,
    ) -> Result<ReadFileOutput, distill_tool_runtime::ToolError> {
        Self::read_with_streamability(&ctx, input)
            .await
            .map(|(output, _streamable)| output)
    }
}
impl ReadFileTool {
    /// Shared body of `run` and `execute`: resolve context, run the read,
    /// apply read-lint tracking. `streamable` is a call-local flag, so
    /// concurrent reads cannot flip each other's value.
    async fn read_with_streamability(
        ctx: &distill_tool_runtime::ToolCallContext,
        input: ReadFileInput,
    ) -> Result<(ReadFileOutput, bool), distill_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(ctx)?;
        let cwd_override = ctx
            .extensions
            .get::<distill_tool_runtime::Cwd>()
            .map(|c| c.0.clone());
        let bv = crate::types::tool_metadata::behavior_version(ctx);
        let mut streamable_text = false;
        let invoking = crate::types::tool_metadata::invoking_param_names(ctx);
        let output = run_read_file(
            input,
            cwd_override.clone(),
            bv.as_deref(),
            resources.clone(),
            Some(&mut streamable_text),
            &invoking,
        )
        .await?;
        Ok((output, streamable_text))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::local::LocalFs;
    use crate::implementations::read_file::MAX_PDF_BYTES;
    use crate::implementations::read_file::compress_image_for_conversation;
    use crate::implementations::skills::types::SkillInfo;
    use crate::notification::types::ToolNotificationHandle;
    use crate::types::resources::{NotificationHandle, Resources};
    use crate::types::tool_metadata::test_ctx;
    use std::sync::Arc;
    use tempfile::TempDir;
    /// The cap is token-granular over bytes (not chars): text of exactly `READ_FILE_MAX_TOKENS` tokens
    /// fits, and it takes a whole extra token's worth of bytes to exceed it.
    #[test]
    fn exceeds_read_cap_is_token_granular() {
        let bytes_per_token = distill_token_estimation::BYTES_PER_TOKEN as usize;
        let cap_bytes = READ_FILE_MAX_TOKENS * bytes_per_token;
        assert!(!exceeds_read_cap(&"Q".repeat(cap_bytes)));
        assert!(!exceeds_read_cap(
            &"Q".repeat(cap_bytes + bytes_per_token - 1)
        ));
        assert!(exceeds_read_cap(&"Q".repeat(cap_bytes + bytes_per_token)));
        let multibyte = "路".repeat((cap_bytes + bytes_per_token).div_ceil("路".len()));
        assert!(multibyte.len() >= cap_bytes + bytes_per_token);
        assert!(multibyte.chars().count() < cap_bytes);
        assert!(exceeds_read_cap(&multibyte));
    }
    #[test]
    fn lines_within_byte_budget_keeps_whole_lines_that_fit() {
        let content = "1→aaaa\nbbbb\ncccc";
        assert_eq!(lines_within_byte_budget(content, 100), 3);
        assert_eq!(lines_within_byte_budget(content, 18), 3);
        assert_eq!(lines_within_byte_budget(content, 17), 2);
        assert_eq!(lines_within_byte_budget(content, 14), 2);
        assert_eq!(lines_within_byte_budget(content, 13), 1);
    }
    #[test]
    fn lines_within_byte_budget_always_keeps_first_line() {
        assert_eq!(
            lines_within_byte_budget("1→a very long first line\nb", 3),
            1
        );
        assert_eq!(lines_within_byte_budget("", 0), 1);
    }
    fn budget_resources(cwd: &std::path::Path, max_output_bytes: usize) -> Resources {
        let mut resources = test_resources(cwd);
        resources.insert(Params(ReadFileParams {
            max_output_bytes: Some(max_output_bytes),
            ..Default::default()
        }));
        resources
    }
    /// 200 lines of 10 visible chars each (~2.2 KB formatted): over the budgets used below, under the token cap.
    fn budget_fixture() -> String {
        (1..=200)
            .map(|i| format!("line{i:06}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[tokio::test]
    async fn max_output_bytes_truncates_to_whole_lines_with_marker() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("big.txt"), budget_fixture()).unwrap();
        let shared = budget_resources(tmp.path(), 500).into_shared();
        let input = ReadFileInput {
            path: "big.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let out = distill_tool_runtime::Tool::run(&ReadFileTool, test_ctx(shared), input)
            .await
            .unwrap();
        let ReadFileOutput::FileContent(fc) = out else {
            panic!("expected FileContent, got {out:?}");
        };
        let (body, marker) = fc
            .content
            .rsplit_once('\n')
            .expect("marker on its own line");
        assert!(body.len() <= 500, "body {} bytes over budget", body.len());
        assert!(
            body.ends_with(|c: char| c.is_ascii_digit()),
            "body: {body:?}"
        );
        let kept = body.lines().count();
        assert!(kept > 0 && kept < 200, "kept {kept}");
        assert!(marker.starts_with("... ["), "marker: {marker:?}");
        assert!(
            marker.ends_with(&format!(
                " characters truncated; file has 200 total lines; showing lines 1-{kept}; \
                 rerun with offset={}] ...",
                kept + 1
            )),
            "marker: {marker:?}"
        );
        assert_eq!(fc.content_concise.as_deref(), Some(fc.content.as_str()));
        assert_eq!(fc.raw_output.lines().count(), kept);
        assert_eq!(fc.total_lines, 200);
    }
    #[tokio::test]
    async fn max_output_bytes_marker_uses_requested_offset_and_renamed_param() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("big.txt"), budget_fixture()).unwrap();
        let resources = budget_resources(tmp.path(), 100);
        let mut ctx = test_ctx(resources.into_shared());
        ctx.extensions
            .insert(crate::types::resources::InvokingToolParamNames(
                [("offset".to_string(), "start_line".to_string())].into(),
            ));
        let input = ReadFileInput {
            path: "big.txt".to_string(),
            offset: Some(50),
            limit: None,
            pages: None,
            format: None,
        };
        let out = distill_tool_runtime::Tool::run(&ReadFileTool, ctx, input)
            .await
            .unwrap();
        let ReadFileOutput::FileContent(fc) = out else {
            panic!("expected FileContent, got {out:?}");
        };
        assert!(fc.content.starts_with("50→line000050\n"), "{}", fc.content);
        let kept = fc.content.lines().count() - 1;
        assert!(fc.content.contains(&format!(
            "showing lines 50-{}; rerun with start_line={}] ...",
            50 + kept - 1,
            50 + kept
        )));
    }
    #[tokio::test]
    async fn max_output_bytes_is_a_no_op_when_window_fits() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("small.txt"), "a\nb\nc\n").unwrap();
        let shared = budget_resources(tmp.path(), 5_000).into_shared();
        let input = ReadFileInput {
            path: "small.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let out = distill_tool_runtime::Tool::run(&ReadFileTool, test_ctx(shared), input)
            .await
            .unwrap();
        let ReadFileOutput::FileContent(fc) = out else {
            panic!("expected FileContent, got {out:?}");
        };
        assert_eq!(fc.content, "1→a\nb\nc\n");
        assert!(!fc.content.contains("truncated"));
    }
    #[tokio::test]
    async fn max_output_bytes_exempts_skill_markdown() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("SKILL.md"), budget_fixture()).unwrap();
        let shared = budget_resources(tmp.path(), 100).into_shared();
        let input = ReadFileInput {
            path: "SKILL.md".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let out = distill_tool_runtime::Tool::run(&ReadFileTool, test_ctx(shared), input)
            .await
            .unwrap();
        let ReadFileOutput::FileContent(fc) = out else {
            panic!("expected FileContent, got {out:?}");
        };
        assert!(fc.content.contains("line000200"));
        assert!(!fc.content.contains("truncated"));
    }
    /// Set up Resources with real filesystem for tests.
    fn test_resources(cwd: &std::path::Path) -> Resources {
        let mut resources = Resources::new();
        resources.insert(Cwd(cwd.to_path_buf()));
        resources.insert(FileSystem(Arc::new(LocalFs)));
        resources.insert(NotificationHandle(ToolNotificationHandle::noop()));
        resources
    }
    #[tokio::test]
    async fn read_file_basic() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "test.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let shared = resources.into_shared();
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(shared.clone()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(content.content.contains("line1"));
                assert!(content.content.contains("line2"));
                assert!(content.content.contains("line3"));
                assert!(content.raw_output.contains("line1"));
                assert_eq!(content.total_lines, 4);
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn legacy_read_file_not_found_returns_exact_historical_message() {
        let tmp = TempDir::new().unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "nonexistent.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let mut ctx = test_ctx(resources.into_shared());
        ctx.extensions.insert(distill_tool_runtime::BehaviorVersion(
            "legacy-0.4.10".to_string(),
        ));
        let result = distill_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        let expected = format!(
            "Failed to read file: {}",
            tmp.path().join("nonexistent.txt").display()
        );
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert_eq!(msg, expected);
            }
            other => panic!("Expected legacy FileReadError, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn current_read_file_not_found_returns_structured_not_found() {
        let tmp = TempDir::new().unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "nonexistent.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileNotFound(msg) => {
                assert!(msg.contains("does not exist"), "got: {msg}");
            }
            other => panic!("Expected FileNotFound, got {:?}", other),
        }
    }
    fn seeded_manager(skills: Vec<SkillInfo>) -> SkillManager {
        let mut manager = SkillManager::new();
        manager.seed(None, None, skills, None, None, None);
        manager
    }
    async fn not_found_msg(resources: Resources, path: &str) -> String {
        let input = ReadFileInput {
            path: path.to_owned(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result =
            distill_tool_runtime::Tool::run(&ReadFileTool, test_ctx(resources.into_shared()), input)
                .await
                .unwrap();
        match result {
            ReadFileOutput::FileNotFound(msg) => msg,
            other => panic!("Expected FileNotFound, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn missing_skill_read_suggests_registered_path() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".grok/skills/code-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "# Code review\n").unwrap();
        let mut resources = test_resources(tmp.path());
        resources.insert(PathNotFoundHints(true));
        resources.insert(seeded_manager(vec![SkillInfo {
            name: "code-review".to_owned(),
            path: skill_path.to_string_lossy().into_owned(),
            disable_model_invocation: true,
            ..SkillInfo::default()
        }]));
        let msg = not_found_msg(resources, "/wrong/root/skills/code-review/SKILL.md").await;
        assert_eq!(
            msg,
            format!(
                "Error: /wrong/root/skills/code-review/SKILL.md does not exist.\n\
                 The skill you are looking for is registered at:\n{}",
                skill_path.display()
            )
        );
    }
    #[tokio::test]
    async fn missing_skill_read_uses_display_path() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".grok/skills/review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "# Review\n").unwrap();
        let mut resources = test_resources(tmp.path());
        resources.insert(DisplayCwd(std::path::PathBuf::from("/display/project")));
        let mut manager = SkillManager::new();
        manager.seed(
            Some(tmp.path().to_path_buf()),
            None,
            vec![SkillInfo {
                name: "review".to_owned(),
                path: skill_path.to_string_lossy().into_owned(),
                ..SkillInfo::default()
            }],
            Some("/display/project".to_owned()),
            None,
            None,
        );
        resources.insert(manager);
        let msg = not_found_msg(resources, "/wrong/root/review/SKILL.md").await;
        assert_eq!(
            msg,
            "Error: /wrong/root/review/SKILL.md does not exist.\n\
             The skill you are looking for is registered at:\n\
             /display/project/.grok/skills/review/SKILL.md"
        );
    }
    #[tokio::test]
    async fn missing_skill_read_omits_ambiguous_suggestion() {
        let tmp = TempDir::new().unwrap();
        let first = tmp.path().join("first/review/SKILL.md");
        let second = tmp.path().join("second/review/SKILL.md");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, "# First\n").unwrap();
        std::fs::write(&second, "# Second\n").unwrap();
        let mut resources = test_resources(tmp.path());
        resources.insert(PathNotFoundHints(true));
        resources.insert(seeded_manager(vec![
            SkillInfo {
                name: "review".to_owned(),
                path: first.to_string_lossy().into_owned(),
                ..SkillInfo::default()
            },
            SkillInfo {
                name: "review".to_owned(),
                path: second.to_string_lossy().into_owned(),
                ..SkillInfo::default()
            },
        ]));
        let msg = not_found_msg(resources, "/wrong/root/review/SKILL.md").await;
        assert_eq!(
            msg,
            format!(
                "Error: /wrong/root/review/SKILL.md does not exist.\n\
                 Note: your current working directory is {}",
                tmp.path().display()
            )
        );
    }
    #[tokio::test]
    async fn missing_skill_read_omits_stale_registered_path() {
        let tmp = TempDir::new().unwrap();
        let stale_path = tmp.path().join(".grok/skills/review/SKILL.md");
        let mut resources = test_resources(tmp.path());
        resources.insert(PathNotFoundHints(true));
        resources.insert(seeded_manager(vec![SkillInfo {
            name: "review".to_owned(),
            path: stale_path.to_string_lossy().into_owned(),
            ..SkillInfo::default()
        }]));
        let msg = not_found_msg(resources, "/wrong/root/review/SKILL.md").await;
        assert_eq!(
            msg,
            format!(
                "Error: /wrong/root/review/SKILL.md does not exist.\n\
                 Note: your current working directory is {}",
                tmp.path().display()
            )
        );
    }
    #[tokio::test]
    async fn legacy_read_file_directory_returns_exact_historical_message() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "subdir".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let mut ctx = test_ctx(resources.into_shared());
        ctx.extensions.insert(distill_tool_runtime::BehaviorVersion(
            "legacy-0.4.10".to_string(),
        ));
        let result = distill_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        let expected_path = dunce::canonicalize(tmp.path().join("subdir"))
            .unwrap_or_else(|_| tmp.path().join("subdir"));
        let expected = format!("Failed to read file: {}", expected_path.display());
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert_eq!(msg, expected);
            }
            other => panic!("Expected legacy FileReadError, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn current_read_file_is_directory_returns_structured_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "subdir".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::IsADirectory(msg) => {
                assert!(msg.contains("is a directory, not a file"), "got: {msg}");
            }
            other => panic!("Expected IsADirectory, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_empty() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "empty.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert_eq!(content.content, "");
                assert!(content.content_concise.is_none());
                assert_eq!(content.raw_output, "");
                assert_eq!(content.total_lines, 0);
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_with_offset_and_limit() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("multi.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "multi.txt".to_string(),
            offset: Some(2),
            limit: Some(2),
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(content.content.contains("2"));
                assert!(content.content.contains("3"));
                assert_eq!(content.offset, Some(2));
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("absolute.txt");
        std::fs::write(&file_path, "content").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: file_path.to_string_lossy().to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(content.raw_output.contains("content"));
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "hello\n").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "hello.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert_eq!(content.content, "1→hello\n");
                assert_eq!(content.total_lines, 2);
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_concise_output() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello\nworld\n").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "test.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                let concise = content.content_concise.unwrap();
                assert_eq!(concise, "1→hello\nworld\n");
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn token_limit_error_references_grep() {
        let tmp = TempDir::new().unwrap();
        let line = "x".repeat(200);
        let big_content = std::iter::repeat_n(line.as_str(), 1100)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("big.txt"), &big_content).unwrap();
        let tool = ReadFileTool;
        let mut resources = test_resources(tmp.path());
        resources.insert(TemplateRenderer::new(
            [(ToolKind::Search, "Grep".to_string())].into(),
            Default::default(),
        ));
        let input = ReadFileInput {
            path: "big.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileTooLarge(msg) => {
                assert!(msg.contains("exceeds maximum allowed tokens"));
                assert!(
                    msg.contains("Grep"),
                    "Error should reference renamed grep tool: {}",
                    msg
                );
            }
            other => panic!("Expected FileTooLarge, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn token_limit_error_when_range_specified_gives_better_message() {
        let tmp = TempDir::new().unwrap();
        let line = "x".repeat(200);
        let big_content = std::iter::repeat_n(line.as_str(), 1100)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("big.txt"), &big_content).unwrap();
        let tool = ReadFileTool;
        let mut resources = test_resources(tmp.path());
        resources.insert(TemplateRenderer::new(
            [(ToolKind::Search, "Grep".to_string())].into(),
            Default::default(),
        ));
        let input = ReadFileInput {
            path: "big.txt".to_string(),
            offset: Some(1),
            limit: Some(800),
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileTooLarge(msg) => {
                assert!(msg.contains("requested line range"));
                assert!(msg.contains("offset=1"));
                assert!(msg.contains("limit=800"));
                assert!(msg.contains("exceeds the maximum allowed tokens"));
                assert!(
                    msg.contains("Grep"),
                    "Error should still reference the grep tool: {}",
                    msg
                );
                assert!(!msg.contains("Please use offset and limit parameters"));
            }
            other => panic!("Expected FileTooLarge, got {:?}", other),
        }
    }
    /// Regression: FileTooLarge must name *this* tool's schema keys, not
    /// whatever a sibling Read tool last wrote into the kind-wide param map.
    #[tokio::test]
    async fn token_limit_error_uses_invoking_tool_param_names_not_kind_wide() {
        let tmp = TempDir::new().unwrap();
        let line = "x".repeat(200);
        let big_content = std::iter::repeat_n(line.as_str(), 1100)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("big.txt"), &big_content).unwrap();
        let tool = ReadFileTool;
        let mut resources = test_resources(tmp.path());
        resources.insert(TemplateRenderer::new(
            [(ToolKind::Search, "Grep".to_string())].into(),
            [(
                ToolKind::Read,
                [
                    ("offset".to_string(), "poisoned_offset".to_string()),
                    ("limit".to_string(), "poisoned_limit".to_string()),
                ]
                .into(),
            )]
            .into(),
        ));
        let input = ReadFileInput {
            path: "big.txt".to_string(),
            offset: Some(1),
            limit: Some(800),
            pages: None,
            format: None,
        };
        let mut ctx = test_ctx(resources.into_shared());
        ctx.extensions
            .insert(crate::types::resources::InvokingToolParamNames(
                [
                    ("offset".to_string(), "start_line".to_string()),
                    ("limit".to_string(), "max_lines".to_string()),
                ]
                .into(),
            ));
        let result = distill_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileTooLarge(msg) => {
                assert!(
                    msg.contains("start_line=1") && msg.contains("max_lines=800"),
                    "expected invoking-tool names, got: {msg}"
                );
                assert!(
                    !msg.contains("poisoned_offset") && !msg.contains("poisoned_limit"),
                    "must not use kind-wide sibling renames: {msg}"
                );
            }
            other => panic!("Expected FileTooLarge, got {:?}", other),
        }
    }
    #[test]
    fn test_extract_file_content_lines_basic() {
        let extracted = extract_file_content_lines("1\n2\r\n3\n", None, None, 4);
        assert_eq!(extracted.content, "1→1\n2\n3\n");
        assert_eq!(extracted.content_concise, "1→1\n2\n3\n");
        assert_eq!(extracted.raw_output, "1\n2\r\n3\n");
    }
    /// Regression: a long single-line base64 URI used to be cut mid-payload by the (since-removed)
    /// per-line clip and re-emitted as a corrupt vision token. Pin that the full payload is
    /// captured byte-equal.
    #[test]
    fn extract_captures_long_inline_base64_image_before_truncation() {
        let payload = "A".repeat(50_000);
        let file_content = format!("# README\n![logo](data:image/png;base64,{payload})\n");
        let total_lines = file_content.matches('\n').count() + 1;
        let extracted = extract_file_content_lines(&file_content, None, None, total_lines);
        let Some(image) = extracted.extracted_images.first() else {
            panic!(
                "expected one extracted image: {:?}",
                extracted.extracted_images
            );
        };
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, payload);
        assert!(
            extracted
                .content
                .contains("[image content will be provided separately]"),
            "expected capture placeholder; got: {}",
            crate::util::truncate_str(&extracted.content, 300)
        );
        assert!(
            !extracted.content.contains("AAAAAAAAAAAA"),
            "raw base64 must not survive into model-visible content"
        );
        assert!(
            !extracted.content.contains("[... truncated ("),
            "captured line must be short enough to skip truncation"
        );
        assert!(extracted.content.contains("# README"));
        assert!(extracted.content.contains("![logo]("));
    }
    /// URIs under the truncation threshold are still captured — pins
    /// uniform "embedded images become vision tokens" regardless of
    /// line length.
    #[test]
    fn extract_captures_short_inline_base64_image_without_truncation_pressure() {
        let payload = "A".repeat(1500);
        let file_content = format!("inline data:image/png;base64,{payload} done\n");
        let total_lines = file_content.matches('\n').count() + 1;
        let extracted = extract_file_content_lines(&file_content, None, None, total_lines);
        let Some(image) = extracted.extracted_images.first() else {
            panic!(
                "expected one extracted image: {:?}",
                extracted.extracted_images
            );
        };
        assert_eq!(image.data, payload);
        assert!(
            extracted
                .content
                .contains("[image content will be provided separately]")
        );
        assert!(!extracted.content.contains(&payload));
    }
    /// No-op fast path: ordinary file content round-trips byte-equal.
    #[test]
    fn extract_leaves_non_data_uri_lines_untouched() {
        let file_content = "fn main() {\n    println!(\"hello\");\n}\n";
        let total_lines = file_content.matches('\n').count() + 1;
        let extracted = extract_file_content_lines(file_content, None, None, total_lines);
        assert_eq!(
            extracted.content,
            "1→fn main() {\n    println!(\"hello\");\n}\n"
        );
        assert!(extracted.extracted_images.is_empty());
    }
    #[test]
    fn test_extract_file_content_lines_with_offset() {
        let extracted = extract_file_content_lines("1\n2\n3\r\n4\r", Some(3), None, 4);
        assert_eq!(extracted.content, "3→3\n4\r".to_owned());
        assert_eq!(extracted.content_concise, "3→3\n4\r".to_owned());
        assert_eq!(extracted.raw_output, "3\r\n4\r".to_owned());
    }
    #[test]
    fn test_extract_file_content_lines_with_offset_and_limit() {
        let extracted = extract_file_content_lines("1\n2\n3\r\n4\r", Some(2), Some(2), 4);
        assert_eq!(extracted.content, "2→2\n3".to_owned());
        assert_eq!(extracted.content_concise, "2→2\n3".to_owned());
        assert_eq!(extracted.raw_output, "2\n3\n".to_owned());
    }
    #[test]
    fn test_is_image_returns_true_for_image_mime_types() {
        let image_mime_types = [
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "image/svg+xml",
            "image/bmp",
            "image/tiff",
        ];
        for mime_type in image_mime_types {
            let metadata = FileMetadata {
                size: 1024,
                mime_type: mime_type.to_string(),
            };
            assert!(
                metadata.is_image(),
                "Expected is_image() to return true for mime type: {}",
                mime_type
            );
        }
    }
    #[test]
    fn test_is_image_returns_false_for_non_image_mime_types() {
        let non_image_mime_types = [
            "text/plain",
            "text/html",
            "application/json",
            "application/pdf",
            "application/octet-stream",
            "video/mp4",
            "audio/mpeg",
        ];
        for mime_type in non_image_mime_types {
            let metadata = FileMetadata {
                size: 1024,
                mime_type: mime_type.to_string(),
            };
            assert!(
                !metadata.is_image(),
                "Expected is_image() to return false for mime type: {}",
                mime_type
            );
        }
    }
    #[test]
    fn test_is_image_with_empty_mime_type() {
        let metadata = FileMetadata {
            size: 0,
            mime_type: "".to_string(),
        };
        assert!(!metadata.is_image());
    }
    #[tokio::test]
    async fn read_file_adobe_svg_with_truncated_png_prefix_reads_as_text() {
        let tmp = TempDir::new().unwrap();
        let bytes = crate::implementations::read_file::metadata::truncated_png_then_svg();
        std::fs::write(tmp.path().join("baidu.svg"), &bytes).unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "baidu.svg".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(
                    content.raw_output.contains("<svg"),
                    "must read SVG markup, not embed the thumbnail: {content:?}"
                );
                assert!(content.raw_output.contains("baidu"));
                assert!(
                    !content.raw_output.contains('\u{FFFD}'),
                    "PNG prefix must not leak into the text read"
                );
            }
            ReadFileOutput::ImageContent(_) => {
                panic!("Adobe SVG must not take the PNG image-embed path")
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn read_file_truncated_png_prefix_svg_without_markup_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let png = crate::implementations::read_file::metadata::truncated_png_prefix();
        std::fs::write(tmp.path().join("preview.svg"), &png).unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "preview.svg".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::ImageSizeError(msg) => {
                assert!(msg.contains("incomplete PNG") || msg.contains("SVG"));
            }
            ReadFileOutput::ImageContent(_) => {
                panic!("incomplete PNG preview on .svg must not embed")
            }
            other => panic!("expected ImageSizeError, got {other:?}"),
        }
    }
    fn build_gitignore(root: &std::path::Path, patterns: &[&str]) -> ignore::gitignore::Gitignore {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        for pattern in patterns {
            builder.add_line(None, pattern).unwrap();
        }
        builder.build().unwrap()
    }
    fn test_resources_with_gitignore(cwd: &std::path::Path) -> Resources {
        let mut resources = test_resources(cwd);
        let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let gi = build_gitignore(&canonical, &["build/", "node_modules/", "*.log"]);
        resources.insert(GitignoreFilter::new(gi, canonical));
        resources
    }
    #[tokio::test]
    async fn read_file_allows_gitignored_files_by_default() {
        let tmp = TempDir::new().unwrap();
        let canonical_root = dunce::canonicalize(tmp.path()).unwrap();
        let build_dir = canonical_root.join("build");
        std::fs::create_dir(&build_dir).unwrap();
        std::fs::write(
            build_dir.join("output.txt"),
            "text data from gitignored build/ dir",
        )
        .unwrap();
        let tool = ReadFileTool;
        let resources = test_resources_with_gitignore(tmp.path());
        let input = ReadFileInput {
            path: "build/output.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(
                    content
                        .raw_output
                        .contains("text data from gitignored build/ dir")
                );
            }
            other => {
                panic!(
                    "Expected FileContent for gitignored file (read_file allows by default), got {:?}",
                    other
                )
            }
        }
    }
    #[tokio::test]
    async fn read_file_blocked_when_respect_gitignore_enabled() {
        let tmp = TempDir::new().unwrap();
        let canonical_root = dunce::canonicalize(tmp.path()).unwrap();
        let build_dir = canonical_root.join("build");
        std::fs::create_dir(&build_dir).unwrap();
        std::fs::write(build_dir.join("output.o"), "binary data").unwrap();
        let tool = ReadFileTool;
        let mut resources = test_resources_with_gitignore(tmp.path());
        resources.insert(RespectGitignore(true));
        let input = ReadFileInput {
            path: "build/output.o".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert!(
                    msg.contains("ignored by .gitignore"),
                    "Error should mention .gitignore: {}",
                    msg
                );
            }
            other => {
                panic!(
                    "Expected FileReadError for gitignored file with RespectGitignore(true), got {:?}",
                    other
                )
            }
        }
    }
    #[tokio::test]
    async fn legacy_read_file_allows_gitignored_files() {
        let tmp = TempDir::new().unwrap();
        let canonical_root = dunce::canonicalize(tmp.path()).unwrap();
        let build_dir = canonical_root.join("build");
        std::fs::create_dir(&build_dir).unwrap();
        std::fs::write(build_dir.join("output.txt"), "build output data\n").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources_with_gitignore(tmp.path());
        let input = ReadFileInput {
            path: "build/output.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let mut ctx = test_ctx(resources.into_shared());
        ctx.extensions.insert(distill_tool_runtime::BehaviorVersion(
            "legacy-0.4.10".to_string(),
        ));
        let result = distill_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(content.raw_output.contains("build output data"));
            }
            other => {
                panic!(
                    "Expected FileContent for legacy gitignored file, got {:?}",
                    other
                )
            }
        }
    }
    #[tokio::test]
    async fn read_file_allowed_when_not_gitignored() {
        let tmp = TempDir::new().unwrap();
        let canonical_root = dunce::canonicalize(tmp.path()).unwrap();
        let src_dir = canonical_root.join("src");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}\n").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources_with_gitignore(tmp.path());
        let input = ReadFileInput {
            path: "src/main.rs".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(content.raw_output.contains("fn main()"));
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_allows_gitignored_by_extension() {
        let tmp = TempDir::new().unwrap();
        let canonical_root = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::write(
            canonical_root.join("debug.log"),
            "log data for read_file test\n",
        )
        .unwrap();
        let tool = ReadFileTool;
        let resources = test_resources_with_gitignore(tmp.path());
        let input = ReadFileInput {
            path: "debug.log".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert!(content.raw_output.contains("log data for read_file test"));
            }
            other => {
                panic!(
                    "Expected FileContent for gitignored *.log (read_file allows), got {:?}",
                    other
                )
            }
        }
    }
    #[tokio::test]
    async fn read_file_no_gitignore_filter_allows_all() {
        let tmp = TempDir::new().unwrap();
        let build_dir = tmp.path().join("build");
        std::fs::create_dir(&build_dir).unwrap();
        std::fs::write(build_dir.join("output.txt"), "data").unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "build/output.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(_) => {}
            other => {
                panic!(
                    "Expected FileContent when no gitignore filter, got {:?}",
                    other
                )
            }
        }
    }
    #[test]
    fn extract_file_content_lines_full_file() {
        let file_content = "use std::io;\n\nfn main() {\n    println!(\"Hello, world!\");\n}\n";
        let total_lines = file_content.matches('\n').count() + 1;
        let extracted = extract_file_content_lines(file_content, None, None, total_lines);
        assert_eq!(
            extracted.content,
            "1→use std::io;\n\nfn main() {\n    println!(\"Hello, world!\");\n}\n"
        );
    }
    #[test]
    fn extract_file_content_lines_with_offset_and_limit() {
        let file_content = "line1\nline2\nline3\nline4\nline5\n";
        let total_lines = 6;
        let extracted = extract_file_content_lines(file_content, Some(2), Some(2), total_lines);
        assert_eq!(extracted.content, "2→line2\nline3");
    }
    #[test]
    fn extract_file_content_lines_compaction_reread_scenario() {
        let file_content = "\
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

impl Config {
    pub fn new(name: &str, port: u16) -> Self {
        Self {
            name: name.to_string(),
            port,
        }
    }
}
";
        let total_lines = file_content.matches('\n').count() + 1;
        let max_reread_lines = 2_000;
        let effective_limit = Some(max_reread_lines);
        let extracted =
            extract_file_content_lines(file_content, None, effective_limit, total_lines);
        let expected = "\
1→use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

impl Config {
10\u{2192}    pub fn new(name: &str, port: u16) -> Self {
        Self {
            name: name.to_string(),
            port,
        }
    }
}
";
        assert_eq!(extracted.content, expected);
        let truncated = effective_limit.map(|l| l < total_lines).unwrap_or(false);
        assert!(
            !truncated,
            "17-line file should not be truncated at 2000-line limit"
        );
    }
    /// End-to-end test: write a real file to disk, read it back, format it
    /// with extract_file_content_lines — exercising the exact same pipeline
    /// that reread_file_for_compaction uses in production.
    #[tokio::test]
    async fn reread_file_from_disk_for_compaction() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("src/auth.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        let source_code = "\
use jwt::Claims;
use actix_web::HttpRequest;

pub fn verify(req: &HttpRequest) -> Result<Claims, Error> {
    let token = req.headers()
        .get(\"Authorization\")
        .ok_or(Error::Missing)?
        .to_str()
        .map_err(|_| Error::Invalid)?;
    jwt::decode(token)
}
";
        std::fs::write(&file_path, source_code).unwrap();
        let file_bytes = tokio::fs::read(&file_path).await.unwrap();
        let max_reread_bytes: usize = 10_485_760;
        assert!(
            file_bytes.len() <= max_reread_bytes,
            "test file should be under the byte threshold"
        );
        let file_content = String::from_utf8_lossy(&file_bytes).into_owned();
        let total_lines = file_content.matches('\n').count() + 1;
        assert_eq!(
            total_lines, 12,
            "source_code should have 12 lines (11 content + trailing empty)"
        );
        let max_reread_lines: usize = 2_000;
        let effective_limit = Some(max_reread_lines);
        let extracted =
            extract_file_content_lines(&file_content, None, effective_limit, total_lines);
        let expected = "\
1→use jwt::Claims;
use actix_web::HttpRequest;

pub fn verify(req: &HttpRequest) -> Result<Claims, Error> {
    let token = req.headers()
        .get(\"Authorization\")
        .ok_or(Error::Missing)?
        .to_str()
        .map_err(|_| Error::Invalid)?;
10\u{2192}    jwt::decode(token)
}
";
        assert_eq!(extracted.content, expected);
        let truncated = effective_limit.map(|l| l < total_lines).unwrap_or(false);
        assert!(
            !truncated,
            "12-line file should not be truncated at 2000-line limit"
        );
        assert_eq!(extracted.raw_output, source_code);
    }
    /// Same as above but with offset+limit to simulate a partial re-read
    /// (e.g. the model had only read lines 4-8 before compaction).
    #[tokio::test]
    async fn reread_file_from_disk_partial_range() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("big_module.rs");
        let source = "line_one\nline_two\nline_three\nline_four\nline_five\n\
                       line_six\nline_seven\nline_eight\nline_nine\nline_ten\n";
        std::fs::write(&file_path, source).unwrap();
        let file_bytes = tokio::fs::read(&file_path).await.unwrap();
        let file_content = String::from_utf8_lossy(&file_bytes).into_owned();
        let total_lines = file_content.matches('\n').count() + 1;
        assert_eq!(total_lines, 11);
        let offset = Some(4);
        let limit = 3_usize;
        let max_reread_lines = 2_000_usize;
        let effective_limit = Some(limit.min(max_reread_lines));
        let extracted =
            extract_file_content_lines(&file_content, offset, effective_limit, total_lines);
        assert_eq!(extracted.content, "4→line_four\nline_five\nline_six");
        let truncated = effective_limit.map(|l| l < total_lines).unwrap_or(false);
        assert!(
            truncated,
            "3-line window on 11-line file should be truncated"
        );
    }
    use crate::implementations::read_file::image::{
        MAX_IMAGE_DIMENSION, MAX_IMAGE_PAYLOAD_BYTES, MAX_IMAGE_RAW_BYTES,
    };
    /// Creates a PNG with pseudo-random pixel data so it doesn't compress
    /// into a trivially small file (unlike a solid-color or gradient image).
    fn make_noisy_png(width: u32, height: u32) -> Vec<u8> {
        use image::{ImageBuffer, Rgba};
        let img = ImageBuffer::from_fn(width, height, |x, y| {
            let seed = (x as u64).wrapping_mul(6364136223846793005)
                ^ (y as u64).wrapping_mul(1442695040888963407);
            let r = (seed & 0xFF) as u8;
            let g = ((seed >> 8) & 0xFF) as u8;
            let b = ((seed >> 16) & 0xFF) as u8;
            Rgba([r, g, b, 255u8])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }
    #[test]
    fn compress_flat_color_picks_png() {
        use image::{ImageBuffer, Rgba};
        let img = ImageBuffer::from_pixel(1024u32, 768, Rgba([40u8, 80, 120, 255]));
        let mut png_buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png_buf, image::ImageFormat::Png).unwrap();
        let mut raw = png_buf.into_inner();
        let target_raw = MAX_IMAGE_RAW_BYTES + 1;
        if raw.len() < target_raw {
            raw.resize(target_raw, 0xAA);
        }
        let b64_size = (raw.len() * 4).div_ceil(3);
        assert!(
            b64_size > MAX_IMAGE_PAYLOAD_BYTES,
            "test image ({b64_size} B b64) must exceed the payload limit"
        );
        let (result, mime) = compress_image_for_conversation(raw, "image/png".into()).unwrap();
        assert_eq!(
            mime, "image/png",
            "flat-color image should pick PNG over JPEG"
        );
        let b64_after = (result.len() * 4).div_ceil(3);
        assert!(
            b64_after <= MAX_IMAGE_PAYLOAD_BYTES,
            "compressed image ({b64_after} B b64) must fit within {MAX_IMAGE_PAYLOAD_BYTES} B"
        );
    }
    #[test]
    fn compress_oversized_image_preserves_aspect_ratio() {
        let png = make_noisy_png(3000, 2000);
        let (result, mime) = compress_image_for_conversation(png, "image/png".into()).unwrap();
        assert_eq!(mime, "image/jpeg");
        let decoded = image::load_from_memory(&result).unwrap();
        assert!(decoded.width() <= MAX_IMAGE_DIMENSION);
        assert!(decoded.height() <= MAX_IMAGE_DIMENSION);
        let ratio = decoded.width() as f64 / decoded.height() as f64;
        assert!(
            (ratio - 1.5).abs() < 0.05,
            "aspect ratio {ratio:.3} should be ~1.5 (3:2), not 1.0 (square)"
        );
    }
    /// Fail closed: bytes that can't be identified as an endpoint format
    /// must never embed raw (the API rejects everything but JPEG/PNG/WebP/
    /// ICO, and a poisoned tool result 400s every subsequent turn).
    #[test]
    fn compress_undecodable_format_fails_closed() {
        use crate::implementations::read_file::image::CompressImageError;
        let garbage = b"not a real image format at all";
        let err = compress_image_for_conversation(garbage.to_vec(), "image/svg+xml".into())
            .expect_err("unsniffable bytes must not pass through");
        assert!(
            matches!(err, CompressImageError::FormatDetectionFailed),
            "got: {err:?}"
        );
    }
    #[test]
    fn compress_output_never_exceeds_payload_limit() {
        let png = make_noisy_png(4096, 3072);
        match compress_image_for_conversation(png, "image/png".into()) {
            Ok((buf, mime)) => {
                assert_eq!(mime, "image/jpeg");
                let b64_len = (buf.len() * 4).div_ceil(3);
                assert!(
                    b64_len <= MAX_IMAGE_PAYLOAD_BYTES,
                    "JPEG output ({b64_len} B b64) must fit within {MAX_IMAGE_PAYLOAD_BYTES} B"
                );
            }
            Err(_) => {}
        }
    }
    /// Wrapper-level user-visible message: prefix matches the caller's
    /// `"Could not embed image in conversation: ..."` and the legacy
    /// "Image too large to embed..." is no longer used.
    #[test]
    fn compress_oversized_garbage_user_message_is_non_legacy() {
        let bytes = vec![0u8; MAX_IMAGE_PAYLOAD_BYTES + 4096];
        use crate::implementations::read_file::image::CompressImageError;
        let err = compress_image_for_conversation(bytes, "image/png".into()).unwrap_err();
        let rendered = format!("Could not embed image in conversation: {err}");
        assert!(rendered.contains("format"));
        assert!(!rendered.starts_with("Image too large to embed"));
        assert!(
            matches!(err, CompressImageError::FormatDetectionFailed),
            "got: {err:?}"
        );
    }
    #[tokio::test]
    async fn skill_file_ignores_offset_and_limit() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".grok/skills/commit");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: ".grok/skills/commit/SKILL.md".to_string(),
            offset: Some(3),
            limit: Some(1),
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(fc) => {
                assert!(
                    fc.content.contains("line1"),
                    "missing line1: {}",
                    fc.content
                );
                assert!(
                    fc.content.contains("line5"),
                    "missing line5: {}",
                    fc.content
                );
                assert_eq!(fc.offset, None);
                assert_eq!(fc.limit, None);
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    fn write_oversized_skill(tmp: &TempDir) {
        let line = "x".repeat(200);
        let big_content = std::iter::repeat_n(line.as_str(), 1100)
            .collect::<Vec<_>>()
            .join("\n");
        let skill_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), &big_content).unwrap();
    }
    #[tokio::test]
    async fn skill_file_over_cap_returns_file_too_large() {
        let tmp = TempDir::new().unwrap();
        write_oversized_skill(&tmp);
        let tool = ReadFileTool;
        let mut resources = test_resources(tmp.path());
        resources.insert(TemplateRenderer::new(
            [(ToolKind::Search, "grep".to_string())].into(),
            Default::default(),
        ));
        let input = ReadFileInput {
            path: "skills/SKILL.md".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        assert!(
            matches!(result, ReadFileOutput::FileTooLarge(_)),
            "got {:?}",
            std::mem::discriminant(&result),
        );
    }
    #[tokio::test]
    async fn skill_file_over_cap_honors_offset_and_limit() {
        let tmp = TempDir::new().unwrap();
        write_oversized_skill(&tmp);
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "skills/SKILL.md".to_string(),
            offset: Some(3),
            limit: Some(2),
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(fc) => {
                assert_eq!((Some(3), Some(2)), (fc.offset, fc.limit));
                let line = "x".repeat(200);
                assert_eq!(format!("3→{line}\n{line}"), fc.content);
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn md_in_skills_dir_ignores_model_offset_and_limit() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".grok/skills/my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let content = (1..=1200)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(skill_dir.join("reference.md"), &content).unwrap();
        let tool = ReadFileTool;
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: ".grok/skills/my-skill/reference.md".to_string(),
            offset: Some(3),
            limit: Some(1),
            pages: None,
            format: None,
        };
        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(fc) => {
                assert!(
                    fc.content.contains("line1"),
                    "missing line1: {}",
                    fc.content
                );
                assert!(
                    fc.content.contains("line1200"),
                    "missing line1200: {}",
                    fc.content
                );
                assert_eq!(fc.offset, None);
                assert_eq!(fc.limit, None);
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
    #[test]
    fn parse_single_page() {
        assert_eq!(parse_page_range("3", 10).unwrap(), vec![2]);
    }
    #[test]
    fn parse_page_range_inclusive() {
        assert_eq!(parse_page_range("2-5", 10).unwrap(), vec![1, 2, 3, 4]);
    }
    #[test]
    fn parse_open_ended_range() {
        assert_eq!(parse_page_range("8-", 10).unwrap(), vec![7, 8, 9]);
    }
    #[test]
    fn parse_comma_separated_mixed() {
        assert_eq!(
            parse_page_range("1,3,7-9", 10).unwrap(),
            vec![0, 2, 6, 7, 8]
        );
    }
    #[test]
    fn parse_deduplicates_and_sorts() {
        assert_eq!(parse_page_range("5,3,1,3,5", 10).unwrap(), vec![0, 2, 4]);
    }
    #[test]
    fn parse_page_range_clamps_open_end() {
        assert_eq!(parse_page_range("8-", 9).unwrap(), vec![7, 8]);
    }
    #[test]
    fn parse_page_range_rejects_zero() {
        let err = parse_page_range("0", 10).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }
    #[test]
    fn parse_page_range_rejects_beyond_count() {
        let err = parse_page_range("11", 10).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }
    #[test]
    fn parse_page_range_rejects_start_gt_end() {
        let err = parse_page_range("5-3", 10).unwrap_err();
        assert!(err.contains("start must be"), "got: {err}");
    }
    #[test]
    fn parse_page_range_rejects_invalid_number() {
        let err = parse_page_range("abc", 10).unwrap_err();
        assert!(err.contains("invalid page number"), "got: {err}");
    }
    #[test]
    fn parse_page_range_rejects_empty() {
        let err = parse_page_range("", 10).unwrap_err();
        assert!(err.contains("no pages specified"), "got: {err}");
    }
    #[test]
    fn parse_page_range_rejects_only_commas() {
        let err = parse_page_range(",,,", 10).unwrap_err();
        assert!(err.contains("no pages specified"), "got: {err}");
    }
    #[test]
    fn parse_page_range_rejects_too_many_pages() {
        let err = parse_page_range("1-21", 30).unwrap_err();
        assert!(err.contains("maximum is"), "got: {err}");
    }
    #[test]
    fn parse_page_range_max_pages_ok() {
        let result = parse_page_range("1-20", 30).unwrap();
        assert_eq!(result.len(), 20);
    }
    #[test]
    fn parse_page_range_whitespace_tolerance() {
        assert_eq!(parse_page_range(" 1 , 3 , 5 ", 10).unwrap(), vec![0, 2, 4]);
    }
    #[test]
    fn parse_page_range_single_page_doc() {
        assert_eq!(parse_page_range("1", 1).unwrap(), vec![0]);
    }
    #[test]
    fn parse_page_range_range_clamped_to_doc_end() {
        assert_eq!(parse_page_range("1-100", 5).unwrap(), vec![0, 1, 2, 3, 4]);
    }
    #[test]
    fn parse_page_range_zero_page_doc() {
        let err = parse_page_range("1", 0).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }
    async fn run_read_file_on(filename: &str, content: &[u8]) -> ReadFileOutput {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(filename), content).unwrap();
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: filename.to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        distill_tool_runtime::Tool::run(&ReadFileTool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn read_file_binary_rejected() {
        let result = run_read_file_on("archive.zip", b"PK\x03\x04fake zip content").await;
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert!(
                    msg.contains("Cannot read binary file"),
                    "expected binary rejection, got: {msg}"
                );
            }
            other => panic!("Expected FileReadError for binary file, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn read_file_binary_by_content() {
        let mut content = vec![0x00u8; 50];
        content.extend(vec![b'A'; 50]);
        let result = run_read_file_on("data.dat", &content).await;
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert!(
                    msg.contains("Cannot read binary file"),
                    "expected binary rejection, got: {msg}"
                );
            }
            other => panic!("Expected FileReadError for binary content, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn pdf_detection_by_extension() {
        let result = run_read_file_on("bad.pdf", b"not really a pdf").await;
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert!(
                    msg.contains("PDF") || msg.contains("pdf"),
                    "expected PDF-related error, got: {msg}"
                );
            }
            other => panic!("Expected FileReadError for invalid PDF, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn pdf_detection_by_magic_bytes() {
        let result = run_read_file_on("mystery.bin", b"%PDF-1.4 invalid rest").await;
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert!(
                    msg.contains("PDF") || msg.contains("pdf"),
                    "expected PDF-related error from magic byte detection, got: {msg}"
                );
            }
            other => panic!("Expected FileReadError for corrupt PDF, got {:?}", other),
        }
    }
    #[tokio::test]
    async fn pdf_size_gate_rejects_oversized() {
        let mut data = b"%PDF-1.4".to_vec();
        data.resize(MAX_PDF_BYTES + 1, 0);
        let result = run_read_file_on("huge.pdf", &data).await;
        match result {
            ReadFileOutput::FileReadError(msg) => {
                assert!(
                    msg.contains("exceeds") && msg.contains("MB"),
                    "expected size limit error, got: {msg}"
                );
            }
            other => panic!("Expected FileReadError for oversized PDF, got {:?}", other),
        }
    }
    #[test]
    fn pdf_not_caught_by_binary_guard() {
        assert!(
            !crate::util::binary::BINARY_EXTENSIONS.contains(&"pdf"),
            "pdf must not be in BINARY_EXTENSIONS"
        );
        assert!(
            !crate::util::binary::is_binary("pdf", b"%PDF-1.4"),
            "PDF content should not be detected as binary"
        );
    }
    /// Drive `execute` to completion; returns ordered deltas + terminal.
    /// Asserts `[Progress*, Terminal]` and the canonical envelope.
    async fn execute_collect(
        ctx: distill_tool_runtime::ToolCallContext,
        input: ReadFileInput,
    ) -> (Vec<String>, ReadFileOutput) {
        use futures::StreamExt;
        let mut stream = distill_tool_runtime::Tool::execute(&ReadFileTool, ctx, input).await;
        let mut deltas = Vec::new();
        let mut terminal: Option<ReadFileOutput> = None;
        while let Some(item) = stream.next().await {
            match item {
                distill_tool_runtime::ToolStreamItem::Progress(p) => {
                    assert!(terminal.is_none(), "Progress arrived after Terminal");
                    match p {
                        distill_tool_runtime::ToolProgress::Custom { subkind, payload } => {
                            assert_eq!(subkind, "read_file_chunk", "unexpected subkind");
                            let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) else {
                                panic!("payload missing delta: {payload}");
                            };
                            deltas.push(delta.to_owned());
                        }
                        other => panic!("expected Custom progress, got {other:?}"),
                    }
                }
                distill_tool_runtime::ToolStreamItem::Terminal(r) => {
                    assert!(terminal.is_none(), "more than one Terminal yielded");
                    terminal = Some(r.expect("read_file terminal should be Ok"));
                }
            }
        }
        (deltas, terminal.expect("stream ended without a Terminal"))
    }
    /// Gate ON: concatenated deltas reproduce the terminal `content` exactly.
    #[tokio::test]
    async fn read_file_streams_formatted_text_prefix() {
        let tmp = TempDir::new().unwrap();
        let body: String = (1..=300).map(|i| format!("line number {i}\n")).collect();
        std::fs::write(tmp.path().join("big.txt"), &body).unwrap();
        let input = ReadFileInput {
            path: "big.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let resources = test_resources(tmp.path());
        let (deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        assert!(
            deltas.len() >= 2,
            "expected multiple coalesced deltas, got {}",
            deltas.len()
        );
        let streamed = deltas.concat();
        match terminal {
            ReadFileOutput::FileContent(fc) => {
                assert!(
                    fc.content.starts_with(&streamed),
                    "streamed body must be a prefix of the terminal content"
                );
                assert_eq!(
                    streamed, fc.content,
                    "streamed deltas must reproduce the terminal content exactly"
                );
                assert!(
                    fc.content.starts_with("1→line number 1\n"),
                    "content must carry the formatter's line-number projection"
                );
            }
            other => panic!("expected FileContent terminal, got {other:?}"),
        }
    }
    /// Absent context = gate off ⇒ no Progress, terminal still surfaces.
    #[tokio::test]
    async fn read_file_streaming_suppressed_when_gate_off() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let input = ReadFileInput {
            path: "f.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let mut ctx = distill_tool_runtime::ToolCallContext::default();
        ctx.extensions
            .insert(test_resources(tmp.path()).into_shared());
        let (deltas, terminal) = execute_collect(ctx, input).await;
        assert!(
            deltas.is_empty(),
            "gate off must suppress all Progress, got {}",
            deltas.len()
        );
        assert!(matches!(terminal, ReadFileOutput::FileContent(_)));
    }
    /// PDF `format="text"` returns `FileContent` yet must NOT stream (returns
    /// before the text path; PPTX shares the same early-return shape).
    #[tokio::test]
    async fn read_file_pdf_text_path_is_terminal_only() {
        let tmp = TempDir::new().unwrap();
        let pdf_bytes = crate::implementations::read_file::pdf::make_test_pdf(&["Hello World"]);
        std::fs::write(tmp.path().join("doc.pdf"), &pdf_bytes).unwrap();
        let input = ReadFileInput {
            path: "doc.pdf".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: Some("text".to_string()),
        };
        let resources = test_resources(tmp.path());
        let (deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        assert!(
            deltas.is_empty(),
            "PDF text extraction must be terminal-only, got {} deltas",
            deltas.len()
        );
        assert!(
            matches!(terminal, ReadFileOutput::FileContent(_)),
            "PDF format=text yields FileContent, got {terminal:?}"
        );
    }
    /// PDF read with the default (image) format renders to `PdfPageImages` — a
    /// single non-incremental result, so it must not stream.
    #[tokio::test]
    async fn read_file_pdf_image_path_is_terminal_only() {
        let tmp = TempDir::new().unwrap();
        let pdf_bytes = crate::implementations::read_file::pdf::make_test_pdf(&["Some Text"]);
        std::fs::write(tmp.path().join("img.pdf"), &pdf_bytes).unwrap();
        let input = ReadFileInput {
            path: "img.pdf".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let resources = test_resources(tmp.path());
        let (deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        assert!(
            deltas.is_empty(),
            "PDF image rendering must be terminal-only, got {} deltas",
            deltas.len()
        );
        assert!(
            matches!(terminal, ReadFileOutput::PdfPageImages(_)),
            "PDF default format renders images, got {terminal:?}"
        );
    }
    /// Regression: a concurrent text read must not make a PDF-text read on
    /// the same `Resources` stream (streamability is call-local).
    #[tokio::test]
    async fn read_file_concurrent_text_and_pdf_text_do_not_cross_talk() {
        let tmp = TempDir::new().unwrap();
        let body: String = (1..=300).map(|i| format!("line number {i}\n")).collect();
        std::fs::write(tmp.path().join("big.txt"), &body).unwrap();
        let pdf_bytes = crate::implementations::read_file::pdf::make_test_pdf(&["Hello World"]);
        std::fs::write(tmp.path().join("doc.pdf"), &pdf_bytes).unwrap();
        let shared = test_resources(tmp.path()).into_shared();
        let text_input = || ReadFileInput {
            path: "big.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let pdf_input = || ReadFileInput {
            path: "doc.pdf".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: Some("text".to_string()),
        };
        for _ in 0..10 {
            let (text, pdf) = tokio::join!(
                execute_collect(test_ctx(shared.clone()), text_input()),
                execute_collect(test_ctx(shared.clone()), pdf_input()),
            );
            let (text_deltas, text_terminal) = text;
            let (pdf_deltas, pdf_terminal) = pdf;
            assert!(
                pdf_deltas.is_empty(),
                "concurrent PDF-text read must stay terminal-only, got {} deltas",
                pdf_deltas.len()
            );
            assert!(matches!(pdf_terminal, ReadFileOutput::FileContent(_)));
            assert!(!text_deltas.is_empty(), "the text read should still stream");
            match text_terminal {
                ReadFileOutput::FileContent(fc) => {
                    assert_eq!(text_deltas.concat(), fc.content)
                }
                other => panic!("expected FileContent for the text read, got {other:?}"),
            }
        }
    }
    /// Regression: a single formatted line above the 16 KiB cap still streams
    /// losslessly via fixed-size char-aligned windows.
    #[tokio::test]
    async fn read_file_streams_oversized_line_without_cap_break() {
        let tmp = TempDir::new().unwrap();
        let long = "a".repeat(20_000);
        std::fs::write(tmp.path().join("long.txt"), format!("{long}\nshort\n")).unwrap();
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "long.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let (deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        assert!(
            deltas.len() >= 2,
            "the >16 KiB line must be split into multiple sub-cap deltas, got {}",
            deltas.len()
        );
        for d in &deltas {
            assert!(
                d.len() <= STREAM_DELTA_TARGET_BYTES,
                "every delta must stay within the per-frame budget so stream_chunk never caps \
                 (and silently drops) it; got {} bytes",
                d.len()
            );
        }
        match terminal {
            ReadFileOutput::FileContent(fc) => {
                assert_eq!(
                    deltas.concat(),
                    fc.content,
                    "char-aligned sub-cap windows must reproduce the >16 KiB-line content exactly"
                )
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }
    /// Regression for the "death spiral" incident: a single-line ~49.5KB JSON payload must be readable in full with default
    /// config. The old 2000-char per-line clip made such files unreadable by construction (bash output and MCP results are
    /// byte-capped too), so the model could never load a payload it needed to re-emit as tool input.
    #[tokio::test]
    async fn single_line_payload_reads_in_full_by_default() {
        let tmp = TempDir::new().unwrap();
        let payload = format!(
            "{{\"uid\":\"cdlmfnq6x2o74e\",\"panels\":\"{}\"}}",
            "x".repeat(49_500)
        );
        std::fs::write(tmp.path().join("payload.json"), &payload).unwrap();
        let resources = test_resources(tmp.path());
        let input = ReadFileInput {
            path: "payload.json".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let (_deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        match terminal {
            ReadFileOutput::FileContent(fc) => {
                assert!(
                    fc.content.contains(&payload),
                    "single-line payload must be returned unclipped (got {} chars)",
                    fc.content.len()
                );
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }
    async fn read_huge_file(content: &str, with_execute_tool: bool) -> ReadFileOutput {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("huge.json"), content).unwrap();
        let mut resources = test_resources(tmp.path());
        let mut kinds = std::collections::HashMap::from([(ToolKind::Search, "grep".to_string())]);
        if with_execute_tool {
            kinds.insert(ToolKind::Execute, "run_terminal_command".to_string());
        }
        resources.insert(TemplateRenderer::new(kinds, Default::default()));
        let input = ReadFileInput {
            path: "huge.json".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };
        let (_deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        terminal
    }
    /// A single-line file that busts the whole-read token cap gets the
    /// shell-tool hint — line-based offset/limit cannot narrow one line.
    /// ~120KB single line ≈ 30K estimated tokens > READ_FILE_MAX_TOKENS (25K).
    #[tokio::test]
    async fn oversized_single_line_gets_shell_hint() {
        for content in [
            "z".repeat(120_000),
            format!("{}\n", "z".repeat(120_000)),
            format!("{}\r\n", "z".repeat(120_000)),
        ] {
            match read_huge_file(&content, true).await {
                ReadFileOutput::FileTooLarge(msg) => {
                    assert!(
                        msg.contains("single very long line")
                            && msg.contains("'run_terminal_command'"),
                        "single-line overflow must steer to the execute tool, got: {msg}"
                    );
                }
                other => panic!("expected FileTooLarge, got {other:?}"),
            }
        }
    }
    /// No execute tool in the toolset → no shell hint (never steer the model
    /// to a tool it cannot call).
    #[tokio::test]
    async fn oversized_single_line_hint_suppressed_without_execute_tool() {
        match read_huge_file(&"z".repeat(120_000), false).await {
            ReadFileOutput::FileTooLarge(msg) => {
                assert!(
                    !msg.contains("single very long line"),
                    "hint must be suppressed without an execute tool, got: {msg}"
                );
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }
    /// A narrowed read (offset/limit) that still lands on one oversized line
    /// gets the shell hint — the window, not the whole file, is what
    /// offset/limit cannot shrink further.
    #[tokio::test]
    async fn oversized_narrowed_window_single_line_gets_shell_hint() {
        let tmp = TempDir::new().unwrap();
        let content = format!("# header\n{}\nfooter\n", "z".repeat(120_000));
        std::fs::write(tmp.path().join("huge.json"), content).unwrap();
        let mut resources = test_resources(tmp.path());
        let mut kinds = std::collections::HashMap::from([(ToolKind::Search, "grep".to_string())]);
        kinds.insert(ToolKind::Execute, "run_terminal_command".to_string());
        resources.insert(TemplateRenderer::new(kinds, Default::default()));
        let input = ReadFileInput {
            path: "huge.json".to_string(),
            offset: Some(2),
            limit: Some(1),
            pages: None,
            format: None,
        };
        let (_deltas, terminal) = execute_collect(test_ctx(resources.into_shared()), input).await;
        match terminal {
            ReadFileOutput::FileTooLarge(msg) => {
                assert!(
                    msg.contains("single very long line"),
                    "narrowed single-line window must get the shell hint, got: {msg}"
                );
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }
    /// Multi-line oversized files keep the standard offset/limit guidance.
    #[tokio::test]
    async fn oversized_multi_line_gets_standard_guidance() {
        let content = format!("{}\n", "z".repeat(3_000)).repeat(50);
        match read_huge_file(&content, true).await {
            ReadFileOutput::FileTooLarge(msg) => {
                assert!(
                    !msg.contains("single very long line") && msg.contains("offset"),
                    "multi-line overflow must keep offset/limit guidance, got: {msg}"
                );
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }
    #[test]
    fn read_file_offset_schema_advertises_start_default() {
        let src = include_str!("mod.rs");
        assert!(
            src.contains(
                "description = \"The line number to start reading from. Only provide if the file is too large to read at once.\""
            ),
            "offset schemars description must remain the pre-PR wording"
        );
        assert!(
            src.contains("default = \"schema_default_offset\""),
            "offset must advertise schema_default_offset"
        );
    }
    #[test]
    fn resolve_read_start_line_negative_trailing_newline() {
        assert_eq!(resolve_read_start_line("a\nb\nc\n", Some(-3)), 2);
    }
    #[test]
    fn resolve_read_start_line_negative_no_trailing_newline() {
        assert_eq!(resolve_read_start_line("a\nb\nc", Some(-3)), 2);
    }
    #[test]
    fn resolve_read_start_line_very_negative_clamps_to_one() {
        assert_eq!(resolve_read_start_line("a\nb\nc\n", Some(-999)), 1);
    }
    #[test]
    fn resolve_read_start_line_zero_is_one() {
        assert_eq!(resolve_read_start_line("a\nb\nc\n", Some(0)), 1);
    }
    #[test]
    fn extract_file_content_lines_negative_offset() {
        let file_content = "line1\nline2\nline3\nline4\nline5\n";
        let total_lines = file_content.matches('\n').count() + 1;
        let extracted = extract_file_content_lines(file_content, Some(-2), Some(2), total_lines);
        assert_eq!(extracted.content, "5→line5\n");
    }
    #[test]
    fn extract_first_line_always_numbered_small_read() {
        let extracted = extract_file_content_lines("a\nb\nc\n", None, None, 4);
        assert_eq!(extracted.content, "1→a\nb\nc\n");
        assert_eq!(extracted.content_concise, "1→a\nb\nc\n");
    }
    #[test]
    fn extract_first_visible_line_numbered_with_offset() {
        let file_content = "a\nb\nc\nd\ne\n";
        let extracted = extract_file_content_lines(file_content, Some(3), None, 6);
        assert_eq!(extracted.content, "3→c\nd\ne\n");
    }
    #[test]
    fn extract_decade_line_numbered_in_addition_to_first() {
        let file_content: String = (1..=12).map(|i| format!("L{i}\n")).collect();
        let extracted = extract_file_content_lines(&file_content, None, None, 13);
        assert_eq!(
            extracted.content,
            "1→L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\n10→L10\nL11\nL12\n"
        );
    }
    /// Reviewer case: a window that resolves to only an empty line (here the
    /// phantom trailing empty of "hello\n" via offset=2) must still emit a
    /// line-number anchor rather than an empty body.
    #[test]
    fn extract_empty_only_window_still_anchored() {
        let extracted = extract_file_content_lines("hello\n", Some(2), None, 2);
        assert_eq!(extracted.content, "2→");
        assert_eq!(extracted.content_concise, "2→");
    }
    /// Harness parity: offset=-1 on a file with no trailing `\n` resolves to the
    /// phantom field only (start past any `split_inclusive` line), so Grok
    /// returns empty content/raw — same as the reference phantom-only window.
    #[test]
    fn extract_file_content_lines_negative_one_no_trailing_newline_stable() {
        let file_content = "a\nb\nc";
        let total_lines = file_content.matches('\n').count() + 1;
        assert_eq!(resolve_read_start_line(file_content, Some(-1)), 4);
        let extracted = extract_file_content_lines(file_content, Some(-1), Some(1), total_lines);
        assert!(
            extracted.content.is_empty(),
            "phantom-only start must not emit split_inclusive lines"
        );
        assert!(extracted.raw_output.is_empty());
        assert!(extracted.content_concise.is_empty());
    }
    #[test]
    fn read_file_input_accepts_negative_offset_json() {
        let neg: ReadFileInput =
            serde_json::from_str(r#"{"target_file":"x","offset":-3}"#).unwrap();
        assert_eq!(neg.offset, Some(-3));
        let str_neg: ReadFileInput =
            serde_json::from_str(r#"{"target_file":"x","offset":"-3"}"#).unwrap();
        assert_eq!(str_neg.offset, Some(-3));
    }
    #[test]
    fn stored_read_offset_drops_negatives() {
        assert_eq!(stored_read_offset(Some(-3)), None);
        assert_eq!(stored_read_offset(Some(0)), Some(0));
        assert_eq!(stored_read_offset(Some(4)), Some(4));
        assert_eq!(stored_read_offset(None), None);
    }
}
