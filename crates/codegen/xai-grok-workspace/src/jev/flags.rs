//! Feature gating for every Jev lever (plan §1.6/I-1: **default OFF**).
//!
//! `enabled` is the master switch: with it off, [`super::JevRuntime::from_flags`]
//! returns `None` and no client — therefore no connection — is ever built. The
//! per-lever switches are independent so a lever whose evaluation gate fails can
//! stay off while the others ship (criterion 5 of the implementation goal).

/// Whether the Jev decision path can act in this process: the path is enabled,
/// the caller is not in shadow, and a credential is resolvable. The TUI badge
/// reads this; it never carries the credential itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevStatus {
    pub enabled: bool,
    pub shadow: bool,
    pub credential_present: bool,
}

impl JevStatus {
    /// True when Jev can actually decide or block something right now.
    pub const fn active(&self) -> bool {
        self.enabled && self.credential_present
    }

    /// Short label for the prompt footer: `jev`, `jev·shadow`, or `jev:off`.
    pub const fn label(&self) -> &'static str {
        if !self.active() {
            "jev:off"
        } else if self.shadow {
            "jev·shadow"
        } else {
            "jev"
        }
    }
}

impl JevFlags {
    /// Projects the flags plus credential presence into the display status.
    pub const fn status(&self, credential_present: bool) -> JevStatus {
        JevStatus {
            enabled: self.enabled,
            shadow: self.shadow,
            credential_present,
        }
    }
}

/// Explicit per-lever switches coming from configuration. `None` means "not
/// configured", which resolves to the harness default (see
/// [`JevFlags::harness_default`]) rather than to off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JevLadderOverlay {
    pub permission_classifier: Option<bool>,
    /// E10 — run the deterministic crushers on a large tool result.
    pub e_crushers: Option<bool>,
    /// E2 — extract by importance (errors, file:line, paths, last lines) with an elided middle.
    pub e_importance: Option<bool>,
    /// E3 — compress a large tool result with the cheap model, storing the original first.
    pub e_cheap_compress: Option<bool>,
    /// E5 — run a registered cheap task (classify/extract/digest) on a tool result and use its answer.
    pub e_cheap_task: Option<bool>,
    /// E4 — serve a repeated read as a pointer to the bytes already sent.
    pub e_read_reuse: Option<bool>,
    /// E7 — let the decision layer choose main-vs-cheap, the form and the effort for a micro-action.
    pub e_lane_choice: Option<bool>,
    /// E6 — hand a non-critical micro-task to a cheap-model subagent.
    pub e_cheap_agent: Option<bool>,
    /// E9 — send only the standing-prompt blocks this turn needs.
    pub e_prompt_blocks: Option<bool>,
    /// E8 — stop calling a lane for the rest of the turn after repeated failures.
    pub e_breaker: Option<bool>,
    pub p1_tool_family: Option<bool>,
    pub p2_read_shortlist: Option<bool>,
    pub p3_compaction_recorte: Option<bool>,
    pub p5_call_validation: Option<bool>,
    pub p6_skill_suggestion: Option<bool>,
    /// YOLO / always-approve brake.
    pub yolo_veto: Option<bool>,
    /// A1: rank candidate files before reading them.
    pub a1_file_to_edit: Option<bool>,
    /// A3: keep only the log/test lines that explain a failure.
    pub a3_log_lines: Option<bool>,
    /// A4: rank search results before fetching them.
    pub a4_web_results: Option<bool>,
    /// A5: rank memory candidates before injecting them.
    pub a5_memory_rank: Option<bool>,
    /// A6: pick which test to run for a change.
    pub a6_test_to_run: Option<bool>,
    /// B1: classify the turn's intent and complexity.
    pub b1_intent_routing: Option<bool>,
    /// B2: money lever (model tier); off until its own gate passes.
    pub b2_model_tier: Option<bool>,
    /// B2 (auto): the per-model-call effort the user opts into with `/effort auto`.
    pub b2_micro_effort: Option<bool>,
    /// B2 (local): prefer the configured local model for calls it can fully do.
    pub b2_local_model: Option<bool>,
    /// B3: pick an existing agent definition for the task.
    pub b3_subagent_type: Option<bool>,
    /// B6: hint that delegating is worth it (never spawns).
    pub b6_delegation_hint: Option<bool>,
    /// C1: detect that requested work is still unfinished.
    pub c1_premature_stop: Option<bool>,
    /// C2: classify a failure and whether the fix is in user code.
    pub c2_failure_triage: Option<bool>,
    /// C3: refuse to call work complete while something is missing.
    pub c3_completion_check: Option<bool>,
    /// C4: flag a risky diff for confirmation (advisory).
    pub c4_diff_risk: Option<bool>,
    /// C5: order errors by importance.
    pub c5_error_priority: Option<bool>,
    /// C6: screen tool output for instruction-like text; off until its cost is measured.
    pub c6_injection_screen: Option<bool>,
    /// C7: label the change type for release notes.
    pub c7_change_type: Option<bool>,
    /// D2: drop a large inert tool output from the context.
    pub d2_big_output_retention: Option<bool>,
    /// D3: re-inject only still-relevant chunks after compaction.
    pub d3_post_compaction: Option<bool>,
}

/// Resolves one boolean switch: an explicit value from any trusted layer wins
/// (first config, then the environment); with neither, the harness default.
pub const fn resolve_switch(config: Option<bool>, env: Option<bool>, default: bool) -> bool {
    match (config, env) {
        (Some(value), _) => value,
        (None, Some(value)) => value,
        (None, None) => default,
    }
}

impl JevFlags {
    /// The harness policy default: every lever **on**, not in shadow.
    ///
    /// This is the owner's explicit override of the plan's invariant I-1 (which
    /// required default OFF). The kill switch stays intact: `[jev] enabled = false`
    /// or `GROK_JEV=0` disables everything, and [`JevFlags::default`] remains the
    /// inert all-off value used by tests and by "no policy configured" call sites.
    pub const fn harness_default() -> Self {
        Self {
            enabled: true,
            shadow: false,
            permission_classifier: true,
            p1_tool_family: true,
            p2_read_shortlist: true,
            p3_compaction_recorte: true,
            p5_call_validation: true,
            p6_skill_suggestion: true,
            yolo_veto: true,
            a1_file_to_edit: true,
            a3_log_lines: true,
            a4_web_results: true,
            a5_memory_rank: true,
            a6_test_to_run: true,
            b1_intent_routing: true,
            b2_model_tier: false,
            b2_micro_effort: true,
            b2_local_model: true,
            b3_subagent_type: true,
            b6_delegation_hint: true,
            c1_premature_stop: true,
            c2_failure_triage: true,
            c3_completion_check: true,
            c4_diff_risk: true,
            c5_error_priority: true,
            c6_injection_screen: false,
            c7_change_type: true,
            e_crushers: true,
            e_importance: true,
            e_cheap_compress: false,
            e_cheap_task: false,
            e_read_reuse: true,
            e_lane_choice: false,
            e_cheap_agent: false,
            e_prompt_blocks: false,
            e_breaker: true,
            d2_big_output_retention: true,
            d3_post_compaction: true,
        }
    }

    /// Overlays explicit switches on top of a base (normally
    /// [`JevFlags::harness_default`]). `config_enabled` / `env_enabled` are the
    /// two trusted tiers for the master switch; every lever is ANDed with it, so
    /// disabling the master switch disables each lever regardless of its own key.
    pub const fn overlaid(
        mut self,
        config_enabled: Option<bool>,
        env_enabled: Option<bool>,
        shadow: Option<bool>,
        ladder: JevLadderOverlay,
    ) -> Self {
        self.enabled = resolve_switch(config_enabled, env_enabled, self.enabled);
        self.shadow = resolve_switch(shadow, None, self.shadow);
        self.permission_classifier = self.enabled
            && resolve_switch(
                ladder.permission_classifier,
                None,
                self.permission_classifier,
            );
        self.p1_tool_family =
            self.enabled && resolve_switch(ladder.p1_tool_family, None, self.p1_tool_family);
        self.p2_read_shortlist =
            self.enabled && resolve_switch(ladder.p2_read_shortlist, None, self.p2_read_shortlist);
        self.p3_compaction_recorte = self.enabled
            && resolve_switch(
                ladder.p3_compaction_recorte,
                None,
                self.p3_compaction_recorte,
            );
        self.p5_call_validation = self.enabled
            && resolve_switch(ladder.p5_call_validation, None, self.p5_call_validation);
        self.p6_skill_suggestion = self.enabled
            && resolve_switch(ladder.p6_skill_suggestion, None, self.p6_skill_suggestion);
        self.yolo_veto = self.enabled && resolve_switch(ladder.yolo_veto, None, self.yolo_veto);
        self.e_crushers = self.enabled && resolve_switch(ladder.e_crushers, None, self.e_crushers);
        self.e_importance = self.enabled && resolve_switch(ladder.e_importance, None, self.e_importance);
        self.e_cheap_compress = self.enabled && resolve_switch(ladder.e_cheap_compress, None, self.e_cheap_compress);
        self.e_cheap_task = self.enabled && resolve_switch(ladder.e_cheap_task, None, self.e_cheap_task);
        self.e_read_reuse = self.enabled && resolve_switch(ladder.e_read_reuse, None, self.e_read_reuse);
        self.e_lane_choice = self.enabled && resolve_switch(ladder.e_lane_choice, None, self.e_lane_choice);
        self.e_cheap_agent = self.enabled && resolve_switch(ladder.e_cheap_agent, None, self.e_cheap_agent);
        self.e_prompt_blocks = self.enabled && resolve_switch(ladder.e_prompt_blocks, None, self.e_prompt_blocks);
        self.e_breaker = self.enabled && resolve_switch(ladder.e_breaker, None, self.e_breaker);
        self.a1_file_to_edit =
            self.enabled && resolve_switch(ladder.a1_file_to_edit, None, self.a1_file_to_edit);
        self.a3_log_lines =
            self.enabled && resolve_switch(ladder.a3_log_lines, None, self.a3_log_lines);
        self.a4_web_results =
            self.enabled && resolve_switch(ladder.a4_web_results, None, self.a4_web_results);
        self.a5_memory_rank =
            self.enabled && resolve_switch(ladder.a5_memory_rank, None, self.a5_memory_rank);
        self.a6_test_to_run =
            self.enabled && resolve_switch(ladder.a6_test_to_run, None, self.a6_test_to_run);
        self.b1_intent_routing =
            self.enabled && resolve_switch(ladder.b1_intent_routing, None, self.b1_intent_routing);
        self.b2_model_tier =
            self.enabled && resolve_switch(ladder.b2_model_tier, None, self.b2_model_tier);
        self.b2_micro_effort =
            self.enabled && resolve_switch(ladder.b2_micro_effort, None, self.b2_micro_effort);
        self.b2_local_model =
            self.enabled && resolve_switch(ladder.b2_local_model, None, self.b2_local_model);
        self.b3_subagent_type =
            self.enabled && resolve_switch(ladder.b3_subagent_type, None, self.b3_subagent_type);
        self.b6_delegation_hint = self.enabled
            && resolve_switch(ladder.b6_delegation_hint, None, self.b6_delegation_hint);
        self.c1_premature_stop =
            self.enabled && resolve_switch(ladder.c1_premature_stop, None, self.c1_premature_stop);
        self.c2_failure_triage =
            self.enabled && resolve_switch(ladder.c2_failure_triage, None, self.c2_failure_triage);
        self.c3_completion_check = self.enabled
            && resolve_switch(ladder.c3_completion_check, None, self.c3_completion_check);
        self.c4_diff_risk =
            self.enabled && resolve_switch(ladder.c4_diff_risk, None, self.c4_diff_risk);
        self.c5_error_priority =
            self.enabled && resolve_switch(ladder.c5_error_priority, None, self.c5_error_priority);
        self.c6_injection_screen = self.enabled
            && resolve_switch(ladder.c6_injection_screen, None, self.c6_injection_screen);
        self.c7_change_type =
            self.enabled && resolve_switch(ladder.c7_change_type, None, self.c7_change_type);
        self.d2_big_output_retention = self.enabled
            && resolve_switch(
                ladder.d2_big_output_retention,
                None,
                self.d2_big_output_retention,
            );
        self.d3_post_compaction = self.enabled
            && resolve_switch(ladder.d3_post_compaction, None, self.d3_post_compaction);
        self
    }
}

/// Every Jev lever, all disabled by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JevFlags {
    /// Master switch. Off ⇒ no client is constructed anywhere.
    pub enabled: bool,
    /// Shadow phase for the permission classifier: compute and record, but never
    /// change the decision (plan §4 S-005, phase 1).
    pub shadow: bool,
    /// Install the Jev-backed permission classifier ahead of the LLM one.
    pub permission_classifier: bool,
    /// E10 — run the deterministic crushers on a large tool result.
    pub e_crushers: bool,
    /// E2 — extract by importance (errors, file:line, paths, last lines) with an elided middle.
    pub e_importance: bool,
    /// E3 — compress a large tool result with the cheap model, storing the original first.
    pub e_cheap_compress: bool,
    /// E5 — run a registered cheap task (classify/extract/digest) on a tool result and use its answer.
    pub e_cheap_task: bool,
    /// E4 — serve a repeated read as a pointer to the bytes already sent.
    pub e_read_reuse: bool,
    /// E7 — let the decision layer choose main-vs-cheap, the form and the effort for a micro-action.
    pub e_lane_choice: bool,
    /// E6 — hand a non-critical micro-task to a cheap-model subagent.
    pub e_cheap_agent: bool,
    /// E9 — send only the standing-prompt blocks this turn needs.
    pub e_prompt_blocks: bool,
    /// E8 — stop calling a lane for the rest of the turn after repeated failures.
    pub e_breaker: bool,
    /// P1 — prune the per-turn tool set by family.
    pub p1_tool_family: bool,
    /// P2 — pick line/segment windows instead of whole files.
    pub p2_read_shortlist: bool,
    /// P3 — decide which segments the compactor must see.
    pub p3_compaction_recorte: bool,
    /// P5 — validate a tool call (block/ask only) before executing it.
    pub p5_call_validation: bool,
    /// P6 — pick which announced skill matters for this turn.
    pub p6_skill_suggestion: bool,
    /// YOLO / always-approve brake: consult Jev before auto-approving so a
    /// confident catastrophe is refused instead of run. Never prompts.
    pub yolo_veto: bool,
    /// A1: rank candidate files before reading them.
    pub a1_file_to_edit: bool,
    /// A3: keep only the log/test lines that explain a failure.
    pub a3_log_lines: bool,
    /// A4: rank search results before fetching them.
    pub a4_web_results: bool,
    /// A5: rank memory candidates before injecting them.
    pub a5_memory_rank: bool,
    /// A6: pick which test to run for a change.
    pub a6_test_to_run: bool,
    /// B1: classify the turn's intent and complexity.
    pub b1_intent_routing: bool,
    /// B2: money lever (model tier); off until its own gate passes.
    pub b2_model_tier: bool,
    /// B2 (auto): pick the effort for every model call (only runs in auto mode).
    pub b2_micro_effort: bool,
    /// B2 (local): prefer the configured local model when it can fully do the call.
    pub b2_local_model: bool,
    /// B3: pick an existing agent definition for the task.
    pub b3_subagent_type: bool,
    /// B6: hint that delegating is worth it (never spawns).
    pub b6_delegation_hint: bool,
    /// C1: detect that requested work is still unfinished.
    pub c1_premature_stop: bool,
    /// C2: classify a failure and whether the fix is in user code.
    pub c2_failure_triage: bool,
    /// C3: refuse to call work complete while something is missing.
    pub c3_completion_check: bool,
    /// C4: flag a risky diff for confirmation (advisory).
    pub c4_diff_risk: bool,
    /// C5: order errors by importance.
    pub c5_error_priority: bool,
    /// C6: screen tool output for instruction-like text; off until its cost is measured.
    pub c6_injection_screen: bool,
    /// C7: label the change type for release notes.
    pub c7_change_type: bool,
    /// D2: drop a large inert tool output from the context.
    pub d2_big_output_retention: bool,
    /// D3: re-inject only still-relevant chunks after compaction.
    pub d3_post_compaction: bool,
}

impl JevFlags {
    /// Explicit all-off, for call sites that want to be obvious about it.
    pub const fn off() -> Self {
        Self {
            enabled: false,
            shadow: false,
            permission_classifier: false,
            p1_tool_family: false,
            p2_read_shortlist: false,
            p3_compaction_recorte: false,
            p5_call_validation: false,
            p6_skill_suggestion: false,
            yolo_veto: false,
            a1_file_to_edit: false,
            a3_log_lines: false,
            a4_web_results: false,
            a5_memory_rank: false,
            a6_test_to_run: false,
            b1_intent_routing: false,
            b2_model_tier: false,
            b2_micro_effort: false,
            b2_local_model: false,
            b3_subagent_type: false,
            b6_delegation_hint: false,
            c1_premature_stop: false,
            c2_failure_triage: false,
            c3_completion_check: false,
            c4_diff_risk: false,
            c5_error_priority: false,
            c6_injection_screen: false,
            c7_change_type: false,
            d2_big_output_retention: false,
            e_crushers: false,
            e_importance: false,
            e_cheap_compress: false,
            e_cheap_task: false,
            e_read_reuse: false,
            e_lane_choice: false,
            e_cheap_agent: false,
            e_prompt_blocks: false,
            e_breaker: false,
            d3_post_compaction: false,
        }
    }

    /// Master switch ON, every lever ON, not in shadow — the harness policy
    /// default (`[jev]` unset, `GROK_JEV` unset).
    pub const fn all_levers_for_tests() -> Self {
        Self::harness_default()
    }

    /// True when nothing is enabled: the "provably inert" state.
    pub const fn is_off(&self) -> bool {
        !self.enabled
            && !self.shadow
            && !self.permission_classifier
            && !self.p1_tool_family
            && !self.p2_read_shortlist
            && !self.p3_compaction_recorte
            && !self.p5_call_validation
            && !self.p6_skill_suggestion
            && !self.yolo_veto
    }

    /// True when any lever is enabled (used to skip building a client at all).
    pub const fn any(&self) -> bool {
        self.enabled
            || self.shadow
            || self.permission_classifier
            || self.p1_tool_family
            || self.p2_read_shortlist
            || self.p3_compaction_recorte
            || self.p5_call_validation
            || self.p6_skill_suggestion
            || self.yolo_veto
    }

    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub const fn with_shadow(mut self, shadow: bool) -> Self {
        self.shadow = shadow;
        self
    }

    pub const fn with_permission_classifier(mut self, on: bool) -> Self {
        self.permission_classifier = on;
        self
    }

    pub const fn with_p1_tool_family(mut self, on: bool) -> Self {
        self.p1_tool_family = on;
        self
    }

    pub const fn with_p2_read_shortlist(mut self, on: bool) -> Self {
        self.p2_read_shortlist = on;
        self
    }

    pub const fn with_p3_compaction_recorte(mut self, on: bool) -> Self {
        self.p3_compaction_recorte = on;
        self
    }

    pub const fn with_p5_call_validation(mut self, on: bool) -> Self {
        self.p5_call_validation = on;
        self
    }

    pub const fn with_p6_skill_suggestion(mut self, on: bool) -> Self {
        self.p6_skill_suggestion = on;
        self
    }

    /// Whether a given lever may run at all (master switch AND its own switch).
    pub const fn lever_active(&self, lever: JevLever) -> bool {
        if !self.enabled {
            return false;
        }
        match lever {
            JevLever::PermissionClassifier => self.permission_classifier,
            JevLever::ECrushers => self.e_crushers,
            JevLever::EImportance => self.e_importance,
            JevLever::ECheapCompress => self.e_cheap_compress,
            JevLever::ECheapTask => self.e_cheap_task,
            JevLever::EReadReuse => self.e_read_reuse,
            JevLever::ELaneChoice => self.e_lane_choice,
            JevLever::ECheapAgent => self.e_cheap_agent,
            JevLever::EPromptBlocks => self.e_prompt_blocks,
            JevLever::EBreaker => self.e_breaker,
            JevLever::P1ToolFamily => self.p1_tool_family,
            JevLever::P2ReadShortlist => self.p2_read_shortlist,
            JevLever::P3CompactionRecorte => self.p3_compaction_recorte,
            JevLever::P5CallValidation => self.p5_call_validation,
            JevLever::P6SkillSuggestion => self.p6_skill_suggestion,
            JevLever::YoloVeto => self.yolo_veto,
            JevLever::A1FileToEdit => self.a1_file_to_edit,
            JevLever::A3LogLines => self.a3_log_lines,
            JevLever::A4WebResults => self.a4_web_results,
            JevLever::A5MemoryRank => self.a5_memory_rank,
            JevLever::A6TestToRun => self.a6_test_to_run,
            JevLever::B1IntentRouting => self.b1_intent_routing,
            JevLever::B2ModelTier => self.b2_model_tier,
            JevLever::B2MicroEffort => self.b2_micro_effort,
            JevLever::B2LocalModel => self.b2_local_model,
            JevLever::B3SubagentType => self.b3_subagent_type,
            JevLever::B6DelegationHint => self.b6_delegation_hint,
            JevLever::C1PrematureStop => self.c1_premature_stop,
            JevLever::C2FailureTriage => self.c2_failure_triage,
            JevLever::C3CompletionCheck => self.c3_completion_check,
            JevLever::C4DiffRisk => self.c4_diff_risk,
            JevLever::C5ErrorPriority => self.c5_error_priority,
            JevLever::C6InjectionScreen => self.c6_injection_screen,
            JevLever::C7ChangeType => self.c7_change_type,
            JevLever::D2BigOutputRetention => self.d2_big_output_retention,
            JevLever::D3PostCompaction => self.d3_post_compaction,
        }
    }
}

/// Addressable levers, so tests and telemetry can name one without a bool soup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevLever {
    PermissionClassifier,
    ECrushers,
    EImportance,
    ECheapCompress,
    ECheapTask,
    EReadReuse,
    ELaneChoice,
    ECheapAgent,
    EPromptBlocks,
    EBreaker,
    P1ToolFamily,
    P2ReadShortlist,
    P3CompactionRecorte,
    P5CallValidation,
    P6SkillSuggestion,
    YoloVeto,
    A1FileToEdit,
    A3LogLines,
    A4WebResults,
    A5MemoryRank,
    A6TestToRun,
    B1IntentRouting,
    B2ModelTier,
    B2MicroEffort,
    B2LocalModel,
    B3SubagentType,
    B6DelegationHint,
    C1PrematureStop,
    C2FailureTriage,
    C3CompletionCheck,
    C4DiffRisk,
    C5ErrorPriority,
    C6InjectionScreen,
    C7ChangeType,
    D2BigOutputRetention,
    D3PostCompaction,
}

impl JevLever {
    /// Telemetry/log label for this lever.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PermissionClassifier => "permission_classifier",
            Self::ECrushers => "e_crushers",
            Self::EImportance => "e_importance",
            Self::ECheapCompress => "e_cheap_compress",
            Self::ECheapTask => "e_cheap_task",
            Self::EReadReuse => "e_read_reuse",
            Self::ELaneChoice => "e_lane_choice",
            Self::ECheapAgent => "e_cheap_agent",
            Self::EPromptBlocks => "e_prompt_blocks",
            Self::EBreaker => "e_breaker",
            Self::P1ToolFamily => "p1_tool_family",
            Self::P2ReadShortlist => "p2_read_shortlist",
            Self::P3CompactionRecorte => "p3_compaction_recorte",
            Self::P5CallValidation => "p5_call_validation",
            Self::P6SkillSuggestion => "p6_skill_suggestion",
            Self::YoloVeto => "yolo_veto",
            Self::A1FileToEdit => "a1_file_to_edit",
            Self::A3LogLines => "a3_log_lines",
            Self::A4WebResults => "a4_web_results",
            Self::A5MemoryRank => "a5_memory_rank",
            Self::A6TestToRun => "a6_test_to_run",
            Self::B1IntentRouting => "b1_intent_routing",
            Self::B2ModelTier => "b2_model_tier",
            Self::B2MicroEffort => "b2_micro_effort",
            Self::B2LocalModel => "b2_local_model",
            Self::B3SubagentType => "b3_subagent_type",
            Self::B6DelegationHint => "b6_delegation_hint",
            Self::C1PrematureStop => "c1_premature_stop",
            Self::C2FailureTriage => "c2_failure_triage",
            Self::C3CompletionCheck => "c3_completion_check",
            Self::C4DiffRisk => "c4_diff_risk",
            Self::C5ErrorPriority => "c5_error_priority",
            Self::C6InjectionScreen => "c6_injection_screen",
            Self::C7ChangeType => "c7_change_type",
            Self::D2BigOutputRetention => "d2_big_output_retention",
            Self::D3PostCompaction => "d3_post_compaction",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_provably_off() {
        let flags = JevFlags::default();
        assert!(flags.is_off());
        assert!(!flags.any());
        for lever in [
            JevLever::PermissionClassifier,
            JevLever::P1ToolFamily,
            JevLever::P2ReadShortlist,
            JevLever::P3CompactionRecorte,
            JevLever::P5CallValidation,
            JevLever::P6SkillSuggestion,
            JevLever::YoloVeto,
            JevLever::A1FileToEdit,
            JevLever::A3LogLines,
            JevLever::A4WebResults,
            JevLever::A5MemoryRank,
            JevLever::A6TestToRun,
            JevLever::B1IntentRouting,
            JevLever::B2ModelTier,
            JevLever::B3SubagentType,
            JevLever::B6DelegationHint,
            JevLever::C1PrematureStop,
            JevLever::C2FailureTriage,
            JevLever::C3CompletionCheck,
            JevLever::C4DiffRisk,
            JevLever::C5ErrorPriority,
            JevLever::C6InjectionScreen,
            JevLever::C7ChangeType,
            JevLever::D2BigOutputRetention,
            JevLever::D3PostCompaction,
        ] {
            assert!(!flags.lever_active(lever), "{} must be off", lever.as_str());
        }
    }

    #[test]
    fn a_lever_requires_the_master_switch() {
        let mut flags = JevFlags::default();
        flags.p1_tool_family = true;
        assert!(!flags.lever_active(JevLever::P1ToolFamily));
        let flags = flags.with_enabled(true);
        assert!(flags.lever_active(JevLever::P1ToolFamily));
        assert!(!flags.lever_active(JevLever::P2ReadShortlist));
        assert!(flags.any());
        assert!(!flags.is_off());
    }

    #[test]
    fn the_harness_default_turns_every_lever_on() {
        let flags = JevFlags::harness_default();
        assert!(flags.enabled, "master switch is on by policy");
        assert!(!flags.shadow, "active, not shadow, by policy");
        for lever in [
            JevLever::PermissionClassifier,
            JevLever::P1ToolFamily,
            JevLever::P2ReadShortlist,
            JevLever::P3CompactionRecorte,
            JevLever::P5CallValidation,
            JevLever::P6SkillSuggestion,
            JevLever::YoloVeto,
            JevLever::A1FileToEdit,
            JevLever::A3LogLines,
            JevLever::A4WebResults,
            JevLever::A5MemoryRank,
            JevLever::A6TestToRun,
            JevLever::B1IntentRouting,
            JevLever::B3SubagentType,
            JevLever::B6DelegationHint,
            JevLever::C1PrematureStop,
            JevLever::C2FailureTriage,
            JevLever::C3CompletionCheck,
            JevLever::C4DiffRisk,
            JevLever::C5ErrorPriority,
            JevLever::C7ChangeType,
            JevLever::D2BigOutputRetention,
            JevLever::D3PostCompaction,
        ] {
            assert!(flags.lever_active(lever), "{} must be on", lever.as_str());
        }
        assert!(!flags.is_off());
    }

    #[test]
    fn unset_switches_keep_the_default_and_explicit_ones_win() {
        let flags =
            JevFlags::harness_default().overlaid(None, None, None, JevLadderOverlay::default());
        assert!(flags.enabled && flags.permission_classifier);

        let off = JevFlags::harness_default().overlaid(
            Some(false),
            None,
            None,
            JevLadderOverlay::default(),
        );
        assert!(!off.enabled);
        assert!(
            !off.permission_classifier && !off.p1_tool_family && !off.p6_skill_suggestion,
            "the master switch gates every lever"
        );
    }

    #[test]
    fn the_environment_tier_can_disable_and_enable() {
        let via_env_off = JevFlags::harness_default().overlaid(
            None,
            Some(false),
            None,
            JevLadderOverlay::default(),
        );
        assert!(!via_env_off.enabled);
        let via_env_on =
            JevFlags::default().overlaid(None, Some(true), None, JevLadderOverlay::default());
        assert!(via_env_on.enabled);
        // Config beats environment when both speak.
        let both = JevFlags::harness_default().overlaid(
            Some(false),
            Some(true),
            None,
            JevLadderOverlay::default(),
        );
        assert!(!both.enabled, "an explicit config value is authoritative");
    }

    #[test]
    fn a_single_lever_can_be_turned_off_without_touching_the_rest() {
        let flags = JevFlags::harness_default().overlaid(
            None,
            None,
            Some(true),
            JevLadderOverlay {
                permission_classifier: Some(false),
                ..JevLadderOverlay::default()
            },
        );
        assert!(flags.enabled);
        assert!(flags.shadow, "shadow can be turned on explicitly");
        assert!(!flags.permission_classifier);
        assert!(flags.p1_tool_family, "other levers keep the default");
        assert!(!flags.lever_active(JevLever::PermissionClassifier));
    }

    #[test]
    fn the_yolo_brake_is_independent_and_on_by_default() {
        assert!(JevFlags::harness_default().yolo_veto);
        assert!(JevFlags::harness_default().lever_active(JevLever::YoloVeto));
        let off = JevFlags::harness_default().overlaid(
            None,
            None,
            None,
            JevLadderOverlay {
                yolo_veto: Some(false),
                ..JevLadderOverlay::default()
            },
        );
        assert!(off.enabled, "disabling the brake keeps the path on");
        assert!(!off.yolo_veto, "the brake is switchable on its own key");
        assert!(off.permission_classifier, "other levers are untouched");
    }

    #[test]
    fn the_status_label_says_what_the_tui_badge_shows() {
        let enabled = JevFlags::harness_default();
        assert_eq!(enabled.status(true).label(), "jev");
        assert!(enabled.status(true).active());
        // No credential ⇒ the seam cannot act, and the badge says so.
        assert_eq!(enabled.status(false).label(), "jev:off");
        assert!(!enabled.status(false).active());
        // Shadow ⇒ observation only, and the badge distinguishes it.
        let shadow = enabled.with_shadow(true);
        assert_eq!(shadow.status(true).label(), "jev·shadow");
        assert!(shadow.status(true).active());
        // Disabled ⇒ off no matter the credential.
        let off = JevFlags::default();
        assert_eq!(off.status(true).label(), "jev:off");
        assert!(!off.status(true).active());
    }

    #[test]
    fn items_with_a_pending_gate_are_off_by_default() {
        let flags = JevFlags::harness_default();
        // Both levers are switchable and documented as pending their own gate.
        assert!(!flags.b2_model_tier, "B2 (money lever) waits for its gate");
        assert!(
            !flags.c6_injection_screen,
            "C6 (per-output cost) waits for its gate"
        );
        let on = flags.overlaid(
            None,
            None,
            None,
            JevLadderOverlay {
                b2_model_tier: Some(true),
                c6_injection_screen: Some(true),
                ..JevLadderOverlay::default()
            },
        );
        assert!(
            on.b2_model_tier && on.c6_injection_screen,
            "both are switchable"
        );
    }
}
