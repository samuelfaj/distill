// Modified for Distill by Samuel Fajreldines, 2026.
//! End-to-end coverage of the out-of-process Mermaid render path.
//!
//! Spawns the **real** built pager binary as the hidden `__mermaid-render` child via [`render_via_subprocess`].
//! That is the exact function the render worker uses in production. It asserts:
//!   * a valid diagram (the cyclic login-flow) renders to a decodable PNG;
//!   * an oversized or invalid diagram is *contained*: the child exits non-zero, the parent returns `Err`, and no PNG is written;
//!   * a tight timeout makes the parent kill the child and return `Err` quickly: a real, process-killable timeout.
//!
//! These exercise the cross-platform `Command`, `current_exe()`, and `Child::kill` code against the actual binary.
//! The in-process worker unit tests cannot (under `cargo test` the harness binary is not the pager).
//!
//! Every test is `#[ignore]`: it needs the built binary, so `cargo test` skips it by default. Opt in with
//! `PAGER_BINARY=target/debug/distill cargo test -p distill-pager --test mermaid_render_subprocess -- --ignored`
//! (Bazel wires `PAGER_BINARY` from `//crates/codegen/distill-pager-bin:distill-pager`).
//! Unlike the spawn-only suites in `distill-pager-pty-harness`, this test stays here because it exercises pager library code.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use distill_pager::app::mermaid_worker::render_via_subprocess;
use distill_pager::scrollback::blocks::mermaid_content::MermaidRenderQuality;

/// `PAGER_BINARY` (absolutized: Bazel sets a runfiles-relative path) or `CARGO_BIN_EXE_distill`.
fn pager_binary() -> Result<PathBuf, String> {
    for key in ["PAGER_BINARY", "CARGO_BIN_EXE_distill"] {
        if let Some(value) = std::env::var_os(key) {
            let path = PathBuf::from(value);
            if path.exists() {
                return std::path::absolute(&path)
                    .map_err(|e| format!("failed to absolutize {key}={}: {e}", path.display()));
            }
        }
    }
    Err("PAGER_BINARY/CARGO_BIN_EXE_distill not set; build distill-pager-bin and export PAGER_BINARY".to_owned())
}

/// A cyclic login-flow whose back-edge (`Attempts -->|No| Enter`) routes back into the cycle, the tricky case for flowchart edge routing.
const LOGIN_FLOW: &str = "flowchart TD\n\
    Start([User visits login page]) --> Enter[Enter username & password]\n\
    Enter --> Submit[Submit credentials]\n\
    Submit --> Validate{Credentials valid?}\n\
    Validate -->|No| Fail[Show error message]\n\
    Fail --> Attempts{Too many failed attempts?}\n\
    Attempts -->|Yes| Lock[Lock account]\n\
    Attempts -->|No| Enter\n\
    Validate -->|Yes| Session[Create session]";

#[test]
#[ignore = "spawns the built pager binary; run with cargo test -- --ignored"]
fn child_renders_login_flow_to_png() {
    let bin = pager_binary().expect("resolve pager binary");
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("login.png");

    let result = render_via_subprocess(
        &bin,
        LOGIN_FLOW,
        false,
        1024,
        MermaidRenderQuality::Open,
        &out,
        Duration::from_secs(30),
    );

    assert!(
        result.is_ok(),
        "the login-flow must render via the __mermaid-render child: {result:?}"
    );
    assert!(out.exists(), "the child wrote the PNG to the out-path");
    let bytes = std::fs::read(&out).expect("read PNG");
    let img = image::load_from_memory(&bytes).expect("output is a decodable PNG");
    assert!(img.width() > 0 && img.height() > 0);
}

#[test]
#[ignore = "spawns the built pager binary; run with cargo test -- --ignored"]
fn oversized_source_is_contained() {
    // Source over the 64 KiB cap: the child rejects it and exits non-zero
    // The parent then returns Err and no PNG is produced (the render degrades to the fallback)
    let bin = pager_binary().expect("resolve pager binary");
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("huge.png");
    let huge = format!("flowchart TD\n{}", "A-->B\n".repeat(100_000));

    let result = render_via_subprocess(
        &bin,
        &huge,
        false,
        1024,
        MermaidRenderQuality::Terminal,
        &out,
        Duration::from_secs(30),
    );

    assert!(
        result.is_err(),
        "oversized source must be contained: {result:?}"
    );
    assert!(!out.exists(), "a contained (failed) child writes no PNG");
}

#[test]
#[ignore = "spawns the built pager binary; run with cargo test -- --ignored"]
fn invalid_diagram_is_contained() {
    // An unrenderable diagram: the child's render errors and it exits non-zero; the parent returns Err, no PNG
    // This is the same containment path a child panic (abort, then non-success exit) would take
    let bin = pager_binary().expect("resolve pager binary");
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("bad.png");

    let result = render_via_subprocess(
        &bin,
        "this is not a mermaid diagram at all",
        false,
        1024,
        MermaidRenderQuality::Terminal,
        &out,
        Duration::from_secs(30),
    );

    assert!(
        result.is_err(),
        "an invalid diagram must be contained: {result:?}"
    );
    assert!(!out.exists(), "no PNG for an unrenderable diagram");
}

#[test]
#[ignore = "spawns the built pager binary; run with cargo test -- --ignored"]
fn tight_timeout_kills_child_and_returns_err() {
    // A 1 ms budget cannot cover spawning and rendering, so the parent must kill and reap the child and return Err. It
    // must return promptly, not block on the child finishing.
    let bin = pager_binary().expect("resolve pager binary");
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("slow.png");

    let started = Instant::now();
    let result = render_via_subprocess(
        &bin,
        LOGIN_FLOW,
        false,
        1024,
        MermaidRenderQuality::Open,
        &out,
        Duration::from_millis(1),
    );
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a 1ms budget must time out: {result:?}");
    assert!(
        elapsed < Duration::from_secs(10),
        "the parent must return at the deadline (real kill), took {elapsed:?}",
    );
    assert!(!out.exists(), "a killed child leaves no PNG");
}
