// Modified for Distill by Samuel Fajreldines, 2026.
//! Splits markdown content into chunks suitable for embedding and search.
//!
//! Chunks respect markdown structure (headers, paragraphs, code blocks) and include ancestor headers so each chunk stands alone.
//! Character counts stand in for token counts, at about 4 characters per token.

use distill_config_types::MemoryIndexConfig;

/// A chunk of text extracted from a memory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The chunk text, including ancestor header context.
    pub text: String,
    /// 0-based start line in the source file.
    pub start_line: usize,
    /// 0-based end line (exclusive) in the source file.
    pub end_line: usize,
}

/// Compute a blake3 hash of the chunk text, returned as a hex string.
pub fn chunk_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Split markdown content into chunks, respecting structure.
/// Split on `##` headers; each section is a candidate chunk; If a section exceeds `max_chunk_chars`, split on paragraph boundaries (`\n\n`); If a paragraph still exceeds `max_chunk_chars`, split on line boundaries.
/// Continuation chunks are prefixed with the last `chunk_overlap_chars` of the previous chunk for embedding continuity, plus ancestor header context.
pub fn chunk_markdown(content: &str, config: &MemoryIndexConfig) -> Vec<Chunk> {
    if content.is_empty() {
        return vec![];
    }

    let max_chars = config.max_chunk_chars;
    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return vec![];
    }

    if content.len() <= max_chars {
        return vec![Chunk {
            text: content.to_string(),
            start_line: 0,
            end_line: lines.len(),
        }];
    }

    let sections = split_by_headers(&lines);
    let mut chunks = Vec::new();

    for section in &sections {
        let section_text = section.lines.join("\n");

        if section_text.len() <= max_chars {
            chunks.push(Chunk {
                text: add_header_context(&section.header_context, &section_text),
                start_line: section.start_line,
                end_line: section.start_line + section.lines.len(),
            });
        } else {
            // Section too large: split on paragraph boundaries
            let sub_chunks =
                split_section_by_paragraphs(section, max_chars, config.chunk_overlap_chars);
            chunks.extend(sub_chunks);
        }
    }

    chunks
}

/// A section of the document delimited by headers.
struct Section<'a> {
    /// The lines in this section (including the header line itself).
    lines: Vec<&'a str>,
    /// 0-based start line index in the original document.
    start_line: usize,
    /// Ancestor header context (e.g., `"## Architecture > ### Design"`).
    header_context: String,
}

/// Split lines into sections by `##` (or deeper) headers.
fn split_by_headers<'a>(lines: &[&'a str]) -> Vec<Section<'a>> {
    let mut sections: Vec<Section<'a>> = Vec::new();
    let mut current_lines: Vec<&'a str> = Vec::new();
    let mut current_start = 0;
    let mut header_stack: Vec<(usize, String)> = Vec::new(); // (level, text)

    for (i, &line) in lines.iter().enumerate() {
        if let Some(level) = header_level(line) {
            // Flush previous section
            if !current_lines.is_empty() {
                sections.push(Section {
                    lines: std::mem::take(&mut current_lines),
                    start_line: current_start,
                    header_context: format_header_context(&header_stack),
                });
            }
            current_start = i;

            // Pop same-or-deeper headers so only ancestors stay on the stack
            while header_stack.last().is_some_and(|(l, _)| *l >= level) {
                header_stack.pop();
            }
            header_stack.push((level, line.to_string()));
        }
        current_lines.push(line);
    }

    // Flush final section
    if !current_lines.is_empty() {
        sections.push(Section {
            lines: current_lines,
            start_line: current_start,
            header_context: format_header_context(&header_stack),
        });
    }

    sections
}

/// Split a large section into sub-chunks by paragraph boundaries (`\n\n`).
/// Continuation chunks are prefixed with the last `overlap_chars` of the previous chunk for embedding continuity.
fn split_section_by_paragraphs(
    section: &Section<'_>,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_text = String::new();
    let mut current_start = section.start_line;
    let mut line_offset = 0;

    for (i, &line) in section.lines.iter().enumerate() {
        let is_blank = line.trim().is_empty();

        // Paragraph boundary: blank line AND accumulated text is non-empty
        if is_blank && !current_text.is_empty() && current_text.len() + line.len() > max_chars {
            // Flush current chunk
            let flushed = current_text.trim().to_string();
            chunks.push(Chunk {
                text: add_header_context(&section.header_context, &flushed),
                start_line: current_start,
                end_line: section.start_line + i,
            });
            // The next chunk starts with the last `overlap_chars` of the flushed text
            current_text = if overlap_chars > 0 {
                let tail: String = flushed
                    .chars()
                    .rev()
                    .take(overlap_chars)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                tail
            } else {
                String::new()
            };
            current_start = section.start_line + i + 1;
            line_offset = i + 1;
            continue;
        }

        if !current_text.is_empty() {
            current_text.push('\n');
        }
        current_text.push_str(line);

        // If single line pushes us over max, flush what we have
        if current_text.len() > max_chars && i > line_offset {
            // Split before the just-added line; it carries over to the next chunk
            let split_at = current_text.rfind('\n').unwrap_or(current_text.len());
            let (keep, remainder) = current_text.split_at(split_at);
            chunks.push(Chunk {
                text: add_header_context(&section.header_context, keep.trim()),
                start_line: current_start,
                end_line: section.start_line + i,
            });
            current_text = remainder.trim_start_matches('\n').to_string();
            current_start = section.start_line + i;
            line_offset = i;
        }
    }

    // Flush remaining
    if !current_text.trim().is_empty() {
        chunks.push(Chunk {
            text: add_header_context(&section.header_context, current_text.trim()),
            start_line: current_start,
            end_line: section.start_line + section.lines.len(),
        });
    }

    chunks
}

/// Detect markdown header level (1 for `#`, 2 for `##`, etc.). Returns `None` if not a header.
pub(crate) fn header_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    // The hashes must be followed by a space or end of line to count as a header
    let rest = trimmed.get(level..)?;
    if rest.is_empty() || rest.starts_with(' ') {
        Some(level)
    } else {
        None
    }
}

/// Format header stack into a context string like `"## Section > ### Subsection"`.
fn format_header_context(stack: &[(usize, String)]) -> String {
    // The last entry is the current section's own header, not an ancestor
    let Some((_, ancestors)) = stack.split_last() else {
        return String::new();
    };
    ancestors
        .iter()
        .map(|(_, text)| text.trim().to_string())
        .collect::<Vec<_>>()
        .join(" > ")
}

/// Prepend ancestor header context to chunk text (if non-empty).
fn add_header_context(context: &str, text: &str) -> String {
    if context.is_empty() {
        text.to_string()
    } else {
        format!("[Context: {context}]\n\n{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> MemoryIndexConfig {
        MemoryIndexConfig::default()
    }

    #[test]
    fn test_chunk_hash_deterministic() {
        let h1 = chunk_hash("hello world");
        let h2 = chunk_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // A blake3 hash is 32 bytes, so 64 hex chars.
    }

    #[test]
    fn test_chunk_hash_different_inputs() {
        assert_ne!(chunk_hash("hello"), chunk_hash("world"));
    }

    #[test]
    fn test_chunk_empty_content() {
        let chunks = chunk_markdown("", &default_config());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_small_content_single_chunk() {
        let content = "# Title\n\nSome text here.";
        let chunks = chunk_markdown(content, &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks.first().expect("chunk 0").text, content);
        assert_eq!(chunks.first().expect("chunk 0").start_line, 0);
        assert_eq!(chunks.first().expect("chunk 0").end_line, 3);
    }

    #[test]
    fn test_chunk_splits_on_headers() {
        let content = "## Section 1\n\nContent for section 1 goes here with enough text to matter.\n\n\
                        ## Section 2\n\nContent for section 2 is also significant enough to be a chunk.";
        let config = MemoryIndexConfig {
            max_chunk_chars: 80,
            chunk_overlap_chars: 0,
        };
        let chunks = chunk_markdown(content, &config);
        assert!(
            chunks.len() >= 2,
            "should split into at least 2 chunks, got {}",
            chunks.len()
        );
        assert!(chunks.first().expect("chunk 0").text.contains("Section 1"));
        assert!(chunks.last().unwrap().text.contains("Section 2"));
    }

    #[test]
    fn test_chunk_header_context_for_subsections() {
        let content = "## Parent\n\nIntro.\n\n### Child\n\nChild content that is long enough to be its own chunk definitely.";
        let config = MemoryIndexConfig {
            max_chunk_chars: 60,
            chunk_overlap_chars: 0,
        };
        let chunks = chunk_markdown(content, &config);
        let child_chunk = chunks.iter().find(|c| c.text.contains("Child content"));
        assert!(child_chunk.is_some(), "should have a child chunk");
        assert!(
            child_chunk.unwrap().text.contains("[Context: ## Parent]"),
            "child chunk should have parent header context, got: {}",
            child_chunk.unwrap().text
        );
    }

    #[test]
    fn test_chunk_large_section_splits_on_paragraphs() {
        let para1 = "A".repeat(100);
        let para2 = "B".repeat(100);
        let content = format!("## Big Section\n\n{para1}\n\n{para2}");
        let config = MemoryIndexConfig {
            max_chunk_chars: 150,
            chunk_overlap_chars: 0,
        };
        let chunks = chunk_markdown(&content, &config);
        assert!(
            chunks.len() >= 2,
            "should split large section, got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn test_header_level_detection() {
        assert_eq!(header_level("# Title"), Some(1));
        assert_eq!(header_level("## Section"), Some(2));
        assert_eq!(header_level("### Subsection"), Some(3));
        assert_eq!(header_level("#hashtag"), None); // No space after the '#'.
        assert_eq!(header_level("not a header"), None);
        assert_eq!(header_level(""), None);
        assert_eq!(header_level("##"), Some(2)); // Header with no text.
    }

    #[test]
    fn test_chunk_line_numbers() {
        let content = "line 0\nline 1\nline 2\nline 3\nline 4";
        let chunks = chunk_markdown(content, &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks.first().expect("chunk 0").start_line, 0);
        assert_eq!(chunks.first().expect("chunk 0").end_line, 5);
    }

    #[test]
    fn test_chunk_preserves_code_blocks() {
        let content =
            "## Code\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nSome text.";
        let chunks = chunk_markdown(content, &default_config());
        assert_eq!(chunks.len(), 1);
        assert!(chunks.first().expect("chunk 0").text.contains("```rust"));
        assert!(chunks.first().expect("chunk 0").text.contains("fn main()"));
    }
}
