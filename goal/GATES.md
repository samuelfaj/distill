# Gates: Distill onboarding, model routing and subagent parity

Scope: Deliver the requested English onboarding; verify reasoning/worker routing and accurate model status for parent and child sessions without new dependencies.

Historical safety boundary: the Jev catalog from commit `2d905417` is
archived separately for reference only. Its obsolete permission decisions H1,
H2, and P5, plus the `jev·veto` badges, are removed from the current product
and must never be reactivated or counted as completion evidence.

- [ ] G1: Four-step onboarding works on first interactive use and /onboarding, with optional worker/profile skips and existing tutorial preserved.
  EVIDENCE: `controller-pager-focused-retry.log` records 132 focused pager tests passed, including onboarding/auth/status/tutorial cases; the real PTY onboarding run and child preflight used old binary SHA `2cde5fe...` and must rerun against the current product binary before this gate can close.
- [ ] G2: Controlled reasoning-worker-reasoning calls dispatch the expected models with compatibility, context and fallback guards intact.
  EVIDENCE: `controller-routing-idle-tests.log` records 13 routing/idle tests passed, including wire-captured routes, explicit child model/effort, retry attribution, and zero/single-effort guards; E2 C1/C5 source corrections still require fresh compile/review and rerun.
- [ ] G3: Status shows the actual final dispatched model and effort, including tools, fallback and cancellation, without cross-session contamination.
  EVIDENCE: the focused pager log includes the turn-status regression and the routing log includes wire attribution checks; final current-binary integrated model/status proof remains pending with E2 C1/C5 verification.
- [ ] G4: Child creation, resume and workflow paths apply the same applicable Jev, token-saving and model-routing policy as the parent while retaining child permission restrictions and explicit model overrides.
  EVIDENCE: `controller-subagent-resolution-final-retry.log` records 94 resolution tests passed, and `controller-routing-idle-tests.log` records explicit child override/effort cases; fresh C1/C5 field coverage and integrated child lifecycle proof remain pending. Do not count mock-only proof.
- [ ] G5: Subagent list and child detail status identify the actual active model; auxiliary calls do not overwrite foreground attribution.
  EVIDENCE: the focused pager log includes the configured-versus-active tasks-pane regression; current-binary cross-boundary status and auxiliary-call proof remain pending.
- [ ] G6: Focused regression tests and interactive terminal checks at normal and narrow widths pass; no dependency is added.
  EVIDENCE: controller logs record pager 132, routing 13, tools 9, workspace 139, subagent-resolution 94, and shell/permissions/auth 121 passed tests; the product binary and onboarding PTY/child-preflight evidence are old-SHA only and must rerun, while no dependency addition was reported.
- [ ] G7: Branding and contract migration uses the verified Distill README URL, canonical Distill env/schema names, and only tested compatibility aliases for legacy env/serialized values.
  EVIDENCE: `tools-tests.log` records 9 schema/tool tests passed, `controller-shell-permissions-auth.log` records 121 env/auth/permission tests passed, and `controller-branding-final-audit.txt` records the remaining legacy aliases/tests as intentional; final independent branding/source correlation remains pending.
- [ ] G8: Release preparation is verified without final publication: origin, version, existing tags/releases, workflow behavior, and candidate file set are recorded; each approved checkpoint must be committed and pushed, while final tag/release/install remain gated on final functional verification, independent review, and explicit controller publication approval.
  EVIDENCE: current HEAD `241b24668b27cb2b9d5cd5fc81660c8f1bcea83c` and remote SHA are verified; prior checkpoints were pushed; no tag v2, release, or installation exists. Final functional verification, five required E2 corrections, independent review, candidate manifest, and controller publication approval remain pending.
