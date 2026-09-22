// Modified for Distill by Samuel Fajreldines, 2026.
//! The reduction pipeline, as one function, so "flags off is today's bytes" is a
//! property a test can check instead of a promise.
//!
//! Three stages, cheapest first, each one refusing to act when it has nothing to
//! gain:
//!
//! 1. **read reuse** — bytes identical to something already sent become a pointer;
//! 2. **crushers** — deterministic transforms (ANSI/progress/class), with the
//!    lossy progress case stored before the reduced body is accepted;
//! 3. **importance extraction** — lossy, so the original is stored first and the
//!    marker names it, and only after the literal gate agrees.
//!
//! The cheap-model compression is deliberately **not** here: it costs a network
//! call and belongs to the caller, which has the client and the flags. This
//! function is what runs before any model is even considered, and it is what the
//! fixture tests drive.

use distill_workspace::jev::crushers;
use distill_workspace::jev::flags::JevLever;
use distill_workspace::jev::reduce;
use distill_workspace::jev::retention;

/// The flags this pipeline reads, resolved once by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaneFlags {
    pub crushers: bool,
    pub importance: bool,
    pub read_reuse: bool,
}

impl LaneFlags {
    /// Every lane off: the pipeline must then return the input untouched.
    pub const fn off() -> Self {
        Self {
            crushers: false,
            importance: false,
            read_reuse: false,
        }
    }
}

/// One stage's outcome, for the decision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRecord {
    /// The lever this record belongs to, so the caller records it without
    /// re-deriving the lane from its name.
    pub lever: JevLever,
    pub lane: &'static str,
    pub decision: &'static str,
    pub detail: String,
}

/// What the pipeline did to one payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneOutcome {
    pub body: String,
    pub records: Vec<LaneRecord>,
    /// Where the original was stored, when a lossy stage ran.
    pub store_handle: Option<String>,
    /// The document verdict this run decided on. The caller needs it for its own
    /// lanes, and taking it from here keeps one decision, not two.
    pub is_document: bool,
}

impl LaneOutcome {
    /// Fraction of the payload's bytes the pipeline removed.
    pub fn saved_fraction(&self, original: &str) -> f64 {
        if original.is_empty() {
            return 0.0;
        }
        1.0 - (self.body.len() as f64 / original.len() as f64)
    }
}

/// The states the read-reuse stage needs: "have these bytes been sent already?"
pub enum ReuseAnswer {
    /// First time: the caller remembers it, nothing changes.
    First,
    /// Already sent from this call: the payload becomes a pointer.
    Seen(String),
}

/// Where a lossy stage puts the original. `None` means the store refused, and
/// then the lossy stage must not run at all.
pub type Store = dyn Fn(&str) -> Option<String>;

/// The caller's own size threshold for each stage that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneLimits {
    /// Below this the crushers are not worth the scan.
    pub crushers_bytes: usize,
    pub reuse_bytes: usize,
    pub importance_bytes: usize,
}

/// Runs the pipeline. `reuse` is asked only when the flag is on and the payload
/// is big enough to be worth a lookup; `store` is called only when a lossy stage
/// is about to lose something.
///
/// Two guards sit in front of every rewriting stage, and both are decided here
/// so the live path and the tests cannot disagree about them:
///
/// - an exact-output call passes through untouched, because its payload is
///   line-addressed and the reader asked for the bytes;
/// - a document passes through the rewriting stages untouched, because there the
///   output *is* the answer and cutting a hole in it leaves something that still
///   looks complete.
pub fn reduce_payload(
    tool: &str,
    tool_command: &str,
    text: &str,
    flags: LaneFlags,
    limits: LaneLimits,
    reuse: &mut dyn FnMut(&str) -> ReuseAnswer,
    store: &Store,
) -> LaneOutcome {
    let mut records = Vec::new();
    let mut store_handle = None;
    let is_document = retention::looks_structured(tool_command, text);

    // The exact-output guard: the bytes are the answer, so nothing runs on them.
    // It must precede read reuse too: replacing an exact result with a pointer is
    // still a rewrite of a line-addressed answer.
    if crushers::is_exact_output(tool, tool_command) {
        records.push(LaneRecord {
            lever: JevLever::ECrushers,
            lane: "e_crushers",
            decision: "keep",
            detail: "the exact-output guard kept the bytes: this payload is \
                     line-addressed and the reader asked for it"
                .to_owned(),
        });
        return LaneOutcome {
            body: text.to_owned(),
            records,
            store_handle: None,
            is_document,
        };
    }

    // 1. Read reuse: identical bytes are already in the conversation.
    if flags.read_reuse && text.len() >= limits.reuse_bytes {
        let hash = reduce::content_hash(text);
        if let ReuseAnswer::Seen(first_at) = reuse(&hash) {
            records.push(LaneRecord {
                lever: JevLever::EReadReuse,
                lane: "e_read_reuse",
                decision: "reuse",
                detail: format!(
                    "{} bytes already in this conversation from {first_at} (sha {hash})",
                    text.len()
                ),
            });
            return LaneOutcome {
                body: reduce::reuse_note(&hash, &first_at, text.len()),
                records,
                store_handle: None,
                is_document,
            };
        }
    }

    let mut body = text.to_owned();

    // 2. The deterministic crushers. Most are content-preserving, but a
    //    recognisable progress bar deliberately drops intermediate frames. The
    //    latter stores the original before accepting the smaller body; a
    //    diagnostic frame that is not recognisably progress is never collapsed.
    if flags.crushers && !is_document && body.len() >= limits.crushers_bytes {
        let (cleaned, applied) = crushers::preclean(&body);
        if !applied.is_empty() && cleaned.len() < body.len() {
            if reduce::preserves_literals(&body, &cleaned) {
                records.push(LaneRecord {
                    lever: JevLever::ECrushers,
                    lane: "e_crushers",
                    decision: "crush",
                    detail: format!(
                        "{} bytes -> {} bytes via {}",
                        body.len(),
                        cleaned.len(),
                        applied.join("+")
                    ),
                });
                body = cleaned;
            } else if applied.contains(&"progress_bar_crusher") {
                if let Some(handle) = store(&body) {
                    records.push(LaneRecord {
                        lever: JevLever::ECrushers,
                        lane: "e_crushers",
                        decision: "crush",
                        detail: format!(
                            "{} bytes -> {} bytes via {}; raw output stored at {handle}",
                            body.len(),
                            cleaned.len(),
                            applied.join("+")
                        ),
                    });
                    body = format!(
                        "{}\n[raw output stored at {handle} — read that file for the collapsed progress frames]",
                        cleaned.trim_end()
                    );
                    store_handle = Some(handle);
                } else {
                    records.push(LaneRecord {
                        lever: JevLever::ECrushers,
                        lane: "e_crushers",
                        decision: "keep",
                        detail: "the progress crusher would lose intermediate frames, but the store refused the original; keeping today's bytes".to_owned(),
                    });
                }
            } else {
                records.push(LaneRecord {
                    lever: JevLever::ECrushers,
                    lane: "e_crushers",
                    decision: "keep",
                    detail: format!(
                        "the crushers would have dropped {} literal(s); keeping today's bytes",
                        reduce::lost_literals(&body, &cleaned).len()
                    ),
                });
            }
        }
    }

    // 3. Importance extraction: lossy, stored first, gated on the literals.
    if flags.importance && !is_document && body.len() >= limits.importance_bytes {
        let options = reduce::ExtractOptions::default();
        if let Some(extracted) = reduce::extract_important(&body, &options) {
            if !reduce::preserves_literals(&body, &extracted.text) {
                records.push(LaneRecord {
                    lever: JevLever::EImportance,
                    lane: "e_importance",
                    decision: "keep",
                    detail: format!(
                        "a reduction would have dropped {} literal(s); keeping today's bytes",
                        reduce::lost_literals(&body, &extracted.text).len()
                    ),
                });
            } else if let Some(handle) = store(&body) {
                records.push(LaneRecord {
                    lever: JevLever::EImportance,
                    lane: "e_importance",
                    decision: "extract",
                    detail: format!(
                        "{} bytes -> {} bytes, {} lines elided, stored at {handle}",
                        body.len(),
                        extracted.text.len(),
                        extracted.removed_lines
                    ),
                });
                body = format!(
                    "{}\n[full output stored at {handle} — read that file for the elided lines]",
                    extracted.text.trim_end()
                );
                store_handle = Some(handle);
            } else {
                records.push(LaneRecord {
                    lever: JevLever::EImportance,
                    lane: "e_importance",
                    decision: "keep",
                    detail: "the store refused the original; not losing a byte".to_owned(),
                });
            }
        }
    }

    LaneOutcome {
        body,
        records,
        store_handle,
        is_document,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain command whose output is worth reducing. Nothing about it is
    /// line-addressed or a document, so the fixtures below reach the stages they
    /// were written for; the guards have their own tests.
    const TOOL: &str = "run_terminal_command";
    const COMMAND: &str = "cargo build";
    const LIMITS: LaneLimits = LaneLimits {
        crushers_bytes: 2_000,
        reuse_bytes: 2_000,
        importance_bytes: 2_000,
    };

    fn no_reuse() -> impl FnMut(&str) -> ReuseAnswer {
        |_| ReuseAnswer::First
    }

    fn seen_reuse(_: &str) -> ReuseAnswer {
        ReuseAnswer::Seen("read_file(src/previous.txt)".to_owned())
    }

    fn refusing_store() -> impl Fn(&str) -> Option<String> {
        |_| None
    }

    fn build_log() -> String {
        let mut log = String::new();
        for i in 0..300 {
            log.push_str(&format!(
                "   Compiling crate-{i} v0.1.0 (/Users/x/y/crate-{i})\n       Fresh (0.4s)\n       Fresh (0.4s)\n\n"
            ));
        }
        log.push_str("error[E0308]: mismatched types\n  --> src/client.rs:868:5\n");
        log.push_str("    Finished `dev` profile in 120.5s\n");
        log
    }

    /// A crash report as a runtime prints one: locating frames plus the register
    /// dump nobody reads. The dump lines carry addresses, which are literals, so
    /// this fixture is the one the gate must refuse.
    fn stack_with_addresses() -> String {
        let mut out = String::from("thread 'main' panicked at crates/client/src/lib.rs:412:9:\n");
        out.push_str("caused by: timeout while waiting for the sampler\n");
        for i in 0..40 {
            out.push_str(&format!("             at crates/module_{i}/src/lib.rs:{}:9\n", 100 + i));
        }
        for i in 0..30 {
            out.push_str(&format!("0x00007f{i:08x}  0x0000000000000000  0x00007ffee{i:05x}\n"));
        }
        out
    }

    /// A crash report whose dump lines carry no addresses: the same shape, and
    /// here the reduction is allowed to fire.
    fn stack_without_addresses() -> String {
        let mut out = String::from("thread 'main' panicked at crates/client/src/lib.rs:412:9:\n");
        out.push_str("caused by: the sampler never answered\n");
        for i in 0..40 {
            out.push_str(&format!("             at crates/module_{i}/src/lib.rs:{}:9\n", 100 + i));
        }
        for _ in 0..30 {
            out.push_str("register state unavailable on this platform\n");
        }
        out
    }

    /// A test runner's report: failing cases with their assertion text, and the
    /// banner lines a reader never acts on.
    fn test_report() -> String {
        let mut out = String::from("============================= test session starts ==============================\n");
        out.push_str("platform here, runner present\n");
        out.push_str("plugins: anyio, xdist, cov, mock\n");
        out.push_str("collected files, running them now\n");
        for i in 0..60 {
            out.push_str("plugin line with short words only here\n");
            if i % 10 == 0 {
                out.push_str(&format!(
                    "FAILED tests/test_module_{i}.py::case_{i} - AssertionError: assert expected == actual\n"
                ));
            }
        }
        out.push_str("=== FAILURES ===\n");
        out.push_str("short test summary follows\n");
        out
    }

    /// A columnar listing: unique paths, one per row, padded for a human to read.
    fn padded_listing() -> String {
        let mut out = String::new();
        for i in 0..90 {
            out.push_str(&format!(
                "-rw-r--r--  1 root  root   4096 Jan  1 00:00 store/path_{i}/entry\n"
            ));
        }
        out
    }

    /// A recursive listing as a tool actually returns one: a header per
    /// directory, `total 0`, `.` and `..` entries, and the files under it. The
    /// repeated lines are what the crushers are allowed to collapse; the unique
    /// paths are what they must keep.
    fn listing() -> String {
        let mut out = String::new();
        for dir in 0..60 {
            out.push_str(&format!("./crates/module_{dir}:\n"));
            out.push_str("total 0\n");
            out.push_str("drwxr-xr-x  2 samuel staff   64 Sep 18 10:22 .\n");
            out.push_str("drwxr-xr-x  8 samuel staff  256 Sep 18 10:22 ..\n");
            for file in 0..4 {
                out.push_str(&format!(
                    "-rw-r--r--  1 samuel staff  4096 Sep 18 10:22 file_{file}.rs\n"
                ));
            }
        }
        out
    }

    /// A listing of nothing but unique lines is not reducible, and the pipeline
    /// must say so by changing nothing (the app's skip-set, C.1 #26).
    fn unique_listing() -> String {
        (0..300)
            .map(|i| format!("-rw-r--r--  1 samuel staff 4096 Sep 18 10:22 src/module_{i}.rs"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A diff whose context is code (no numbers of its own): dropping the
    /// context is then literal-preserving, which is what the diff crusher is for.
    fn diff() -> String {
        let mut out = String::from(
            "diff --git a/src/x.rs b/src/x.rs\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,40 +1,40 @@\n",
        );
        for _ in 0..200 {
            out.push_str(" use std::collections::BTreeMap;\n");
            out.push_str(" /// A doc comment the reader does not need again.\n");
        }
        out.push_str("-let old = 1;\n+let new = 2;\n");
        out
    }

    /// A large prose document as one actually reads: sentences with no numbers
    /// of their own, and a path only where the text points at the code. Those few
    /// lines are what the importance pass keeps; the filler sentences are what it
    /// elides — and the numbers rule is why the filler must not carry any.
    fn prose() -> String {
        let mut doc = String::new();
        for i in 0..120 {
            if i % 12 == 0 {
                doc.push_str(
                    "SECTION. The rule lives in crates/x/src/lib.rs and the tests that pin it sit \
beside it, in the same crate.\n\n\n\n",
                );
                continue;
            }
            doc.push_str(
                "The harness keeps the catalogues and the levers, and the model is only consulted \
when the cheap lane cannot decide, because a closed task with a guard is cheaper and safer \
than an open question. The rest of this paragraph exists to be long enough that a reader \
would rather skip it than read it twice, which is the whole point of the pass.\n\n\n\n",
            );
        }
        doc
    }

    /// The gate the plan cares about most: with every lane off, the pipeline is
    /// the identity — not "close to", byte for byte.
    #[test]
    fn with_every_flag_off_the_payload_is_byte_identical() {
        for fixture in [build_log(), listing(), unique_listing(), diff(), prose()] {
            let mut reuse = no_reuse();
            let outcome = reduce_payload(
                TOOL,
                COMMAND,
                &fixture,
                LaneFlags::off(),
                LIMITS,
                &mut reuse,
                &refusing_store(),
            );
            assert_eq!(outcome.body, fixture, "flags off must change nothing");
            assert!(outcome.records.is_empty(), "and must record nothing");
            assert!(outcome.store_handle.is_none());
            assert_eq!(outcome.saved_fraction(&fixture), 0.0);
        }
    }

    /// The fixture set the plan names, through the shipped pipeline: each one
    /// shrinks, keeps its literals, and the original is retrievable.
    #[test]
    fn the_fixture_set_shrinks_and_every_literal_survives() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dir = dir.path().to_path_buf();
        let store = move |payload: &str| {
            crate::jev_store::store_payload_in(&dir, payload).map(|path| path.display().to_string())
        };
        let flags = LaneFlags {
            crushers: true,
            importance: true,
            read_reuse: true,
        };

        // A payload with nothing to collapse is left alone, on purpose.
        let unique = unique_listing();
        let mut reuse = no_reuse();
        let untouched = reduce_payload(TOOL, COMMAND, &unique, flags, LIMITS, &mut reuse, &store);
        assert_eq!(untouched.body, unique, "a unique listing is not reducible");

        for (name, fixture) in [
            ("build log", build_log()),
            ("listing", listing()),
            ("prose", prose()),
        ] {
            let mut reuse = no_reuse();
            let outcome = reduce_payload(TOOL, COMMAND, &fixture, flags, LIMITS, &mut reuse, &store);
            let saved = outcome.saved_fraction(&fixture);
            println!(
                "{name}: {} -> {} bytes ({:.1}% saved)",
                fixture.len(),
                outcome.body.len(),
                saved * 100.0
            );

            assert!(saved > 0.10, "{name} barely shrank: {saved}");
            // The marker counts what went, and the original reads back exactly.
            if let Some(handle) = &outcome.store_handle {
                let stored = std::fs::read_to_string(handle).expect("handle reads");
                assert_eq!(stored, fixture, "{name}: the store must be byte-exact");
                assert!(outcome.body.contains(handle), "{name}: the marker names it");
                assert!(outcome.body.contains("lines elided"));
            }
            // Nothing a reader acts on is gone: paths, `file:line`, numbers and
            // the error vocabulary are all still there.
            let lost = reduce::lost_literals(&fixture, &outcome.body);
            assert!(lost.is_empty(), "{name} lost literals: {lost:#?}");
            assert!(!outcome.records.is_empty(), "{name} recorded nothing");
        }

        // A diff is a document: the payload *is* the answer the reader asked for,
        // so both rewriting stages stay out of it and the bytes are identical.
        let diff = diff();
        let mut reuse = no_reuse();
        let outcome = reduce_payload(TOOL, COMMAND, &diff, flags, LIMITS, &mut reuse, &store);
        assert!(outcome.is_document, "a unified diff is a document");
        assert_eq!(
            outcome.body, diff,
            "and a document is never rewritten, whatever its class"
        );
    }

    /// A reduction that would drop a literal is refused, and the payload is kept
    /// whole even though the store was available: this is the gate, not a hope.
    #[test]
    fn a_reduction_that_would_lose_a_literal_is_refused() {
        // A payload whose literals live in the middle, where the extraction
        // elides: the gate must notice and keep the bytes.
        let mut payload = String::new();
        for i in 0..400 {
            payload.push_str(&format!("filler line {i} with no structure at all\n"));
        }
        payload.push_str("error: /tmp/important/path.rs:12 could not be found at 42ms\n");
        for i in 0..400 {
            payload.push_str(&format!("more filler {i}\n"));
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let dir = dir.path().to_path_buf();
        let store = move |payload: &str| {
            crate::jev_store::store_payload_in(&dir, payload).map(|path| path.display().to_string())
        };
        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            TOOL,
            COMMAND,
            &payload,
            LaneFlags {
                crushers: false,
                importance: true,
                read_reuse: false,
            },
            LIMITS,
            &mut reuse,
            &store,
        );
        // The filler lines carry numbers (`filler line 0` … `filler line 399`)
        // and the elided middle would take most of them: the gate refuses, the
        // body is untouched, and the record says why.
        assert_eq!(outcome.body, payload, "a refused reduction changes nothing");
        assert!(outcome.store_handle.is_none(), "and stores nothing");
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.records[0].decision, "keep");
        assert!(
            outcome.records[0].detail.contains("literal"),
            "the record names the reason: {}",
            outcome.records[0].detail
        );

        // The same payload with its numbers given no literal weight (nothing in
        // the middle to lose) does reduce, and the store carries the original.
        let mut reversible = String::new();
        for _ in 0..400 {
            reversible.push_str("filler line with no structure at all\n");
        }
        reversible.push_str("error: src/main.rs:12 could not be found\n");
        for _ in 0..400 {
            reversible.push_str("more filler with no digits\n");
        }
        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            TOOL,
            COMMAND,
            &reversible,
            LaneFlags {
                crushers: false,
                importance: true,
                read_reuse: false,
            },
            LIMITS,
            &mut reuse,
            &store,
        );
        assert!(outcome.body.len() < reversible.len(), "this one reduces");
        assert!(outcome.body.contains("error: src/main.rs:12"));
        let handle = outcome.store_handle.expect("the store kept it");
        assert_eq!(
            std::fs::read_to_string(&handle).expect("reads"),
            reversible,
            "byte-exact recovery"
        );
    }

    /// Every newly reachable class, driven through the shipped pipeline: the
    /// transform the class table names is the one that fires, and nothing a
    /// reader acts on is lost.
    #[test]
    fn each_new_class_fires_the_transform_the_table_names() {
        let flags = LaneFlags {
            crushers: true,
            importance: false,
            read_reuse: false,
        };
        let store = refusing_store();

        // The columnar pass is the clearest new win: the rows are unique, so the
        // repeat collapse has nothing to do and the padding is the whole gain.
        let listing = padded_listing();
        let mut reuse = no_reuse();
        let outcome = reduce_payload(TOOL, COMMAND, &listing, flags, LIMITS, &mut reuse, &store);
        assert!(
            outcome.body.len() < listing.len(),
            "a padded listing must shrink"
        );
        let detail = &outcome.records[0].detail;
        assert!(
            detail.contains("padded_table_compact"),
            "the columnar pass is what fires: {detail}"
        );
        assert!(
            !outcome.body.contains("   "),
            "alignment runs collapse to a tab"
        );
        assert!(
            reduce::lost_literals(&listing, &outcome.body).is_empty(),
            "every path survives the collapse"
        );

        // A test report: the banner lines go, the failures and their assertion
        // text stay.
        let report = test_report();
        let mut reuse = no_reuse();
        let outcome = reduce_payload(TOOL, COMMAND, &report, flags, LIMITS, &mut reuse, &store);
        let detail = outcome
            .records
            .first()
            .map(|record| record.detail.clone())
            .unwrap_or_default();
        assert!(
            detail.contains("test_crusher"),
            "the test pass fires; records were {:?}",
            outcome.records
        );
        assert!(
            outcome.body.contains("AssertionError: assert expected == actual"),
            "the assertion text is the thing a reader acts on"
        );
        assert!(
            reduce::lost_literals(&report, &outcome.body).is_empty(),
            "and no literal goes with it"
        );

        // A crash report with no addresses in its dump: the stack pass fires.
        let stack = stack_without_addresses();
        let mut reuse = no_reuse();
        let outcome = reduce_payload(TOOL, COMMAND, &stack, flags, LIMITS, &mut reuse, &store);
        let detail = &outcome.records[0].detail;
        assert!(detail.contains("stack_crusher"), "the stack pass fires: {detail}");
        assert!(
            outcome.body.contains("panicked at crates/client/src/lib.rs:412:9"),
            "the line that says where it broke stays"
        );
        assert!(
            reduce::lost_literals(&stack, &outcome.body).is_empty(),
            "no literal goes with the dumps"
        );
    }

    /// The literal gate is what decides the aggressive class transforms, and it
    /// says no when the bytes it would drop carry something a reader may need:
    /// the addresses in a register dump, or a lockfile's versions.
    #[test]
    fn a_class_transform_that_would_lose_a_literal_is_refused_by_the_chain() {
        let flags = LaneFlags {
            crushers: true,
            importance: false,
            read_reuse: false,
        };
        let store = refusing_store();
        let stack = stack_with_addresses();
        let mut reuse = no_reuse();
        let outcome = reduce_payload(TOOL, COMMAND, &stack, flags, LIMITS, &mut reuse, &store);
        assert!(
            !outcome.body.contains("stack_crusher"),
            "the stack pass must not be recorded as applied: {:?}",
            outcome.records
        );
        let lost = reduce::lost_literals(&stack, &outcome.body);
        assert!(
            lost.is_empty(),
            "whatever the chain did, the addresses are still there: {lost:#?}"
        );
    }

    /// The guard the catalogue calls `distill_exact_rg`: a line-addressed payload
    /// is never rewritten, whichever tool produced it.
    #[test]
    fn an_exact_output_call_is_never_rewritten() {
        let flags = LaneFlags {
            crushers: true,
            importance: true,
            read_reuse: true,
        };
        let store = refusing_store();
        for (tool, command) in [
            ("grep", "rg -n timeout src"),
            ("run_terminal_command", "grep -R timeout src"),
            ("run_terminal_command", "sed -n 1,40p src/main.rs"),
            ("run_terminal_command", "git show HEAD"),
            ("run_terminal_command", "awk '{print $1}' out.txt"),
        ] {
            let payload = padded_listing();
            let mut reuse = no_reuse();
            let outcome = reduce_payload(tool, command, &payload, flags, LIMITS, &mut reuse, &store);
            assert_eq!(
                outcome.body, payload,
                "{tool} {command} is line-addressed and must pass through byte for byte"
            );
            assert_eq!(outcome.records[0].decision, "keep");
            assert!(
                outcome.records[0].detail.contains("exact-output"),
                "the record names the guard: {}",
                outcome.records[0].detail
            );
        }
    }

    #[test]
    fn exact_output_wins_before_read_reuse() {
        let payload = padded_listing();
        let mut reuse = seen_reuse;
        let outcome = reduce_payload(
            "grep",
            "rg -n timeout src",
            &payload,
            LaneFlags {
                crushers: true,
                importance: true,
                read_reuse: true,
            },
            LIMITS,
            &mut reuse,
            &refusing_store(),
        );
        assert_eq!(outcome.body, payload);
        assert_eq!(outcome.records[0].lane, "e_crushers");
        assert_eq!(outcome.records[0].decision, "keep");
    }

    #[test]
    fn progress_shrinks_but_diagnostics_keep_or_recover_the_raw_output() {
        let job = "downloading registry.example.invalid/packages/a-really-long-package-name-with-build-metadata";
        let mut progress = String::new();
        for percent in 0..100 {
            progress.push_str(&format!("{job} {percent}%\r"));
        }
        progress.push_str(&format!("{job} 100% done\n"));
        let original = progress.clone();
        let dir = tempfile::tempdir().expect("temp dir");
        let dir = dir.path().to_path_buf();
        let store = move |payload: &str| {
            crate::jev_store::store_payload_in(&dir, payload).map(|path| path.display().to_string())
        };
        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            TOOL,
            COMMAND,
            &progress,
            LaneFlags {
                crushers: true,
                importance: false,
                read_reuse: false,
            },
            LIMITS,
            &mut reuse,
            &store,
        );
        assert!(outcome.body.len() < original.len(), "progress should shrink");
        let handle = outcome.store_handle.as_deref().expect("raw frames stored");
        assert_eq!(std::fs::read_to_string(handle).unwrap(), original);
        assert!(outcome.body.contains(handle));

        let diagnostic = "error: src/main.rs:12 exit code 1\rdownloading 100%\n";
        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            TOOL,
            COMMAND,
            diagnostic,
            LaneFlags {
                crushers: true,
                importance: false,
                read_reuse: false,
            },
            LaneLimits {
                crushers_bytes: 1,
                ..LIMITS
            },
            &mut reuse,
            &refusing_store(),
        );
        assert_eq!(outcome.body, diagnostic, "diagnostics are never overwritten");
        assert!(outcome.store_handle.is_none());
    }

    /// A document is the answer the model asked for, so the rewriting stages stay
    /// out of it even when the payload would otherwise shrink.
    #[test]
    fn a_document_is_never_rewritten() {
        let flags = LaneFlags {
            crushers: true,
            importance: true,
            read_reuse: true,
        };
        let store = refusing_store();
        let payload = build_log();
        for command in ["cat src/main.rs", "jq . package.json", "git show HEAD:src/lib.rs"] {
            let mut reuse = no_reuse();
            let outcome = reduce_payload(
                "run_terminal_command",
                command,
                &payload,
                flags,
                LIMITS,
                &mut reuse,
                &store,
            );
            assert_eq!(
                outcome.body, payload,
                "{command} dumps a document: the bytes are the answer"
            );
            assert!(outcome.is_document, "{command} is a document producer");
        }
    }

    /// Skill text stays verbatim whichever tool read it, and so does a payload
    /// whose class has no gain.
    #[test]
    fn skill_text_and_a_no_gain_payload_are_never_rewritten() {
        let flags = LaneFlags {
            crushers: true,
            importance: true,
            read_reuse: true,
        };
        let store = refusing_store();
        let payload = padded_listing();

        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            "read_file",
            "/Users/x/.grok/skills/deploy/SKILL.md",
            &payload,
            flags,
            LIMITS,
            &mut reuse,
            &store,
        );
        assert_eq!(outcome.body, payload, "skill bodies stay verbatim");

        // With the crushers off, the same payload that shrank above is untouched.
        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            TOOL,
            COMMAND,
            &payload,
            LaneFlags::off(),
            LIMITS,
            &mut reuse,
            &store,
        );
        assert_eq!(outcome.body, payload, "levers off is today's bytes");
        assert!(outcome.records.is_empty());
    }

    /// A repeated payload becomes a pointer, and only a repeated one.
    #[test]
    fn a_repeated_payload_becomes_a_pointer_to_the_first_copy() {
        let fixture = build_log();
        let mut seen: Option<String> = None;
        let store = refusing_store();
        let flags = LaneFlags {
            crushers: true,
            importance: true,
            read_reuse: true,
        };

        let mut reuse = |hash: &str| match &seen {
            Some(first) if first == hash => ReuseAnswer::Seen("read_file(src/a.rs)".to_owned()),
            _ => {
                seen = Some(hash.to_owned());
                ReuseAnswer::First
            }
        };
        let first = reduce_payload(TOOL, COMMAND, &fixture, flags, LIMITS, &mut reuse, &store);
        assert!(
            first.body.len() < fixture.len(),
            "the first send still shrinks"
        );
        assert!(
            first
                .records
                .iter()
                .all(|record| record.lane != "e_read_reuse"),
            "the first send is not a reuse"
        );

        let second = reduce_payload(TOOL, COMMAND, &fixture, flags, LIMITS, &mut reuse, &store);
        assert!(
            second.body.contains("already sent this session"),
            "the second send is a pointer: {}",
            second.body
        );
        assert!(second.body.len() < 200);
        assert_eq!(second.records[0].lane, "e_read_reuse");
    }
}
