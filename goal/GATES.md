# Distill onboarding, routing and subagent parity gates

Current reviewed snapshot: HEAD `c3d02c1646eaacbb71c5660041bba58583e0c15e`,
verified on the remote. Source clearance is PASS in
`combined-implementation-clearance.md` (9b) and
`terminal-delta-clearance.md` (c3). Current product binary:
`distill 2.0.0 (c3d02c1646ea) [alpha]`, SHA-256
`f73c33e30533f3202d1eecc52da7c6d2e88a61f218da3b63e820b8b49a9ad94b`.

Historical safety boundary: the Jev catalog from commit `2d905417` is archived
for reference only. H1, H2 and P5 permission decisions and all `jev·veto`
badges are retired and must never be reactivated or counted as evidence.

## Functional gates

- [x] **G1 — Onboarding:** Four-step first-use and `/onboarding` flow, tutorial
  separation, persistence, restart/reopen/back/close, skips, narrow/short
  recovery, model selection and browser-error finish path are evidenced by
  `controller-onboarding-current-pty.log` (2 PASS, 0 FAIL, 36.04 s), its
  current artifacts, and the focused/footer pager suites (132 and 431 PASS).
  The actual frames include normal, narrow, 8x80 resize guard, Step 3 `Esc`,
  `onboarding_completed=true` with `test-model`, restart/reopen/back, and the
  exact X URL with failure feedback and `Finish`.
- [x] **G1 supplementary — Positive worker overlay:** The current positive-worker fixture
  compiled in 1.13 s and passed 2/2 PTY cases in
  `controller-positive-worker-pty.log` (0 failures, 36.19 s). Its normal
  artifacts show `default-model` selected and saved as worker, visible in the
  footer after completion, and preserved after restart. This is mock-endpoint
  application proof, not live OAuth.
- [x] **G2 — Routing:** Controlled final sampler routing, including the exact
  reasoning/high → worker/low → reasoning/high sequence captured by
  `controlled_routes_are_captured_on_the_wire`, explicit child model/effort,
  zero/single-effort guards, retries, fallback and wire attribution pass in
  `controller-routing-idle-tests.log` (13 PASS) and the isolated shell suite
  (405 PASS, 0 FAIL, 23.12 s). C1–C5 are cleared by the final source review;
  no source correction remains.
- [x] **G3 — Status attribution:** Current status PTY cases pass in
  `controller-status-current-pty.log` (2 PASS, 0 FAIL, 11.56 s), the model
  switch PTY passes in `controller-model-current-pty.log` (1 PASS, 0 FAIL,
  5.54 s), and the 405-test ledger/model-switch suite covers final model,
  effort, retry, fallback, cancellation and session isolation. The PTYs prove
  the visible surface; the source/tests prove the broader state transitions.
- [x] **G4 — Child parity:** `controller-child-current-pty.log` passes the
  real child-navigation case (1 PASS, 0 FAIL, 5.35 s). The 94 resolution
  tests and 405 isolated shell tests cover normal and specialized creation,
  fork, resume, workflow, nested policy invariants, explicit overrides,
  permissions, wake and cancellation. Child wire tests inspect actual sampler
  requests and creation/persistence/wake behavior; navigation PTY alone is not
  the proof for every policy or concurrency case.
- [x] **G5 — Child/status surfaces:** Current status and child PTYs prove the
  visible configured/active attribution and navigation surfaces. Footer tests,
  source clearance and the 405/94 suites provide the separate evidence for
  foreground attribution, child isolation, auxiliary-call safety and policy
  coverage.
- [x] **G6 — Regression and environment:** The baseline current-binary PTY
  coverage is six PASS / 0 FAIL cases across onboarding (2), status (2), model
  (1) and child navigation (1); the positive-worker overlay adds 2 PASS / 0
  FAIL in 36.19 s after its 1.13 s fixture compile. Supporting controller
  results are shell 405, resolution 94, footer pager 431, workspace 139,
  tools/schema 9 and installer 1 PASS. The shell run used isolated
  `GROK_HOME`, 16 MiB stack and one thread. No new dependency was added; the
  existing Ratatui line-info feature is unchanged and `Cargo.lock` is
  unchanged.
- [x] **G7 — Branding/contracts:** The final branding audit classifies the
  nine remaining legacy aliases/tests and deliberate external references.
  Canonical Distill namespaces, producers/consumers, compatibility aliases
  and schema tests pass in tools 9 and shell 405. The verified README target is
  `https://github.com/samfaj/distill/blob/main/README.md`.

All PTY/harness provider interactions use isolated mock endpoints and the real
application/coordinator/tool runtime. They are not live OAuth proof. The live
provider attempts recorded in `live-yolo-path.txt` and `live-yolo-qwen-path.txt`
ended in upstream HTTP 429 before tool calls; this is an external limitation,
not a native permission denial. No real credentials were changed and no
external follow was performed. Local HTML inspection remained blocked by the
browser URL policy; no workaround or color-screenshot claim is made.

## G8 — Release preparation and publication

- [x] Candidate identity recorded: `2.0.0 (c3d02c1646ea) [alpha]`, current
  binary hash recorded above, and HEAD/remote SHA verified.
- [x] Final user-facing English release copy prepared at
  `/tmp/distill-todo-orchestration/release-notes-v2.md`.
- [x] Unknown `test.txt` preserved and excluded; credentials, temporary
  harness state and unowned artifacts are excluded from publication.
- [x] Controller-recorded checkpoint history reaches the reviewed HEAD
  `c3d02c1646eaacbb71c5660041bba58583e0c15e`, and that SHA is verified on the
  remote.
- [ ] Controller must independently verify the release workflow, tag inventory
  and `gh` state; this docs handback performs no release action and is not
  evidence that those release-preparation checks are closed.
- [ ] Controller publication gate approved.
- [ ] Final commit/tag remote artifact checks completed.
- [ ] GitHub Release created and independently read back.
- [ ] Published installers installed and verified against the published
  artifact, not merely the local binary.

The final distribution gate is intentionally open. Do not call the release,
tag, install, remote artifact, real OAuth, or full provider verification
complete until the controller independently performs those checks.
