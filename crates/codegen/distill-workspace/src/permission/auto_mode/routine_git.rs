//! Fail-closed `git` arm of the routine-bash heuristic: any shape not recognized here goes to the model.
//! Callers pass wrapper-peeled words with `words[0]` literally `git`; a case-variant or path-qualified spelling may resolve to a different binary.

use crate::permission::exec_risk::{
    git_words_are_read_only_query, git_words_have_unsafe_query_option,
};

pub(super) fn git_words_are_routine(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("git") {
        return false;
    }
    if git_words_are_read_only_query(words) {
        return true;
    }
    if git_words_have_unsafe_query_option(words) {
        return false;
    }
    let Some(verb) = words.get(1).map(String::as_str) else {
        return false;
    };
    let Some(args) = words.get(2..) else {
        return false;
    };
    match verb {
        "add" | "commit" | "pull" | "fetch" => true,
        "worktree" => args.first().map(String::as_str) == Some("list"),
        "checkout" | "switch" => is_routine_branch_switch(args),
        "stash" => is_routine_stash(args),
        _ => false,
    }
}

/// Exact spellings only, so clusters (`-qf`) and abbreviations (`--fo`) fail closed.
const BRANCH_SWITCH_BENIGN_FLAGS: &[&str] = &[
    "-q",
    "--quiet",
    "-d",
    "--detach",
    "-t",
    "--track",
    "--no-track",
    "--guess",
    "--no-guess",
    "--progress",
    "--no-progress",
    "--recurse-submodules",
    "--no-recurse-submodules",
];

/// An unlisted option or a second operand means path mode or a discard, so both fail closed.
fn is_routine_branch_switch(args: &[String]) -> bool {
    let mut operands = 0;
    let mut it = args.iter().map(String::as_str);
    while let Some(word) = it.next() {
        match word {
            "-b" | "-B" | "-c" | "-C" | "--orphan" => {
                it.next();
            }
            // Bare `-` is the previous branch
            "-" => operands += 1,
            _ if BRANCH_SWITCH_BENIGN_FLAGS.contains(&word) => {}
            _ if word.starts_with('-') => return false,
            _ => {
                if operand_reads_as_path(word) {
                    return false;
                }
                operands += 1;
            }
        }
    }
    operands <= 1
}

/// `drop` and `clear` throw away a stash, so only the recoverable subcommands are listed.
fn is_routine_stash(args: &[String]) -> bool {
    let mut it = args.iter().map(String::as_str);
    let subcommand = loop {
        match it.next() {
            Some("-m" | "--message") => {
                it.next();
            }
            Some("-q" | "--quiet") => {}
            Some(word) if word.starts_with('-') => return false,
            other => break other,
        }
    };
    matches!(
        subcommand,
        None | Some("push" | "save" | "pop" | "apply" | "list" | "show" | "branch")
    )
}

/// A dotless bare name (`Makefile`, `src`) is indistinguishable from a branch without a repo lookup and stays routine.
fn operand_reads_as_path(op: &str) -> bool {
    // git-check-ref-format rejects these in a branch name, so the operand is a pathspec or a revision expression like `HEAD~1`
    if op
        .split('/')
        .any(|c| c.starts_with('.') || c.ends_with(".lock"))
        || op.ends_with('/')
        || op.ends_with('.')
        || op.contains("..")
        || op.contains("@{")
        || op.contains(['~', '^', ':', '\\'])
        || op.contains(char::is_whitespace)
    {
        return true;
    }
    if op.starts_with('/') || op.contains(['*', '?', '[']) {
        return true;
    }
    let Some((stem, ext)) = op.rsplit_once('.') else {
        return false;
    };
    // `2.x` / `release/1.2.x` are maintenance branches, not a one-letter file extension
    let stem_leaf = stem.rsplit('/').next().unwrap_or(stem);
    if ext == "x"
        && stem_leaf.bytes().any(|b| b.is_ascii_digit())
        && stem_leaf.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        return false;
    }
    (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
#[path = "routine_git_tests.rs"]
mod tests;
