// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn declared_files_resolve_against_child_cwd_and_dedupe() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let got = normalize_declared_files(
        &s(&["src/a.rs", "./src/x/../a.rs", "", "none", "src/b.rs"]),
        &root,
        None,
    );
    assert_eq!(
        got,
        vec![
            root.join("src/a.rs").to_string_lossy().into_owned(),
            root.join("src/b.rs").to_string_lossy().into_owned(),
        ]
    );
}

#[test]
fn declared_file_absolute_path_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let abs = root.join("lib.rs");
    let got =
        normalize_declared_files(&[abs.to_string_lossy().into_owned()], Path::new("/x"), None);
    assert_eq!(got, vec![abs.to_string_lossy().into_owned()]);
}

#[cfg(unix)]
#[test]
fn symlink_alias_and_real_path_declare_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join("real")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
    let real = normalize_declared_files(&s(&["real/new.rs"]), &root, None);
    let alias = normalize_declared_files(&s(&["alias/new.rs"]), &root, None);
    assert_eq!(real, alias);
}

#[test]
fn overlap_detects_equal_and_directory_containment_both_ways() {
    let a = s(&["/repo/src/a.rs"]);
    assert_eq!(
        first_overlap(&a, &s(&["/repo/src/a.rs"])),
        Some("/repo/src/a.rs")
    );
    assert_eq!(
        first_overlap(&a, &s(&["/repo/src"])),
        Some("/repo/src/a.rs")
    );
    assert_eq!(first_overlap(&s(&["/repo/src"]), &a), Some("/repo/src"));
}

#[test]
fn overlap_ignores_name_prefix_siblings_and_empty_lists() {
    let a = s(&["/repo/src/a.rs"]);
    assert_eq!(
        first_overlap(&a, &s(&["/repo/src/a.rs.bak", "/repo/src/ab.rs"])),
        None
    );
    assert_eq!(first_overlap(&a, &[]), None);
    assert_eq!(first_overlap(&[], &a), None);
}

#[test]
fn conflict_message_names_owner_and_both_remedies() {
    let message = conflict_message("refactor parser", "/repo/src/a.rs");
    assert!(message.contains("'refactor parser'"), "{message}");
    assert!(message.contains("/repo/src/a.rs"), "{message}");
    assert!(
        message.contains("after that subagent finishes"),
        "{message}"
    );
    assert!(message.contains("isolation=\"worktree\""), "{message}");
}

#[test]
fn siblings_block_absent_without_siblings() {
    assert_eq!(siblings_block(&[]), None);
    assert_eq!(prompt_with_siblings("do it", &[]), "do it");
}

#[test]
fn siblings_block_lists_description_and_files() {
    let block = prompt_with_siblings(
        "do it",
        &[
            SiblingFiles {
                description: "fix parser".into(),
                files: s(&["/r/a.rs", "/r/b.rs"]),
            },
            SiblingFiles {
                description: "explore".into(),
                files: Vec::new(),
            },
        ],
    );
    assert!(block.starts_with("do it\n\n<live_siblings>"), "{block}");
    assert!(block.contains("- fix parser: /r/a.rs, /r/b.rs"), "{block}");
    assert!(block.contains("- explore: (no files declared)"), "{block}");
}

#[test]
fn siblings_block_caps_siblings_at_eight() {
    let siblings: Vec<SiblingFiles> = (0..11)
        .map(|n| SiblingFiles {
            description: format!("job{n}"),
            files: s(&["/r/a.rs"]),
        })
        .collect();
    let block = siblings_block(&siblings).unwrap();
    assert!(block.contains("job7"), "{block}");
    assert!(!block.contains("job8"), "{block}");
    assert!(block.contains("+3 more running subagents"), "{block}");
}

#[test]
fn siblings_block_caps_files_per_sibling() {
    let files: Vec<String> = (0..13).map(|n| format!("/r/f{n}.rs")).collect();
    let block = siblings_block(&[SiblingFiles {
        description: "wide".into(),
        files,
    }])
    .unwrap();
    assert!(block.contains("/r/f9.rs"), "{block}");
    assert!(!block.contains("/r/f10.rs"), "{block}");
    assert!(block.contains("+3 more"), "{block}");
}

#[test]
fn outside_note_lists_only_uncovered_edits() {
    let declared = s(&["/r/src/a.rs", "/r/docs"]);
    let edited = s(&["/r/src/a.rs", "/r/docs/x.md", "/r/src/b.rs", "/r/src/b.rs"]);
    let note = outside_declared_note(&declared, &edited).unwrap();
    assert!(note.contains("/r/src/b.rs"), "{note}");
    assert!(!note.contains("/r/src/a.rs"), "{note}");
    assert!(!note.contains("x.md"), "{note}");
    assert_eq!(note.matches("/r/src/b.rs").count(), 1, "{note}");
}

#[test]
fn outside_note_absent_when_covered_or_undeclared() {
    assert_eq!(
        outside_declared_note(&s(&["/r/a.rs"]), &s(&["/r/a.rs"])),
        None
    );
    assert_eq!(outside_declared_note(&s(&["/r/a.rs"]), &[]), None);
    assert_eq!(outside_declared_note(&[], &s(&["/r/a.rs"])), None);
}
