//! The reduction pipeline, as one function, so "flags off is today's bytes" is a
//! property a test can check instead of a promise.
//!
//! Three stages, cheapest first, each one refusing to act when it has nothing to
//! gain:
//!
//! 1. **read reuse** — bytes identical to something already sent become a pointer;
//! 2. **crushers** — the deterministic transforms (ANSI/progress/class), which
//!    cannot lose a literal by construction;
//! 3. **importance extraction** — lossy, so the original is stored first and the
//!    marker names it, and only after the literal gate agrees.
//!
//! The cheap-model compression is deliberately **not** here: it costs a network
//! call and belongs to the caller, which has the client and the flags. This
//! function is what runs before any model is even considered, and it is what the
//! fixture tests drive.

use xai_grok_workspace::jev::crushers;
use xai_grok_workspace::jev::reduce;

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

/// Runs the pipeline. `reuse` is asked only when the flag is on and the payload
/// is big enough to be worth a lookup; `store` is called only when a lossy stage
/// is about to lose something.
pub fn reduce_payload(
    text: &str,
    flags: LaneFlags,
    min_bytes: usize,
    reuse: &mut dyn FnMut(&str) -> ReuseAnswer,
    store: &Store,
) -> LaneOutcome {
    let mut records = Vec::new();
    let mut store_handle = None;

    // 1. Read reuse: identical bytes are already in the conversation.
    if flags.read_reuse && text.len() >= min_bytes {
        let hash = reduce::content_hash(text);
        match reuse(&hash) {
            ReuseAnswer::Seen(first_at) => {
                records.push(LaneRecord {
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
                };
            }
            ReuseAnswer::First => {}
        }
    }

    let mut body = text.to_owned();

    // 2. The deterministic crushers. Some of them drop lines (a diff's context,
    //    a stack's dumps), so the literal gate runs here as well: a crusher that
    //    would take a path, a `file:line`, a number or an error word with it is
    //    refused and the payload stays whole.
    if flags.crushers {
        let (cleaned, applied) = crushers::preclean(&body);
        if !applied.is_empty() && cleaned.len() < body.len() {
            if reduce::preserves_literals(&body, &cleaned) {
                records.push(LaneRecord {
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
            } else {
                records.push(LaneRecord {
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
    if flags.importance && body.len() >= min_bytes {
        let options = reduce::ExtractOptions::default();
        if let Some(extracted) = reduce::extract_important(&body, &options) {
            if !reduce::preserves_literals(&body, &extracted.text) {
                records.push(LaneRecord {
                    lane: "e_importance",
                    decision: "keep",
                    detail: format!(
                        "a reduction would have dropped {} literal(s); keeping today's bytes",
                        reduce::lost_literals(&body, &extracted.text).len()
                    ),
                });
            } else if let Some(handle) = store(&body) {
                records.push(LaneRecord {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_reuse() -> impl FnMut(&str) -> ReuseAnswer {
        |_| ReuseAnswer::First
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
        let mut out = String::from("diff --git a/src/x.rs b/src/x.rs\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,40 +1,40 @@\n");
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
                &fixture,
                LaneFlags::off(),
                2_000,
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
            crate::jev_store::store_payload_in(&dir, payload)
                .map(|path| path.display().to_string())
        };
        let flags = LaneFlags {
            crushers: true,
            importance: true,
            read_reuse: true,
        };

        // A payload with nothing to collapse is left alone, on purpose.
        let unique = unique_listing();
        let mut reuse = no_reuse();
        let untouched = reduce_payload(&unique, flags, 2_000, &mut reuse, &store);
        assert_eq!(untouched.body, unique, "a unique listing is not reducible");

        for (name, fixture) in [
            ("build log", build_log()),
            ("listing", listing()),
            ("diff", diff()),
            ("prose", prose()),
        ] {
            let mut reuse = no_reuse();
            let outcome = reduce_payload(&fixture, flags, 2_000, &mut reuse, &store);
            let saved = outcome.saved_fraction(&fixture);
            println!("{name}: {} -> {} bytes ({:.1}% saved)", fixture.len(), outcome.body.len(), saved * 100.0);

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
            crate::jev_store::store_payload_in(&dir, payload)
                .map(|path| path.display().to_string())
        };
        let mut reuse = no_reuse();
        let outcome = reduce_payload(
            &payload,
            LaneFlags {
                crushers: false,
                importance: true,
                read_reuse: false,
            },
            2_000,
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
            &reversible,
            LaneFlags {
                crushers: false,
                importance: true,
                read_reuse: false,
            },
            2_000,
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
        let first = reduce_payload(&fixture, flags, 2_000, &mut reuse, &store);
        assert!(first.body.len() < fixture.len(), "the first send still shrinks");
        assert!(
            first.records.iter().all(|record| record.lane != "e_read_reuse"),
            "the first send is not a reuse"
        );

        let second = reduce_payload(&fixture, flags, 2_000, &mut reuse, &store);
        assert!(
            second.body.contains("already sent this session"),
            "the second send is a pointer: {}",
            second.body
        );
        assert!(second.body.len() < 200);
        assert_eq!(second.records[0].lane, "e_read_reuse");
    }
}
