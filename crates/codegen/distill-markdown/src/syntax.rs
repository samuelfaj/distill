// Modified for Distill by Samuel Fajreldines, 2026.
//! Syntax highlighting support using syntect.

use std::io::Cursor;
use std::path::Path;
use std::sync::OnceLock;

use syntect::{
    dumps::from_uncompressed_data,
    easy::HighlightLines,
    highlighting::{Theme as SyntectTheme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

/// Syntax highlighting configuration.
///
/// Create one instance and pass it to the markdown renderer.
pub struct Syntect {
    /// The color theme for syntax highlighting.
    pub theme: SyntectTheme,
    /// The syntax definitions (supports 250+ languages via two-face).
    pub syntax_set: SyntaxSet,
}

impl Syntect {
    /// Create a new Syntect instance from theme bytes.
    /// The theme bytes should be a TextMate `.tmTheme` file.
    pub fn new(theme_bytes: &[u8]) -> Self {
        let mut cursor = Cursor::new(theme_bytes);
        let theme = ThemeSet::load_from_reader(&mut cursor).expect("Failed to load theme");
        Self {
            theme,
            syntax_set: syntax_set(),
        }
    }

    /// Find a syntax definition by file path extension.
    pub fn find_syntax_by_file_path(&self, file_path: &Path) -> Option<&SyntaxReference> {
        let ext = file_path.extension()?.to_str()?;
        if is_swift_token(ext) {
            return patched_swift(&self.syntax_set);
        }
        self.syntax_set.find_syntax_by_extension(ext)
    }

    /// Find a syntax definition by language token (e.g., "rust", "python").
    pub fn find_syntax_by_token(&self, token: &str) -> Option<&SyntaxReference> {
        if is_swift_token(token) {
            return patched_swift(&self.syntax_set);
        }
        self.syntax_set.find_syntax_by_token(token)
    }

    /// Create a highlighter for the given file path.
    pub fn highlight_lines_by_file_path(&self, file_path: &Path) -> Option<HighlightLines<'_>> {
        Some(HighlightLines::new(
            self.find_syntax_by_file_path(file_path)?,
            &self.theme,
        ))
    }

    /// Create a highlighter for the given language token.
    pub fn highlight_lines_for_token(&self, token: &str) -> Option<HighlightLines<'_>> {
        Some(HighlightLines::new(
            self.find_syntax_by_token(token)?,
            &self.theme,
        ))
    }

    /// Highlighter for a fenced code block *info* string: a language token (`rust`, `python`) or a `lineStart:lineEnd:path` line-range citation.
    /// A citation's path resolves via [`Syntect::find_syntax_by_file_path`].
    /// When the path has no known syntax, the whole string falls back to [`Syntect::find_syntax_by_token`], so plain ` ```lang` blocks keep working.
    pub fn highlight_lines_for_fence_info(&self, fence_info: &str) -> Option<HighlightLines<'_>> {
        Some(HighlightLines::new(
            self.find_syntax_for_fence_info(fence_info)?,
            &self.theme,
        ))
    }

    /// Resolve the [`SyntaxReference`] for a fenced code block *info* string, with the same rules as [`Syntect::highlight_lines_for_fence_info`].
    /// This is exposed so the incremental open-code highlighter can build its resumable `ParseState`/`HighlightState` from the same syntax.
    /// That keeps its output byte-identical to the batch `HighlightLines` path.
    pub(crate) fn find_syntax_for_fence_info(&self, fence_info: &str) -> Option<&SyntaxReference> {
        if let Some((_, _, path)) = parse_line_citation_fence_info(fence_info)
            && let Some(s) = self.find_syntax_by_file_path(Path::new(path))
        {
            return Some(s);
        }
        self.find_syntax_by_token(fence_info)
    }
}

fn syntax_set() -> SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(load_syntax_set).clone()
}

fn load_syntax_set() -> SyntaxSet {
    from_uncompressed_data(include_bytes!(concat!(env!("OUT_DIR"), "/syntaxes.bin")))
        .expect("syntax dump")
}

fn is_swift_token(token: &str) -> bool {
    token.eq_ignore_ascii_case("swift")
}

fn patched_swift(set: &SyntaxSet) -> Option<&SyntaxReference> {
    // Last `.swift` wins: we append the patched grammar after two-face's Swift.
    set.syntaxes().iter().rev().find(|syntax| {
        syntax
            .file_extensions
            .iter()
            .any(|ext| ext.eq_ignore_ascii_case("swift"))
    })
}

/// The path is the segment after the **second** colon; it is then parsed with [`Path::new`].
/// Paths with extra colons in the first two segments are not supported; use a repo-relative or forward-slash form.
fn parse_line_citation_fence_info(info: &str) -> Option<(&str, &str, &str)> {
    let mut it = info.splitn(3, ':');
    let start = it.next()?;
    let end = it.next()?;
    let path = it.next()?;
    if start.is_empty() || !start.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if end.is_empty() || !end.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if path.is_empty() {
        return None;
    }
    Some((start, end, path))
}

/// Syntax highlight code, returning raw styled segments per line.
/// `fence_info` is the fenced code block *info* string (language tag or citation); see [`Syntect::highlight_lines_for_fence_info`].
/// This function lives here (not in `parse`) so both the parser and the streaming highlighter caches depend one-way on `syntax`.
pub(crate) fn syntax_highlight_raw(
    syntect: Option<&Syntect>,
    fence_info: &str,
    text: &str,
) -> Option<Vec<Vec<(syntect::highlighting::Style, String)>>> {
    use syntect::util::LinesWithEndings;

    let syn = syntect?;
    let mut hl = syn.highlight_lines_for_fence_info(fence_info)?;
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(text) {
        let highlighted = hl.highlight_line(line, &syn.syntax_set).ok()?;
        lines.push(
            highlighted
                .into_iter()
                .map(|(s, t)| (s, t.to_string()))
                .collect(),
        );
    }
    Some(lines)
}

/// Get a shared Syntect instance for tests.
///
/// This loads the tokyo-night theme bundled with the crate.
#[cfg(any(test, fuzzing))]
#[allow(dead_code)]
pub fn test_syntect() -> &'static Syntect {
    static TEST_SYNTECT: OnceLock<Syntect> = OnceLock::new();
    TEST_SYNTECT.get_or_init(|| Syntect::new(include_bytes!("../assets/tokyo-night.tmTheme")))
}

#[cfg(test)]
mod tests {
    use super::parse_line_citation_fence_info;

    #[test]
    fn line_citation_fence_parses_start_end_path() {
        assert_eq!(
            parse_line_citation_fence_info("37:65:crates/example/src/tools/read.rs"),
            Some(("37", "65", "crates/example/src/tools/read.rs"))
        );
    }

    #[test]
    fn line_citation_rejects_non_numeric_line() {
        assert_eq!(parse_line_citation_fence_info("37:ab:file.rs"), None);
    }

    #[test]
    fn line_citation_rejects_plain_lang_token() {
        assert_eq!(parse_line_citation_fence_info("rust"), None);
        assert_eq!(parse_line_citation_fence_info(""), None);
    }

    #[test]
    fn highlight_lines_for_fence_info_resolves_citation_path_to_rust() {
        let s = super::test_syntect();
        assert!(
            s.highlight_lines_for_fence_info("37:65:crates/codegen/distill-markdown/src/parse.rs")
                .is_some()
        );
    }

    #[test]
    fn highlight_lines_for_fence_info_still_accepts_rust_token() {
        let s = super::test_syntect();
        assert!(s.highlight_lines_for_fence_info("rust").is_some());
    }

    fn swift_scope_tokens(source: &str) -> Vec<(String, String)> {
        let syn = super::test_syntect();
        let syntax = syn.find_syntax_by_token("swift").expect("swift syntax");
        let mut parse_state = syntect::parsing::ParseState::new(syntax);
        let mut stack = syntect::parsing::ScopeStack::new();
        let mut tokens = Vec::new();
        for line in syntect::util::LinesWithEndings::from(source) {
            let ops = parse_state
                .parse_line(line, &syn.syntax_set)
                .expect("parse swift");
            let mut last = 0usize;
            for (i, op) in ops {
                if i > last {
                    tokens.push((
                        line.get(last..i).unwrap_or("").to_owned(),
                        stack.to_string(),
                    ));
                }
                stack.apply(&op).expect("scope op");
                last = i;
            }
            if last < line.len() {
                tokens.push((line.get(last..).unwrap_or("").to_owned(), stack.to_string()));
            }
        }
        tokens
    }

    fn token_scopes<'a>(tokens: &'a [(String, String)], needle: &str) -> &'a str {
        tokens
            .iter()
            .find(|(text, _)| text.contains(needle))
            .map(|(_, scopes)| scopes.as_str())
            .unwrap_or_else(|| panic!("missing {needle:?} in {tokens:?}"))
    }

    fn is_string_text_scope(scope: &str) -> bool {
        scope == "string.quoted.double.swift" || scope == "string.quoted.triple.swift"
    }

    fn assert_in_interpolation(scopes: &str, token: &str) {
        assert!(
            scopes
                .split_whitespace()
                .any(|s| s == "meta.expression.swift"),
            "{token} should stay inside interpolation, got {scopes}"
        );
        assert!(
            !scopes.split_whitespace().any(is_string_text_scope),
            "{token} should not paint as string text, got {scopes}"
        );
    }

    #[test]
    fn swift_nested_paren_interpolation_keeps_locale_quoted_in_expression() {
        const REPORTED: &str =
            "Text(verbatim: \"Delete \\((pendingDeletion?.name ?? \"\").localeQuoted)\")\n";
        let tokens = swift_scope_tokens(REPORTED);
        assert_in_interpolation(token_scopes(&tokens, "localeQuoted"), "localeQuoted");
        assert_in_interpolation(token_scopes(&tokens, "pendingDeletion"), "pendingDeletion");
        let locale = tokens
            .iter()
            .position(|(text, _)| text.contains("localeQuoted"))
            .expect("localeQuoted token");
        let close = tokens
            .get(locale + 1)
            .expect("interpolation close after localeQuoted");
        assert_eq!(close.0, ")");
        assert!(
            close
                .1
                .split_whitespace()
                .any(|s| s == "support.punctuation.expression.end.swift"),
            "interpolation should close after localeQuoted, got {}",
            close.1
        );
    }

    #[test]
    fn swift_nested_paren_interpolation_does_not_need_inner_string() {
        const LINE: &str = "let s = \"\\((a ?? b).localeQuoted)\"\n";
        let tokens = swift_scope_tokens(LINE);
        assert_in_interpolation(token_scopes(&tokens, "localeQuoted"), "localeQuoted");
    }

    #[test]
    fn swift_flat_interpolation_still_scopes_locale_quoted() {
        const LINE: &str = "let s = \"\\(x.localeQuoted)\"\n";
        let tokens = swift_scope_tokens(LINE);
        assert_in_interpolation(token_scopes(&tokens, "localeQuoted"), "localeQuoted");
    }

    fn assert_interpolation_closed_before(tokens: &[(String, String)], after: &str) {
        let scopes = token_scopes(tokens, after);
        assert!(
            !scopes
                .split_whitespace()
                .any(|s| is_string_text_scope(s) || s == "meta.expression.swift"),
            "{after} should not inherit spilled interpolation/string scopes, got {scopes}"
        );
    }

    fn assert_interpolation_closer(tokens: &[(String, String)]) {
        assert!(
            tokens.iter().any(|(text, scopes)| {
                text == ")"
                    && scopes
                        .split_whitespace()
                        .any(|s| s == "support.punctuation.expression.end.swift")
            }),
            "expected interpolation closer, got {tokens:?}"
        );
    }

    #[test]
    fn swift_nested_block_comment_parens_do_not_keep_interpolation_open() {
        const LINE: &str =
            "let s = \"\\(1 /* outer /* inner */ ( still outer */)\"; let trailing = 1\n";
        let tokens = swift_scope_tokens(LINE);
        assert_interpolation_closer(&tokens);
        assert_interpolation_closed_before(&tokens, "trailing");
    }

    #[test]
    fn swift_nested_parens_ignore_parens_inside_nested_block_comments() {
        const LINE: &str =
            "let s = \"\\((1 /* outer /* inner */ ( still outer */))\"; let trailing = 1\n";
        let tokens = swift_scope_tokens(LINE);
        assert_interpolation_closer(&tokens);
        assert_interpolation_closed_before(&tokens, "trailing");
    }

    #[test]
    fn swift_nested_doc_comment_parens_do_not_keep_interpolation_open() {
        const LINE: &str =
            "let s = \"\\(1 /** outer /* inner */ ( still outer */)\"; let trailing = 1\n";
        let tokens = swift_scope_tokens(LINE);
        assert_interpolation_closer(&tokens);
        assert_interpolation_closed_before(&tokens, "trailing");
    }

    #[test]
    fn swift_simple_block_comment_parens_do_not_keep_interpolation_open() {
        const LINE: &str = "let s = \"\\(1 /* ( comment */ 2)\"; let trailing = 1\n";
        let tokens = swift_scope_tokens(LINE);
        assert_in_interpolation(token_scopes(&tokens, "2"), "2");
        assert_interpolation_closer(&tokens);
        assert_interpolation_closed_before(&tokens, "trailing");
    }

    #[test]
    fn swift_triple_quoted_string_closes_before_following_code() {
        const SOURCE: &str = concat!(
            "return \"\"\"\n",
            "    {\"shareId\": \"share-1\", \"name\": \"Launch buddy\"}\n",
            "    \"\"\"\n",
            "\n",
            "    // MARK: - Presentation\n",
            "\n",
            "    @Test\n",
            "    func presentationFollowsTheHostStampUntilTheStoreChangesIt() {\n",
        );
        let tokens = swift_scope_tokens(SOURCE);
        assert_interpolation_closed_before(&tokens, "MARK");
        assert_interpolation_closed_before(&tokens, "Test");
        assert_interpolation_closed_before(
            &tokens,
            "presentationFollowsTheHostStampUntilTheStoreChangesIt",
        );
        assert!(
            token_scopes(&tokens, "MARK")
                .split_whitespace()
                .any(|s| s == "comment.line.double-slash.swift"),
            "MARK should be a line comment, got {}",
            token_scopes(&tokens, "MARK")
        );
        assert!(
            token_scopes(&tokens, "Test")
                .split_whitespace()
                .any(|s| s == "storage.modifier.attribute.swift"),
            "@Test should be an attribute, got {}",
            token_scopes(&tokens, "Test")
        );
    }

    #[test]
    fn swift_triple_quoted_string_keeps_nested_paren_interpolation() {
        const SOURCE: &str = concat!(
            "let s = \"\"\"\n",
            "Delete \\((pendingDeletion?.name ?? \"\").localeQuoted)\n",
            "\"\"\"\n",
            "let trailing = 1\n",
        );
        let tokens = swift_scope_tokens(SOURCE);
        assert_in_interpolation(token_scopes(&tokens, "localeQuoted"), "localeQuoted");
        assert_interpolation_closer(&tokens);
        assert_interpolation_closed_before(&tokens, "trailing");
    }

    #[test]
    fn swift_triple_quoted_line_wrap_backslash_is_not_illegal() {
        const SOURCE: &str = concat!(
            "let s = \"\"\"\n",
            "hello \\\n",
            "world\n",
            "bad \\ slash\n",
            "spaced \\  \n",
            "ok\n",
            "\"\"\"\n",
            "let trailing = 1\n",
        );
        let tokens = swift_scope_tokens(SOURCE);
        let wraps: Vec<&str> = tokens
            .iter()
            .filter_map(|(text, scopes)| {
                let trimmed = text.trim_end_matches(['\r', '\n']);
                let rest = trimmed.strip_prefix('\\')?;
                rest.bytes()
                    .all(|b| matches!(b, b' ' | b'\t'))
                    .then_some(scopes.as_str())
            })
            .collect();
        let [eol, mid, spaced] = wraps.as_slice() else {
            panic!("expected wrap + mid-line + spaced wrap backslashes, got {tokens:?}");
        };
        assert!(
            !eol.split_whitespace().any(|s| s == "invalid.illegal.swift"),
            "EOL \\ in \"\"\" should be a line wrap, got {eol}"
        );
        assert!(
            mid.split_whitespace().any(|s| s == "invalid.illegal.swift"),
            "mid-line lone \\ should stay illegal, got {mid}"
        );
        assert!(
            !spaced
                .split_whitespace()
                .any(|s| s == "invalid.illegal.swift"),
            "EOL \\ with trailing spaces should be a line wrap, got {spaced}"
        );
        assert_interpolation_closed_before(&tokens, "trailing");
    }

    #[test]
    fn swift_triple_quoted_inner_quotes_do_not_toggle_string() {
        const SOURCE: &str = concat!(
            "let s = \"\"\"\n",
            "\"odd\" quote count: \"one\" \"two\" \"three\"\n",
            "\"\"\"\n",
            "// after\n",
        );
        let tokens = swift_scope_tokens(SOURCE);
        assert_interpolation_closed_before(&tokens, "after");
        assert!(
            token_scopes(&tokens, "after")
                .split_whitespace()
                .any(|s| s == "comment.line.double-slash.swift"),
            "code after closing delimiter should be a comment, got {}",
            token_scopes(&tokens, "after")
        );
    }
}
