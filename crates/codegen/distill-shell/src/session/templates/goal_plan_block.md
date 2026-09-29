A structured plan for this goal is on disk — the source of truth for "done". Read it first and keep it open.

Plan: {PLAN_PATH}

- When you deviate from the plan, append ONE terse bullet (what changed and why) to its single `## Deviations` section. Add to that one section, don't start a new one, and don't edit the plan's other items; it is not a progress log, so leave out test counts, "all fixed" and re-verification notes.
- Before claiming completion, run the plan's `## Verification plan` yourself and confirm its observations hold. Commit real tests that drive the shipped code. The harness records the checks you run for the verifier, so save captured output to your scratch dir (the one the goal rules name; never shared `/tmp/...`) only where a command result cannot show an observation. Fix any missing observation before calling the goal complete.
