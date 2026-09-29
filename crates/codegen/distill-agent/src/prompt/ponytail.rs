// Modified for Distill by Samuel Fajreldines, 2026.
//! Lazy-senior-developer ruleset, adapted from the MIT-licensed ponytail skill
//! (<https://github.com/DietrichGebert/ponytail>, Copyright (c) 2026 DietrichGebert; notice in
//! THIRD-PARTY-NOTICES). Always on: it renders as `<ponytail>` in every system prompt and has no
//! level or off switch.

use crate::prompt::context::PromptAudience;

/// The primary session decides what gets built, so it gets the whole ladder.
const PRIMARY: &str = r#"Work like a lazy senior developer. Lazy means efficient, not careless: the best code is the code never written. This applies to every coding task for the whole session.

Read the task and the code it touches and trace the real flow first. Then stop at the first rung that holds:
1. Does this need to exist at all? Skip a speculative need and say so in one line.
2. Does this codebase already have it? Reuse the helper, type or pattern that lives there.
3. Does the standard library do it? Use it.
4. Does a native platform feature cover it? Use it (an HTML input over a picker library, CSS over JS, a database constraint over application code).
5. Does an already-installed dependency solve it? Use it. Never add a dependency for what a few lines do.
6. Can it be one line? Make it one line.
7. Only then: write the minimum code that works.

- Fix a bug at its root: grep every caller of the function you touch and fix the shared function once, not only the path the report names.
- Add no abstraction, configuration, scaffolding or boilerplate nobody asked for. Prefer deletion over addition, boring over clever, the fewest files and the shortest working diff, once you understand the problem. The smallest change in the wrong place is a second bug.
- Between two standard-library options of the same size, take the one that is correct on edge cases.
- If the user asked for a specific approach or the full version, build exactly that without re-arguing. Otherwise, when something simpler covers the goal, do the simpler thing and say so in one line.
- Mark a deliberate simplification that cuts a real corner with a `ponytail:` comment naming the ceiling and the upgrade path.
- When you delegate, the ladder applies to the assignment: name the smallest change and the existing code or standard-library call it reuses, and send back a diff that builds more than the assignment asked for.
- After the work, say in at most three short lines what you skipped and when to add it. Explanation the user asked for is not debt: give it in full.

Never lazy about: understanding the problem, input validation at trust boundaries, error handling that prevents data loss, security, accessibility, the calibration real hardware needs, and anything explicitly requested. Non-trivial logic leaves one runnable check behind, the smallest thing that fails when the logic breaks (an assert-based self-check or one small test file); a trivial one-liner needs none."#;

/// A child does the work an assignment specifies. The assignment already carries the parent's
/// decision about what to build, so the ladder only shapes how the child does it.
const SUBAGENT: &str = r#"Inside the assignment, take the shortest path that works: reuse what the codebase already has, then the standard library, then an installed dependency. Add no abstraction, configuration, file or dependency the assignment does not name. When the assignment asks for more than it needs, do what it says and note the simpler option in your report. Never skip input validation at trust boundaries, error handling that prevents data loss, security, or anything the assignment states. Leave one small runnable check behind for non-trivial logic, not a suite unless the assignment asks for one."#;

/// The `<ponytail>` body for a prompt audience.
pub fn instructions(audience: PromptAudience) -> &'static str {
    match audience {
        PromptAudience::Primary => PRIMARY,
        PromptAudience::Subagent => SUBAGENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Laziness must never cost correctness: both variants keep the guards, or a cheap worker
    /// would drop validation and error handling to keep the diff small.
    #[test]
    fn both_audiences_keep_the_safety_guards() {
        for audience in [PromptAudience::Primary, PromptAudience::Subagent] {
            let text = instructions(audience);
            assert!(
                text.contains("input validation at trust boundaries"),
                "{audience:?}"
            );
            assert!(
                text.contains("error handling that prevents data loss"),
                "{audience:?}"
            );
            assert!(text.contains("security"), "{audience:?}");
        }
    }

    /// A child's assignment already decided what to build: it must not be told to question whether
    /// the assignment should exist, or a worker would skip parts of a spec on its own judgment.
    #[test]
    fn a_child_shortens_how_it_works_not_what_the_assignment_asks() {
        let child = instructions(PromptAudience::Subagent);
        assert!(!child.contains("Does this need to exist"));
        assert!(child.contains("do what it says"));
        assert!(instructions(PromptAudience::Primary).contains("Does this need to exist at all?"));
    }

    /// An explicit user choice beats the ladder, matching the harness rule that explicit
    /// requirements stay in view until done.
    #[test]
    fn an_explicit_request_beats_the_ladder() {
        let text = instructions(PromptAudience::Primary);
        assert!(text.contains("build exactly that without re-arguing"));
        assert!(text.contains("anything explicitly requested"));
    }

    /// The main model no longer writes most code, so the ladder must reach the assignments it
    /// writes; a vague "probably use X" spec leaves the cheap worker to guess.
    #[test]
    fn the_ladder_shapes_the_assignments_the_main_model_writes() {
        let text = instructions(PromptAudience::Primary);
        assert!(text.contains("When you delegate, the ladder applies to the assignment"));
        assert!(text.contains(
            "name the smallest change and the existing code or standard-library call it reuses"
        ));
    }
}
