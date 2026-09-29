<system-reminder>
<goal-state>
Objective: {objective}
Status: Active
Tokens: {tokens} | Elapsed: {elapsed}
</goal-state>

{bail_preface}{plan_pointer}{verifier_gaps}{strategist_note}{reverify_block}Goal NOT complete — continue working. Next step:
{next_step}

If you keep a {todo_tool} list, keep it current. Run relevant checks after meaningful changes; the harness records their outcomes for the verifier. Reuse proof tied to the tested version and environment unless a concrete change invalidates it. Resolve skill instructions from their actual sources; do not invent mandatory reports, videos or councils. Review only new changes and unresolved objections. Tests must drive the SHIPPED code on the real path — no hard-coded values, no starting past the thing under test, no re-implementing it. Use your scratch dir {scratch_dir} {scratch_status} only for captured output a command result cannot show, temp scripts, and throwaway artifacts, never shared `/tmp/...`. Use existing user, system, or project defaults for execution dependencies and environment state. NEVER set `HOME`, `CARGO_HOME`, `RUSTUP_HOME`, package-manager homes, virtualenvs, caches, or config dirs to scratch, or persist references to scratch, which is deleted when the goal ends. The plan's `{SCRATCH}` placeholder resolves there. The verifier audits your committed tests, the recorded checks and your saved evidence rather than rebuilding them — leave honest proof or you WILL be refuted. Run the plan's `## Verification plan` steps yourself and confirm the observations it lists hold. The harness evaluates completion automatically after this round, re-checks the same steps adversarially when appropriate, and inlines any outstanding verifier gaps above.
</system-reminder>
