// Modified for Distill by Samuel Fajreldines, 2026.
//! File ownership declared by `task` children: path normalization, overlap
//! checks between live siblings, and the model-facing texts built from them.

use std::path::{Component, Path, PathBuf};

/// Siblings listed in a child's prompt.
pub const MAX_LISTED_SIBLINGS: usize = 8;
/// Files listed per sibling in a child's prompt.
const MAX_LISTED_FILES: usize = 10;
/// Edited-outside-declared paths listed in a completion note.
const MAX_LISTED_OUTSIDE: usize = 20;

/// A live subagent that shares the workspace, as shown to a new child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiblingFiles {
    pub description: String,
    pub files: Vec<String>,
}

/// Resolve declared paths against the child's cwd into absolute, lexically
/// clean, deduplicated strings. Blank and sentinel entries are dropped.
pub fn normalize_declared_files(
    files: &[String],
    cwd: &Path,
    display_cwd: Option<&Path>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in files {
        if !distill_tool_types::is_not_sentinel(raw) {
            continue;
        }
        let resolved = crate::types::resources::resolve_model_path(cwd, display_cwd, raw);
        let path = canonicalize_existing_ancestor(&normalize_lexically(&resolved));
        let path = path.to_string_lossy().into_owned();
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Resolve `.` and `..` without touching the filesystem.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the deepest existing ancestor so symlink aliases compare equal
/// even for files that do not exist yet.
fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = dunce::canonicalize(ancestor) {
            suffix.reverse();
            canonical.extend(suffix);
            return canonical;
        }
        let (Some(name), Some(parent)) = (ancestor.file_name(), ancestor.parent()) else {
            return path.to_path_buf();
        };
        suffix.push(name.to_owned());
        ancestor = parent;
    }
}

/// First path in `a` that equals, contains or lies inside a path in `b`.
pub fn first_overlap<'a>(a: &'a [String], b: &[String]) -> Option<&'a str> {
    a.iter()
        .find(|left| {
            b.iter().any(|right| {
                Path::new(left.as_str()).starts_with(right)
                    || Path::new(right).starts_with(left.as_str())
            })
        })
        .map(String::as_str)
}

/// Tool error text for a spawn that overlaps a live sibling's files.
pub fn conflict_message(owner_description: &str, overlap: &str) -> String {
    format!(
        "File ownership conflict: `{overlap}` is already declared by running subagent \
         '{owner_description}'. Run this task after that subagent finishes, or spawn it with \
         isolation=\"worktree\"."
    )
}

/// Prompt block telling a child which siblings run in the same workspace.
pub fn siblings_block(siblings: &[SiblingFiles]) -> Option<String> {
    if siblings.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<live_siblings>\nOther subagents are running in this workspace. Do not edit files they \
         declared; if you need one, say so in your final report instead.\n",
    );
    for sibling in siblings.iter().take(MAX_LISTED_SIBLINGS) {
        out.push_str(&format!("- {}: ", sibling.description));
        if sibling.files.is_empty() {
            out.push_str("(no files declared)");
        } else {
            out.push_str(
                &sibling
                    .files
                    .iter()
                    .take(MAX_LISTED_FILES)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            if sibling.files.len() > MAX_LISTED_FILES {
                out.push_str(&format!(
                    ", +{} more",
                    sibling.files.len() - MAX_LISTED_FILES
                ));
            }
        }
        out.push('\n');
    }
    if siblings.len() > MAX_LISTED_SIBLINGS {
        out.push_str(&format!(
            "- +{} more running subagents\n",
            siblings.len() - MAX_LISTED_SIBLINGS
        ));
    }
    out.push_str("</live_siblings>");
    Some(out)
}

/// `prompt` followed by the sibling block, when there are siblings.
pub fn prompt_with_siblings(prompt: &str, siblings: &[SiblingFiles]) -> String {
    match siblings_block(siblings) {
        Some(block) => format!("{prompt}\n\n{block}"),
        None => prompt.to_owned(),
    }
}

/// Completion note listing `edited` paths that no declared path covers.
/// `None` when nothing was declared or every edit is covered.
pub fn outside_declared_note(declared: &[String], edited: &[String]) -> Option<String> {
    if declared.is_empty() {
        return None;
    }
    let mut outside: Vec<&str> = Vec::new();
    for path in edited {
        let covered = declared
            .iter()
            .any(|d| Path::new(path.as_str()).starts_with(d));
        if !covered && !outside.contains(&path.as_str()) {
            outside.push(path);
        }
    }
    if outside.is_empty() {
        return None;
    }
    let extra = outside.len().saturating_sub(MAX_LISTED_OUTSIDE);
    let mut listed = outside
        .into_iter()
        .take(MAX_LISTED_OUTSIDE)
        .collect::<Vec<_>>()
        .join(", ");
    if extra > 0 {
        listed.push_str(&format!(", +{extra} more"));
    }
    Some(format!(
        "<files_outside_declared>This subagent edited files outside its declared list: {listed}</files_outside_declared>"
    ))
}

#[cfg(test)]
#[path = "file_ownership_tests.rs"]
mod tests;
