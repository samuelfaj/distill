use super::*;
use crate::auto_update::STALE_TMP_AGE;
use std::time::Duration;

fn make_all_stale(dir: &Path) {
    let old = std::time::SystemTime::now() - (STALE_TMP_AGE + Duration::from_secs(60));
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_file() {
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn executable_is_in_use_detects_this_test_process() {
    let exe = std::env::current_exe().expect("current_exe");
    assert!(
        executable_is_in_use(&exe),
        "process table must see {}",
        exe.display()
    );
}

#[test]
fn executable_is_in_use_is_false_for_missing_binary() {
    assert!(!executable_is_in_use(Path::new("/no/such/grok-binary")));
}

#[cfg(unix)]
#[test]
fn executable_is_in_use_matches_hardlink() {
    let exe = std::env::current_exe().expect("current_exe");
    let link = exe.with_file_name(format!(
        "{}.hardlink-{}",
        exe.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::hard_link(&exe, &link).expect("hardlink next to current_exe");
    let seen = executable_is_in_use(&link);
    let _ = std::fs::remove_file(&link);
    assert!(
        seen,
        "inode of {} must match {}",
        link.display(),
        exe.display()
    );
}

#[tokio::test]
async fn cleanup_keeps_an_older_binary_marked_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for v in ["0.1.140", "0.1.141", "0.1.142", "0.1.143", "0.1.144"] {
        std::fs::write(d.join(format!("grok-{v}-macos-aarch64")), v).unwrap();
    }
    std::fs::write(d.join("grok-0.1.145-macos-aarch64"), "current").unwrap();
    make_all_stale(d);

    let live = d.join("grok-0.1.140-macos-aarch64");
    cleanup_old_downloads_with(d, "grok", "0.1.145", |path| path == live).await;

    assert!(d.join("grok-0.1.145-macos-aarch64").exists(), "current");
    assert!(d.join("grok-0.1.144-macos-aarch64").exists(), "N-1");
    assert!(live.exists(), "in-use");
    assert!(!d.join("grok-0.1.143-macos-aarch64").exists(), "idle");
}
