//! Deterministic payload reduction: the harness's own cheap lane.
//!
//! Everything here is pure: a payload goes in, a smaller payload plus the record
//! of what happened comes out, and no text is ever invented. Two families:
//!
//! * [`reduce_redundancy`] is **content-preserving by construction** — it only
//!   drops lines whose text appears elsewhere in the same payload (exact
//!   duplicates) and blank runs, so a reader cannot lose a fact it would have
//!   had. [`preserves_literals`] is the guard that proves it.
//! * [`elide_middle`] is **lossy and explicit**: it keeps the head and the tail
//!   and reports the byte range it removed, so the caller stores the original
//!   before anything is lost and hands the model the way back to it.
//!
//! Nothing here talks to a model: the cheap-model compression lane is a separate
//! decision, made by the caller, and this module is what runs when the answer is
//! "no model needed".

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// What a payload looks like, decided from the text alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadClass {
    /// Compiler/test/package output: progress lines, warnings, a summary.
    BuildLog,
    /// A directory or search listing: one entry per line, lots of repeats.
    Listing,
    /// A unified diff.
    Diff,
    /// Command output that is mostly a table of the same shape per row.
    CommandOutput,
    /// Prose: paragraphs, little structure.
    Prose,
    /// Nothing recognizable: the lanes stay out of the way.
    Unknown,
}

/// Classifies a payload by its first recognizable shape.
///
/// Deliberately cheap and conservative: a wrong guess only costs a missed
/// reduction, because both transforms are safe on any text.
pub fn classify_payload(text: &str) -> PayloadClass {
    let mut diff_markers = 0;
    let mut listing_markers = 0;
    let mut log_markers = 0;
    let mut rows = 0;
    for line in text.lines().take(400) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("@@")
            || trimmed.starts_with("+++ ")
            || trimmed.starts_with("--- ")
            || trimmed.starts_with("diff --git")
        {
            diff_markers += 1;
        }
        if trimmed.starts_with("test ")
            || trimmed.starts_with("warning:")
            || trimmed.starts_with("error")
            || trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Checking ")
            || trimmed.starts_with("Finished ")
            || trimmed.starts_with("running ")
        {
            log_markers += 1;
        }
        if trimmed.starts_with("total ")
            || trimmed.starts_with("drwx")
            || trimmed.starts_with("-rw")
            || trimmed.starts_with("./")
            || trimmed.starts_with("    ")
        {
            listing_markers += 1;
        }
        if trimmed.len() > 40 && trimmed.split_whitespace().count() > 6 {
            rows += 1;
        }
    }
    if diff_markers >= 3 {
        return PayloadClass::Diff;
    }
    if log_markers >= 3 {
        return PayloadClass::BuildLog;
    }
    if listing_markers >= 5 {
        return PayloadClass::Listing;
    }
    if rows >= 4 {
        return PayloadClass::CommandOutput;
    }
    if text.len() > 0 && text.lines().any(|line| line.len() > 200) {
        return PayloadClass::Prose;
    }
    PayloadClass::Unknown
}

/// One deterministic reduction, with the record the report needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduction {
    pub text: String,
    pub class: PayloadClass,
    /// Lines the transform removed (never a line whose text is unique).
    pub removed_lines: usize,
    /// Bytes of the original, for the measured ratio.
    pub original_bytes: usize,
}

impl Reduction {
    /// Fraction of the payload's bytes that the reduction removed (`0.0..=1.0`).
    pub fn saved_fraction(&self) -> f64 {
        if self.original_bytes == 0 {
            return 0.0;
        }
        1.0 - (self.text.len() as f64 / self.original_bytes as f64)
    }
}

/// Shortest line the duplicate rule will touch: below this, a repeated line is
/// structure (`}`, `---`, `)`) rather than content, and dropping it would change
/// what the payload means.
const MIN_DEDUPE_CHARS: usize = 12;

/// Drops only what the payload already repeats: a line whose text already
/// appeared (replaced by a pointer to the line that keeps it) and runs of blank
/// lines.
///
/// Content-preserving by construction: every dropped line's text is still in the
/// output, at the line the marker names, so a reader loses position information
/// at worst and never a fact. Returns `None` when there is nothing to do — the
/// common case for a payload a model actually has to read, and what makes "flags
/// off means today's bytes" true on the way in.
pub fn reduce_redundancy(text: &str) -> Option<Reduction> {
    if text.is_empty() {
        return None;
    }
    let class = classify_payload(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    let mut removed_lines = 0usize;
    // Line number in the *reduced* output that keeps the first copy, so the
    // marker points at something the reader can actually see.
    let mut first_seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut written = 0usize;

    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() {
            let mut end = index;
            while end < lines.len() && lines[end].trim().is_empty() {
                end += 1;
            }
            removed_lines += end - index - 1;
            out.push('\n');
            index = end;
            continue;
        }
        let dedupable = line.len() >= MIN_DEDUPE_CHARS && line.chars().any(char::is_alphanumeric);
        if dedupable && let Some(at) = first_seen.get(line) {
            let mut end = index;
            while end < lines.len() && lines[end] == line {
                end += 1;
            }
            out.push_str(&format!("[same as line {at}]\n"));
            removed_lines += end - index;
            index = end;
            continue;
        }
        if dedupable {
            first_seen.insert(line.to_owned(), written + 1);
        }
        out.push_str(line);
        out.push('\n');
        written += 1;
        index += 1;
    }

    if removed_lines == 0 {
        return None;
    }
    Some(Reduction {
        text: out,
        class,
        removed_lines,
        original_bytes: text.len(),
    })
}

/// What an elision removed, so the caller can store the original first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elision {
    pub text: String,
    /// Lines kept verbatim from the head and the tail.
    pub kept_lines: usize,
    /// Lines replaced by the marker.
    pub removed_lines: usize,
    /// Byte offset and length of the removed region **in the original text**, so
    /// the caller can store exactly what was lost.
    pub removed_range: (usize, usize),
}

/// Keeps the head and the tail of a payload and replaces the middle with a
/// marker the caller fills in with the way back to the original.
///
/// This is the lossy family, so it refuses to run unless it actually gains
/// something (a caller that elides 3 lines has paid a store write for nothing),
/// and it never touches a payload smaller than `min_bytes`.
pub fn elide_middle(
    text: &str,
    keep_head: usize,
    keep_tail: usize,
    min_bytes: usize,
    marker: &str,
) -> Option<Elision> {
    if text.len() < min_bytes {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= keep_head + keep_tail + 1 {
        return None;
    }
    let head_end = lines[..keep_head].iter().map(|l| l.len() + 1).sum::<usize>();
    let tail_start: usize = lines[..lines.len() - keep_tail]
        .iter()
        .map(|l| l.len() + 1)
        .sum();
    if tail_start <= head_end {
        return None;
    }
    let removed_lines = lines.len() - keep_head - keep_tail;
    let mut out = String::with_capacity(text.len());
    out.push_str(&lines[..keep_head].join("\n"));
    out.push('\n');
    out.push_str(&marker.replace("{lines}", &removed_lines.to_string()));
    out.push('\n');
    out.push_str(&lines[lines.len() - keep_tail..].join("\n"));
    Some(Elision {
        text: out,
        kept_lines: keep_head + keep_tail,
        removed_lines,
        removed_range: (head_end, tail_start - head_end),
    })
}

/// The literals a reader must never lose: paths, `file:line`, numbers, hashes,
/// quoted commands, urls, and the error vocabulary that explains a failure.
pub fn literals(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for raw in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | ';' | ':' | '"' | '\'' | '`' | '[' | ']' | '{' | '}' | '*' | '<' | '>'
            )
        });
        if token.len() < 3 {
            continue;
        }
        let looks_like_path = token.contains('/')
            && (token.contains('.') || token.starts_with('/') || token.starts_with("./"));
        let has_line_number = token
            .rsplit_once(':')
            .is_some_and(|(_, line)| line.parse::<u32>().is_ok() && token.contains('/'));
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        let has_hash_char = token.chars().any(|c| c.is_ascii_hexdigit()) && token.len() >= 12;
        if looks_like_path || has_line_number || has_hash_char {
            found.insert(token.to_owned());
        } else if has_digit && !token.chars().all(|c| c.is_ascii_digit()) {
            // `E0308`, `v0.1.220`, `x86_64`, `cargo`, `1.5s`, `42ms`: an identifier
            // carrying a number, but not a bare line offset.
            found.insert(token.to_owned());
        } else if has_digit {
            found.insert(token.to_owned());
        }
    }
    for word in ["error", "panic", "failed", "failure", "traceback", "not found"] {
        if text.to_ascii_lowercase().contains(word) {
            found.insert(word.to_owned());
        }
    }
    found
}

/// True when every literal of `original` still appears in `reduced`.
///
/// The guard for the lossy family: a caller that elides must check this on the
/// text it keeps, and report the rest as recoverable-from-store rather than as
/// present.
pub fn preserves_literals(original: &str, reduced: &str) -> bool {
    literals(original)
        .iter()
        .all(|literal| reduced.contains(literal.as_str()))
}

/// The literals of `original` that `reduced` no longer carries.
pub fn lost_literals(original: &str, reduced: &str) -> Vec<String> {
    literals(original)
        .into_iter()
        .filter(|literal| !reduced.contains(literal.as_str()))
        .collect()
}

/// FNV-1a of the payload: the store handle and the read-reuse key.
///
/// Not cryptographic: it names a file and detects "the model already read this",
/// and a collision would only cost a re-read.
pub fn content_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The note that replaces a payload the model has already been given verbatim.
///
/// It names the earlier occurrence and the byte count so the reader knows the
/// content is unchanged and where to look, instead of silently dropping it.
pub fn reuse_note(hash: &str, first_at: &str, bytes: usize) -> String {
    format!(
        "[unchanged content: {bytes} bytes already sent this session at {first_at}, \
         identical sha {hash}; read that copy instead of this one]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_build_log() -> String {
        let mut log = String::new();
        for i in 0..80 {
            log.push_str(&format!("   Compiling crate-{i} v0.1.0 (/Users/samuel/dev/jev-build/crates/crate-{i})\n"));
            // A real cargo log repeats its per-crate progress line between the
            // names; the names are unique and must survive, the repeats must not.
            for _ in 0..5 {
                log.push_str("       Fresh (0.4s)\n");
            }
            log.push('\n');
        }
        log.push_str("warning: unused variable `total` in crates/codegen/xai-grok-shell/src/lib.rs:412\n");
        log.push_str("error[E0308]: mismatched types in crates/codegen/xai-grok-sampler/src/client.rs:868\n");
        log.push_str("    Finished `dev` profile in 120.5s\n");
        log
    }

    fn big_listing() -> String {
        let mut listing = String::new();
        for i in 0..120 {
            listing.push_str(&format!("-rw-r--r--  1 samuel staff 4096 Sep 18 10:22 src/module_{i}.rs\n"));
            listing.push_str("drwxr-xr-x  4 samuel staff  128 Sep 18 10:22 .\n");
        }
        listing
    }

    fn prose_doc() -> String {
        let mut doc = String::new();
        for i in 0..40 {
            doc.push_str(&format!("Paragraph {i}: the harness keeps the catalogues and the levers, and the model is only consulted when the cheap lane cannot decide. The rule is the same everywhere: never invent text, never drop a fact the reader would have had, and always leave a way back to the original. See crates/codegen/xai-grok-workspace/src/jev/reduce.rs:1 for the transforms.\n\n\n\n"));
            // Boilerplate a long document repeats in full (license text, a
            // standard warning block): the second copy is the cheap lane's win.
            doc.push_str("This document is provided as is, without warranty of any kind, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose and noninfringement, as published at https://example.org/license/v2.\n\n\n\n");
        }
        doc
    }

    #[test]
    fn a_payload_without_redundancy_is_left_alone() {
        // Every line is unique: the lane must return nothing, so a caller can
        // treat `None` as "today's bytes" without a second look.
        let unique = (0..50)
            .map(|i| format!("unique line {i} at /a/b/c{i}.rs:{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(reduce_redundancy(&unique).is_none());
        assert_eq!(classify_payload(&unique), PayloadClass::Unknown);
    }

    #[test]
    fn a_build_log_reduces_and_keeps_every_literal() {
        let log = big_build_log();
        let reduced = reduce_redundancy(&log).expect("progress lines and duplicates reduce");
        assert_eq!(reduced.class, PayloadClass::BuildLog);
        assert!(reduced.removed_lines > 0);
        assert!(
            reduced.saved_fraction() > 0.25,
            "a log full of repeated progress lines and blank runs saves a quarter or more, got {}",
            reduced.saved_fraction()
        );
        // The two lines that explain what happened survive, verbatim.
        assert!(reduced.text.contains("error[E0308]: mismatched types"));
        assert!(reduced.text.contains("client.rs:868"));
        assert!(reduced.text.contains("warning: unused variable `total`"));
        assert!(reduced.text.contains("lib.rs:412"));
        assert!(
            preserves_literals(&log, &reduced.text),
            "lost: {:#?}",
            lost_literals(&log, &reduced.text)
        );
        // The reader is told what was removed, instead of it silently vanishing.
        assert!(
            reduced.text.contains("[same as line "),
            "the reader is told where the kept copy is:\n{}",
            reduced.text
        );
    }

    #[test]
    fn a_listing_and_a_prose_doc_reduce_without_losing_a_single_reference() {
        let listing = big_listing();
        let reduced = reduce_redundancy(&listing).expect("a listing repeats the `.` line");
        assert_eq!(reduced.class, PayloadClass::Listing);
        assert!(preserves_literals(&listing, &reduced.text));
        assert!(reduced.text.contains("module_119.rs"), "the tail survives");

        let prose = prose_doc();
        let reduced = reduce_redundancy(&prose).expect("blank runs and boilerplate collapse");
        assert!(
            reduced.saved_fraction() > 0.2,
            "a document that repeats a paragraph per section saves a fifth or more, got {}",
            reduced.saved_fraction()
        );
        assert!(preserves_literals(&prose, &reduced.text));
        assert!(reduced.text.contains("reduce.rs:1"));
    }

    #[test]
    fn elision_reports_exactly_what_it_removed_and_refuses_a_small_gain() {
        let log = big_build_log();
        let elided = elide_middle(
            &log,
            4,
            4,
            1_024,
            "[... {lines} lines elided; full text stored, read it if you need them]",
        )
        .expect("a large payload elides");
        assert!(elided.removed_lines > 0);
        assert!(elided.kept_lines == 8);
        // The reported range is the removed slice of the ORIGINAL, byte-exact:
        // this is what the caller stores before the text is lost.
        let (offset, length) = elided.removed_range;
        let removed = &log[offset..offset + length];
        assert!(removed.contains("crate-40"), "the middle is what went");
        assert!(!elided.text.contains("crate-40"));
        assert!(elided.text.contains("lines elided"));
        // The head and the tail are the reader's anchors.
        assert!(elided.text.contains("crate-0 "));
        assert!(elided.text.contains("Finished `dev` profile"));

        // Too small to be worth a store write: the lane stays out.
        assert!(elide_middle("a\nb\nc\n", 1, 1, 1_024, "[..]").is_none());
        // Nothing in the middle to remove: same.
        assert!(elide_middle(&"x\n".repeat(20), 10, 10, 8, "[..]").is_none());
    }

    #[test]
    fn the_literal_check_names_what_a_lossy_transform_dropped() {
        let text = "read src/main.rs:10 then /tmp/build/out.log and E0308 at 1.5s\n";
        assert!(preserves_literals(text, text));
        let lost = lost_literals(text, "read src/main.rs:10\n");
        assert!(
            lost.iter().any(|item| item.contains("/tmp/build/out.log")),
            "the dropped path is named: {lost:?}"
        );
        assert!(lost.iter().any(|item| item == "E0308"));
    }

    #[test]
    fn the_reuse_note_points_at_the_earlier_copy() {
        let body = "same bytes";
        let hash = content_hash(body);
        assert_eq!(hash, content_hash(body), "a repeated read hashes the same");
        assert_ne!(hash, content_hash("other bytes"));
        let note = reuse_note(&hash, "read_file(src/main.rs)", body.len());
        assert!(note.contains("already sent this session"));
        assert!(note.contains("read_file(src/main.rs)"));
        assert!(note.contains(&hash));
    }

    #[test]
    fn classification_survives_the_shapes_it_will_meet() {
        assert_eq!(classify_payload(&big_build_log()), PayloadClass::BuildLog);
        assert_eq!(classify_payload(&big_listing()), PayloadClass::Listing);
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n context\n-old\n+new\n";
        assert_eq!(classify_payload(diff), PayloadClass::Diff);
        let table = (0..6)
            .map(|i| format!("row {i} has more than forty characters of tabular output here"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(classify_payload(&table), PayloadClass::CommandOutput);
        assert_eq!(classify_payload(""), PayloadClass::Unknown);
    }
}
