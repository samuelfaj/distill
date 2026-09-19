// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use std::fs;
use tempfile::TempDir;

fn at<T>(xs: &[T], i: usize) -> &T {
    let Some(x) = xs.get(i) else {
        panic!("expected index {i}, len {}", xs.len());
    };
    x
}

#[test]
fn collects_top_level_files_with_flat_names() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("chat_history.jsonl"), b"line1\nline2").unwrap();
    fs::write(dir.path().join("summary.json"), b"{}").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    files.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(files.len(), 2);
    assert_eq!(at(&files, 0).name, "chat_history.jsonl");
    assert_eq!(at(&files, 0).data, b"line1\nline2");
    assert_eq!(at(&files, 1).name, "summary.json");
    assert_eq!(at(&files, 1).data, b"{}");
}

#[test]
fn collects_subdirectory_files_with_relative_paths() {
    let dir = TempDir::new().unwrap();
    let prompts_dir = dir.path().join("prompts");
    fs::create_dir(&prompts_dir).unwrap();
    fs::write(prompts_dir.join("prompt_0.txt"), b"long prompt content").unwrap();
    fs::write(prompts_dir.join("prompt_1.txt"), b"another long prompt").unwrap();
    fs::write(dir.path().join("summary.json"), b"{}").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    files.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(files.len(), 3);
    assert_eq!(at(&files, 0).name, "prompts/prompt_0.txt");
    assert_eq!(at(&files, 0).data, b"long prompt content");
    assert_eq!(at(&files, 1).name, "prompts/prompt_1.txt");
    assert_eq!(at(&files, 2).name, "summary.json");
}

#[test]
fn collects_nested_subdirectories() {
    let dir = TempDir::new().unwrap();
    let deep = dir.path().join("a").join("b");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("deep.txt"), b"deep").unwrap();
    fs::write(dir.path().join("top.txt"), b"top").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    files.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(files.len(), 2);
    assert_eq!(at(&files, 0).name, "a/b/deep.txt");
    assert_eq!(at(&files, 1).name, "top.txt");
}

#[test]
fn skips_feedback_draft_artifacts_at_every_depth() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    for path in [
        dir.path().join(distill_feedback::FEEDBACK_DRAFTS_FILENAME),
        dir.path()
            .join(distill_feedback::FEEDBACK_DRAFTS_LOCK_FILENAME),
        nested.join(format!(
            "{}123",
            distill_feedback::FEEDBACK_DRAFTS_TEMP_PREFIX
        )),
        nested.join(distill_feedback::FEEDBACK_DRAFTS_FILENAME),
    ] {
        fs::write(path, b"private draft").unwrap();
    }
    fs::write(nested.join("trace.jsonl"), b"trace").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    assert_eq!(files.len(), 1);
    assert_eq!(at(&files, 0).name, "nested/trace.jsonl");
    assert_eq!(at(&files, 0).data, b"trace");
}

#[test]
fn skips_feedback_draft_images_directory() {
    let dir = TempDir::new().unwrap();
    let draft_id =
        distill_feedback::FeedbackDraftId::from("01931111-aaaa-7bbb-8ccc-ddddeeeeffff".to_owned());
    let images = distill_feedback::feedback_draft_images_dir(dir.path(), &draft_id);
    fs::create_dir_all(&images).unwrap();
    fs::write(images.join("0.png"), b"\x89PNG screenshot bytes").unwrap();
    fs::write(
        images.join("metadata.json"),
        br#"[{"fileName":"0.png","mimeType":"image/png","byteLen":21}]"#,
    )
    .unwrap();
    fs::write(dir.path().join("trace.jsonl"), b"trace").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["trace.jsonl"]);
}

#[cfg(unix)]
#[test]
fn skips_feedback_draft_hardlink_under_another_name() {
    let dir = TempDir::new().unwrap();
    let draft = dir.path().join(distill_feedback::FEEDBACK_DRAFTS_FILENAME);
    fs::write(&draft, b"private draft").unwrap();
    fs::hard_link(&draft, dir.path().join("innocent.json")).unwrap();
    fs::write(dir.path().join("trace.jsonl"), b"trace").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    assert_eq!(files.len(), 1);
    assert_eq!(at(&files, 0).name, "trace.jsonl");
}

#[test]
fn nonexistent_directory_returns_empty() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does_not_exist");

    let mut files = Vec::new();
    collect_session_files_recursive(&missing, &missing, &mut files);

    assert!(files.is_empty());
}

#[test]
fn empty_directory_returns_empty() {
    let dir = TempDir::new().unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    assert!(files.is_empty());
}

#[test]
fn skips_empty_subdirectories() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("empty_subdir")).unwrap();
    fs::write(dir.path().join("file.txt"), b"data").unwrap();

    let mut files = Vec::new();
    collect_session_files_recursive(dir.path(), dir.path(), &mut files);

    assert_eq!(files.len(), 1);
    assert_eq!(at(&files, 0).name, "file.txt");
}
