// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for credit-limit upsells, paywall gating, and auto-topup.

use super::*;
use distill_shell::sampling::error::is_free_usage_exhausted_error;

/// Open the non-max-tier Q&A upsell modal. Panics if the modal was not created.
fn open_upsell_qa(app: &mut AppView, mode: CreditLimitUpsellMode) {
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    open_credit_limit_upsell(agent, mode, false);
}

/// Open the max-tier inline scrollback card upsell.
fn open_upsell_max_card(app: &mut AppView, mode: CreditLimitUpsellMode) {
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    open_credit_limit_upsell(agent, mode, true);
}

/// Return the `QuestionViewState` from agent 0. Panics if absent.
fn agent_qv(app: &AppView) -> &crate::views::question_view::QuestionViewState {
    app.agents
        .get(&AgentId(0))
        .unwrap()
        .question_view
        .as_ref()
        .unwrap()
}

fn first_question(
    qv: &crate::views::question_view::QuestionViewState,
) -> &crate::views::question_view::Question {
    match qv.questions.first() {
        Some(q) => q,
        None => panic!("expected a question"),
    }
}

fn option_at(
    q: &crate::views::question_view::Question,
    i: usize,
) -> &crate::views::question_view::QuestionOption {
    match q.options.get(i) {
        Some(o) => o,
        None => panic!("expected option {i}"),
    }
}

fn set_first_selection(
    qv: &mut crate::views::question_view::QuestionViewState,
    sel: crate::views::question_view::QuestionSelection,
) {
    match qv.selections.first_mut() {
        Some(slot) => *slot = sel,
        None => panic!("expected a selection slot"),
    }
}

fn test_stashed_prompt(text: &str) -> crate::app::agent::InFlightPrompt {
    crate::app::agent::InFlightPrompt {
        text: text.into(),
        images: Vec::new(),
        scrollback_entry: crate::scrollback::EntryId::new(0),
        combined_scrollback_entries: Vec::new(),
        chip_elements: Vec::new(),
    }
}

/// Dispatch a `BillingFetched` task result with sensible defaults.
fn open_usage_modal_nonce(app: &AppView) -> u64 {
    match test_agent(app, AgentId(0)).active_modal.as_ref() {
        Some(crate::views::modal::ActiveModal::UsageInfo { state }) => state.fetch_nonce,
        _ => 0,
    }
}

fn dispatch_billing(
    app: &mut AppView,
    balance: Option<crate::views::credit_bar::CreditBalance>,
    silent: bool,
    subscription_tier: Option<String>,
) {
    let nonce = open_usage_modal_nonce(app);
    dispatch(
        Action::TaskComplete(TaskResult::BillingFetched {
            agent_id: AgentId(0),
            balance,
            silent,
            subscription_tier,
            autotopup: crate::views::credit_bar::AutoTopupFetch::Unchanged,
            nonce,
        }),
        app,
    );
}

#[test]
fn credit_limit_retry_preserves_image_submission_state() {
    let mut app = test_app_with_agent();
    let mut image = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![1, 2, 3],
        mime_type: "image/png".into(),
    });
    image.display_number = 1;
    let prompt = crate::app::agent::InFlightPrompt {
        text: "retry [Image #1]".into(),
        images: vec![image],
        scrollback_entry: crate::scrollback::EntryId::new(0),
        combined_scrollback_entries: Vec::new(),
        chip_elements: vec![crate::app::agent::ChipElement {
            range: 6..16,
            kind: crate::views::prompt_widget::KIND_IMAGE,
            display: None,
        }],
    };
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .credit_limit_stashed_prompt = Some(prompt);

    let effects = dispatch(
        Action::TaskComplete(TaskResult::CreditLimitRecheckComplete {
            agent_id: AgentId(0),
            meta: Some(serde_json::json!({"subscription_tier": "Upgraded"})),
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::SendPromptBlocks { .. }))
    );
    let in_flight = test_agent(&app, AgentId(0))
        .session
        .in_flight_prompt
        .as_ref()
        .unwrap();
    assert_eq!(in_flight.images.len(), 1);
    assert_eq!(in_flight.chip_elements.len(), 1);
}

#[test]
fn credit_limit_recheck_keeps_stash_when_showing_upsell() {
    let mut app = test_app_with_agent();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .credit_limit_stashed_prompt = Some(test_stashed_prompt("retry me"));

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CreditLimitRecheckComplete {
            agent_id: AgentId(0),
            meta: None,
        }),
        &mut app,
    );
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.question_view.is_some());
    assert_eq!(
        agent
            .credit_limit_stashed_prompt
            .as_ref()
            .map(|p| p.text.as_str()),
        Some("retry me")
    );
}

#[test]
fn credit_limit_recheck_drops_stash_when_user_moved_on() {
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.credit_limit_stashed_prompt = Some(test_stashed_prompt("retry me"));
        agent.session.enqueue_prompt("new prompt".into());
    }

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CreditLimitRecheckComplete {
            agent_id: AgentId(0),
            meta: None,
        }),
        &mut app,
    );
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.credit_limit_stashed_prompt.is_none());
    assert!(
        agent.question_view.is_none(),
        "moved-on must not open the upsell"
    );
}

#[test]
fn sending_a_new_prompt_clears_credit_limit_stash() {
    let mut app = test_app_with_agent();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .credit_limit_stashed_prompt = Some(test_stashed_prompt("retry me"));

    let _ = dispatch_send_prompt(&mut app, "something new".into());
    assert!(
        app.agents
            .get(&AgentId(0))
            .unwrap()
            .credit_limit_stashed_prompt
            .is_none()
    );
}

#[test]
fn send_prompt_now_clears_credit_limit_stash() {
    let mut app = test_app_with_agent();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .credit_limit_stashed_prompt = Some(test_stashed_prompt("retry me"));

    let _ = dispatch(
        Action::SendPromptNow {
            text: "steer it".into(),
            images: Vec::new(),
        },
        &mut app,
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .unwrap()
            .credit_limit_stashed_prompt
            .is_none()
    );
}

#[test]
fn retry_credit_limit_prompt_resubmits_stash() {
    let mut app = test_app_with_agent();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .credit_limit_stashed_prompt = Some(test_stashed_prompt("retry me"));

    let effects = dispatch(Action::RetryCreditLimitPrompt, &mut app);
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::SendPrompt { text, .. } if text == "retry me"
        )),
        "expected SendPrompt, got {effects:?}"
    );
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.credit_limit_stashed_prompt.is_none());
    assert_eq!(
        agent
            .session
            .in_flight_prompt
            .as_ref()
            .map(|p| p.text.as_str()),
        Some("retry me")
    );
    let has_retry_line = (0..agent.scrollback.len()).any(|i| {
        matches!(
            &agent.scrollback.entry(i).unwrap().block,
            crate::scrollback::block::RenderBlock::System(sys)
                if sys.text.contains("Trying again")
        )
    });
    assert!(has_retry_line, "expected retry system line");
}

#[test]
fn retry_credit_limit_prompt_toasts_when_stash_empty() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::RetryCreditLimitPrompt, &mut app);
    assert!(effects.is_empty(), "expected no send, got {effects:?}");
    let toast = test_agent(&app, AgentId(0))
        .toast
        .as_ref()
        .map(|(m, _)| m.as_str());
    assert_eq!(toast, Some("No prompt to retry."));
    let has_system = (0..test_agent(&app, AgentId(0)).scrollback.len()).any(|i| {
        matches!(
            &test_agent(&app, AgentId(0)).scrollback.entry(i).unwrap().block,
            crate::scrollback::block::RenderBlock::System(sys)
                if sys.text.contains("No prompt to retry")
        )
    });
    assert!(has_system, "minimal mode has no toast; need a system line");
}

#[test]
fn credit_limit_translate_retry_option_dispatches_retry() {
    use crate::app::agent_view::translate_local_submit_for_test;
    use crate::app::app_view::InputOutcome;
    use crate::views::question_view::QuestionSelection;

    let mut app = test_app_with_agent();
    open_upsell_qa(&mut app, CreditLimitUpsellMode::UnifiedCredits);
    let mut qv = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .question_view
        .take()
        .expect("expected credit-limit upsell modal");
    set_first_selection(&mut qv, QuestionSelection::Single(Some(2)));
    let kind = qv
        .local_kind
        .take()
        .expect("open_credit_limit_upsell sets local_kind");
    match translate_local_submit_for_test(&qv, kind, false) {
        InputOutcome::Action(Action::RetryCreditLimitPrompt) => {}
        other => panic!("expected RetryCreditLimitPrompt, got {other:?}"),
    }
}

#[test]
fn credit_limit_translate_max_tier_retry_is_second_option() {
    use crate::app::agent_view::translate_local_submit_for_test;
    use crate::app::app_view::InputOutcome;
    use crate::views::question_view::QuestionSelection;

    let mut app = test_app_with_agent();
    open_upsell_max_card(&mut app, CreditLimitUpsellMode::UnifiedCredits);
    let mut qv = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .question_view
        .take()
        .expect("expected credit-limit upsell modal");
    set_first_selection(&mut qv, QuestionSelection::Single(Some(1)));
    let kind = qv
        .local_kind
        .take()
        .expect("open_credit_limit_upsell sets local_kind");
    match translate_local_submit_for_test(&qv, kind, false) {
        InputOutcome::Action(Action::RetryCreditLimitPrompt) => {}
        other => panic!("expected RetryCreditLimitPrompt, got {other:?}"),
    }
}

#[test]
fn is_max_tier_positive_match() {
    assert!(is_max_tier(Some("supergrok_heavy")));
    assert!(is_max_tier(Some("SuperGrok Heavy")));
    assert!(is_max_tier(Some("SUPERGROK_HEAVY")));
}

#[test]
fn is_max_tier_non_max_and_unknown() {
    assert!(!is_max_tier(Some("supergrok")));
    assert!(!is_max_tier(Some("premium")));
    assert!(!is_max_tier(Some("free")));
    // Unknown defaults to non-max, so the Q&A is shown
    assert!(!is_max_tier(None));
}

#[test]
fn is_max_tier_handles_mixed_case_and_whitespace() {
    assert!(is_max_tier(Some("SuperGrok_Heavy")));
    assert!(is_max_tier(Some("supergrok heavy")));
    assert!(is_max_tier(Some("SUPERGROK HEAVY")));
}

#[test]
fn is_max_tier_rejects_partial_matches() {
    assert!(!is_max_tier(Some("supergrok_heav")));
    assert!(!is_max_tier(Some("supergrok_heavy_plus")));
    assert!(!is_max_tier(Some("")));
}

#[test]
fn upsell_non_max_shows_qa_with_retry_option() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    let q = first_question(agent_qv(&app));
    assert_eq!(q.options.len(), 3);
    assert_eq!(option_at(q, 0).label, "Upgrade tier");
    assert_eq!(option_at(q, 0).id.as_deref(), Some(UPSELL_URL_UPGRADE));
    assert_eq!(option_at(q, 1).label, "Pay as you go");
    assert_eq!(option_at(q, 1).id.as_deref(), Some(UPSELL_URL_PAYG));
    assert_eq!(option_at(q, 2).label, "Try Again");
    assert_eq!(
        option_at(q, 2).id.as_deref(),
        Some(crate::app::dispatch::CREDIT_LIMIT_RETRY_OPTION_ID)
    );
}

#[test]
fn upsell_non_max_payg_on_shows_increase_label() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: true },
    );
    let q = first_question(agent_qv(&app));
    assert_eq!(q.options.len(), 3);
    assert_eq!(option_at(q, 1).label, "Increase limit");
    assert_eq!(option_at(q, 2).label, "Try Again");
}

#[test]
fn upsell_non_max_qa_heading_is_credit_limit_when_payg_off() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    let heading = &first_question(agent_qv(&app)).question;
    assert!(
        heading.contains("credit limit"),
        "expected 'credit limit' in heading, got: {heading}"
    );
}

#[test]
fn upsell_non_max_qa_heading_is_spending_cap_when_payg_on() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: true },
    );
    let heading = &first_question(agent_qv(&app)).question;
    assert!(
        heading.contains("spending cap"),
        "expected 'spending cap' in heading, got: {heading}"
    );
}

#[test]
fn upsell_non_max_upgrade_url_is_supergrok() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    let url = option_at(first_question(agent_qv(&app)), 0)
        .id
        .as_deref()
        .unwrap();
    assert!(url.contains("supergrok"), "got: {url}");
    assert!(url.contains("referrer=Distill"), "got: {url}");
}

#[test]
fn upsell_non_max_payg_url_is_usage() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    let url = option_at(first_question(agent_qv(&app)), 1)
        .id
        .as_deref()
        .unwrap();
    assert!(url.contains("_s=usage"), "got: {url}");
}

#[test]
fn upsell_non_max_payg_on_description_mentions_spending_cap() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: true },
    );
    assert_eq!(
        option_at(first_question(agent_qv(&app)), 1).description,
        "Raise your pay-as-you-go spending cap"
    );
}

#[test]
fn upsell_non_max_payg_off_description_mentions_on_demand() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert_eq!(
        option_at(first_question(agent_qv(&app)), 1).description,
        "Enable pay-as-you-go credits for on-demand usage"
    );
}

#[test]
fn upsell_non_max_unified_shows_buy_credits() {
    let mut app = test_app_with_agent();
    open_upsell_qa(&mut app, CreditLimitUpsellMode::UnifiedCredits);
    let q = first_question(agent_qv(&app));
    assert!(q.question.contains("weekly limit"));
    assert_eq!(
        option_at(q, 0).description,
        "Upgrade to a higher tier for more usage"
    );
    assert_eq!(option_at(q, 1).label, "Buy more credits");
    assert_eq!(
        option_at(q, 1).description,
        "Purchase credits to keep using Distill"
    );
    assert_eq!(option_at(q, 2).label, "Try Again");
}

#[test]
fn upsell_max_unified_qa_omits_upgrade() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    open_upsell_max_card(&mut app, CreditLimitUpsellMode::UnifiedCredits);
    assert_eq!(
        agent_scrollback_len(&app),
        before,
        "max-tier upsell must not push a scrollback card"
    );
    let q = first_question(agent_qv(&app));
    assert!(q.question.contains("weekly limit"));
    assert_eq!(q.options.len(), 2);
    assert_eq!(option_at(q, 0).label, "Buy more credits");
    assert_eq!(option_at(q, 0).id.as_deref(), Some(UPSELL_URL_PAYG));
    assert_eq!(option_at(q, 1).label, "Try Again");
    assert_eq!(
        option_at(q, 1).id.as_deref(),
        Some(crate::app::dispatch::CREDIT_LIMIT_RETRY_OPTION_ID)
    );
}

#[test]
fn credit_limit_upsell_mode_prefers_unified_flag() {
    let mut bal = test_bal(100.0);
    bal.is_unified_billing_user = Some(true);
    bal.pay_as_you_go = true; // must not override the explicit unified flag
    assert_eq!(
        credit_limit_upsell_mode(Some(&bal)),
        CreditLimitUpsellMode::UnifiedCredits
    );
    bal.is_unified_billing_user = Some(false);
    bal.pay_as_you_go = false;
    assert_eq!(
        credit_limit_upsell_mode(Some(&bal)),
        CreditLimitUpsellMode::LegacyPayg { enabled: false }
    );
    bal.is_unified_billing_user = None;
    bal.pay_as_you_go = true;
    assert_eq!(
        credit_limit_upsell_mode(Some(&bal)),
        CreditLimitUpsellMode::LegacyPayg { enabled: true }
    );
    bal.pay_as_you_go = false;
    assert_eq!(
        credit_limit_upsell_mode(Some(&bal)),
        CreditLimitUpsellMode::UnifiedCredits
    );
    assert_eq!(
        credit_limit_upsell_mode(None),
        CreditLimitUpsellMode::UnifiedCredits
    );
}

#[test]
fn is_credit_limit_error_matches_legacy_403_and_pool_402() {
    assert!(is_credit_limit_error(
        Some(403),
        "status 403: run out of credits"
    ));
    // A 402 Payment Required always counts as a credit/spend limit, whatever the message says
    assert!(is_credit_limit_error(Some(402), "anything"));
    assert!(is_credit_limit_error(
        None,
        "API error (status 402 Payment Required): Distill usage balance exhausted"
    ));
    assert!(is_credit_limit_error(
        None,
        "status 403: run out of credits"
    ));
    assert!(!is_credit_limit_error(Some(403), "content safety blocked"));
    assert!(!is_credit_limit_error(Some(500), "internal server error"));
    // Pool phrases alone without 402/403 status do not match.
    assert!(!is_credit_limit_error(
        None,
        "usage balance exhausted without status"
    ));
}

#[test]
fn upsell_non_max_sets_no_freeform() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert!(
        agent_qv(&app).no_freeform,
        "upsell Q&A should disable freeform input"
    );
}

#[test]
fn upsell_non_max_qa_has_single_select() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert_eq!(
        first_question(agent_qv(&app)).multi_select,
        Some(false),
        "upsell should be single-select"
    );
}

#[test]
fn upsell_non_max_does_not_push_scrollback_block() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert_eq!(
        agent_scrollback_len(&app),
        before,
        "non-max-tier upsell should NOT push a scrollback block"
    );
}

#[test]
fn upsell_non_max_idempotent_when_question_view_already_open() {
    let mut app = test_app_with_agent();
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert!(app.agents.get(&AgentId(0)).unwrap().question_view.is_some());

    // Second call should be a no-op (open_credit_limit_upsell returns if a modal is already open).
    let before = agent_scrollback_len(&app);
    open_upsell_qa(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert_eq!(
        agent_scrollback_len(&app),
        before,
        "second call should not push a block"
    );
}

#[test]
fn upsell_max_tier_opens_qa_without_upgrade_payg_off() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    open_upsell_max_card(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert!(app.agents.get(&AgentId(0)).unwrap().question_view.is_some());
    assert_eq!(agent_scrollback_len(&app), before);
    let q = first_question(agent_qv(&app));
    assert!(q.question.contains("credit limit"));
    assert_eq!(q.options.len(), 2);
    assert_eq!(option_at(q, 0).label, "Pay as you go");
    assert_eq!(option_at(q, 0).id.as_deref(), Some(UPSELL_URL_PAYG));
    assert_eq!(option_at(q, 1).label, "Try Again");
}

#[test]
fn upsell_max_tier_opens_qa_without_upgrade_payg_on() {
    let mut app = test_app_with_agent();
    open_upsell_max_card(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: true },
    );
    let q = first_question(agent_qv(&app));
    assert!(q.question.contains("spending cap"));
    assert_eq!(q.options.len(), 2);
    assert_eq!(option_at(q, 0).label, "Increase limit");
    assert_eq!(option_at(q, 1).label, "Try Again");
}

#[test]
fn upsell_max_tier_opens_question_view() {
    let mut app = test_app_with_agent();
    open_upsell_max_card(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert!(
        app.agents.get(&AgentId(0)).unwrap().question_view.is_some(),
        "max-tier should use the question modal"
    );
}

#[test]
fn upsell_max_tier_buy_url_is_payg() {
    let mut app = test_app_with_agent();
    open_upsell_max_card(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert_eq!(
        option_at(first_question(agent_qv(&app)), 0).id.as_deref(),
        Some(UPSELL_URL_PAYG)
    );
}

#[test]
fn upsell_max_tier_idempotent_when_question_view_already_open() {
    let mut app = test_app_with_agent();
    open_upsell_max_card(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    let first_id = agent_qv(&app).tool_call_id.clone();
    open_upsell_max_card(
        &mut app,
        CreditLimitUpsellMode::LegacyPayg { enabled: false },
    );
    assert_eq!(
        agent_qv(&app).tool_call_id,
        first_id,
        "second call must not replace the open modal"
    );
}

fn is_session_usage_fetch(effects: &[Effect]) -> bool {
    matches!(
        effects,
        [Effect::FetchSessionUsage { agent_id, .. }] if *agent_id == AgentId(0)
    )
}

fn is_nonsilent_billing(effects: &[Effect]) -> bool {
    matches!(
        effects,
        [Effect::FetchBilling { agent_id, silent, .. }] if *agent_id == AgentId(0) && !*silent
    )
}

fn complete_session_usage(
    app: &mut AppView,
    session_id: &str,
    usage: distill_shell::extensions::notification::PromptUsage,
) -> Vec<Effect> {
    dispatch(
        Action::TaskComplete(TaskResult::SessionUsageComplete {
            agent_id: AgentId(0),
            session_id: session_id.to_string().into(),
            usage: Box::new(usage),
            nonce: Default::default(),
        }),
        app,
    )
}

fn fail_session_usage(app: &mut AppView, session_id: &str, error: &str) -> Vec<Effect> {
    dispatch(
        Action::TaskComplete(TaskResult::SessionUsageFailed {
            agent_id: AgentId(0),
            session_id: session_id.to_string().into(),
            error: error.into(),
            nonce: Default::default(),
        }),
        app,
    )
}

#[test]
fn show_usage_schedules_session_fetch_only() {
    let mut app = test_app_with_agent();
    // The scrollback usage flow only runs in Minimal screen mode
    app.screen_mode = crate::app::ScreenMode::Minimal;
    assert!(is_session_usage_fetch(&dispatch(
        Action::ShowUsage,
        &mut app
    )));

    app.usage_visible = false;
    assert!(is_session_usage_fetch(&dispatch(
        Action::ShowUsage,
        &mut app
    )));
}

#[test]
fn show_usage_without_session_still_surfaces_credits() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.agents.get_mut(&AgentId(0)).unwrap().session.session_id = None;
    let before = agent_scrollback_len(&app);
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert!(last_system_text(&app, AgentId(0)).contains("unavailable"));
    assert_eq!(agent_scrollback_len(&app), before + 1);
    assert!(is_nonsilent_billing(&effects));
}

#[test]
fn team_auth_disables_agent_billing_surface() {
    let mut app = test_app_with_agent();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .billing_surface_visible = true;
    app.apply_auth_meta(&distill_login::AuthMeta {
        team_id: Some("team-uuid".into()),
        team_name: Some("Acme Corp".into()),
        ..Default::default()
    });
    assert!(!app.usage_visible);
    assert!(!app.agents.get(&AgentId(0)).unwrap().billing_surface_visible);
}

#[serial_test::serial(GROK_TEST_OPEN_URL_FILE)]
#[test]
fn manage_billing_gates_on_consumer_billing_surface() {
    let out = std::env::temp_dir().join(format!("grok-manage-billing-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out);
    // SAFETY: serialized via `serial_test` so no other test races the env var.
    unsafe { std::env::set_var("GROK_TEST_OPEN_URL_FILE", &out) };
    let mut app = test_app_with_agent();
    dispatch(Action::ManageBilling, &mut app);
    let opened = std::fs::read_to_string(&out).unwrap_or_default();
    assert!(opened.contains("grok.com/?_s=usage"), "got: {opened}");
    let _ = std::fs::remove_file(&out);

    // Non-consumer: silent no-op (slash command never offers manage).
    let mut app = test_app_with_agent();
    app.usage_visible = false;
    let before = agent_scrollback_len(&app);
    assert!(dispatch(Action::ManageBilling, &mut app).is_empty());
    assert_eq!(agent_scrollback_len(&app), before);
}

#[test]
fn session_usage_complete_pushes_block_and_chains_billing() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let before = agent_scrollback_len(&app);
    let usage = distill_shell::extensions::notification::PromptUsage {
        totals: distill_shell::extensions::notification::PromptUsageModel {
            input_tokens: 1_000,
            output_tokens: 100,
            total_tokens: 1_100,
            model_calls: 3,
            cost_usd_ticks: Some(5_000_000_000),
            ..Default::default()
        },
        ..Default::default()
    };
    let effects = complete_session_usage(&mut app, "test-session", usage);
    assert_eq!(agent_scrollback_len(&app), before + 1);
    let text = last_system_text(&app, AgentId(0));
    assert!(
        text.contains("Session usage") && text.contains("$0.5000"),
        "{text}"
    );
    assert!(is_nonsilent_billing(&effects));
}

#[test]
fn session_usage_complete_no_billing_when_surface_hidden() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.usage_visible = false;
    let before = agent_scrollback_len(&app);
    let effects = complete_session_usage(&mut app, "test-session", Default::default());
    assert!(effects.is_empty());
    // Only the credit follow-up is gated; the session block itself must land.
    assert_eq!(agent_scrollback_len(&app), before + 1);
}

#[test]
fn session_usage_complete_redirect_after_session_block() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.usage_billing_redirect_url = Some("https://billing.example.com/me".into());
    // Dispatch defers the redirect until after the session block.
    let before = agent_scrollback_len(&app);
    assert!(is_session_usage_fetch(&dispatch(
        Action::ShowUsage,
        &mut app
    )));
    assert_eq!(agent_scrollback_len(&app), before);

    let effects = complete_session_usage(&mut app, "test-session", Default::default());
    assert!(effects.is_empty());
    assert_eq!(agent_scrollback_len(&app), before + 2);
    assert!(last_system_text(&app, AgentId(0)).contains("https://billing.example.com/me"));
}

#[test]
fn session_usage_complete_drops_stale_session() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    let effects = complete_session_usage(
        &mut app,
        "old-session",
        distill_shell::extensions::notification::PromptUsage {
            totals: distill_shell::extensions::notification::PromptUsageModel {
                model_calls: 99,
                cost_usd_ticks: Some(1_000_000_000_000),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert!(effects.is_empty());
    assert_eq!(agent_scrollback_len(&app), before);
}

#[test]
fn session_usage_failed_pushes_error_and_chains_billing() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let before = agent_scrollback_len(&app);
    let effects = fail_session_usage(&mut app, "test-session", "boom");
    assert_eq!(agent_scrollback_len(&app), before + 1);
    assert!(last_system_text(&app, AgentId(0)).contains("Couldn't load session usage: boom"));
    assert!(is_nonsilent_billing(&effects));
}

#[test]
fn session_usage_failed_drops_stale_session() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    assert!(fail_session_usage(&mut app, "old-session", "boom").is_empty());
    assert_eq!(agent_scrollback_len(&app), before);
}

#[test]
fn billing_fetched_updates_app_credit_balance() {
    let mut app = test_app_with_agent();
    dispatch_billing(&mut app, Some(test_bal(42.0)), true, None);
    assert!(app.credit_balance.is_some());
    assert_eq!(app.credit_balance.as_ref().unwrap().usage_pct, 42.0);
}

#[test]
fn billing_fetched_updates_subscription_tier() {
    let mut app = test_app_with_agent();
    dispatch_billing(&mut app, None, true, Some("supergrok_heavy".into()));
    assert_eq!(app.subscription_tier.as_deref(), Some("supergrok_heavy"));
}

#[test]
fn billing_fetched_silent_does_not_push_scrollback() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch_billing(&mut app, Some(test_bal(50.0)), true, None);
    assert_eq!(
        agent_scrollback_len(&app),
        before,
        "silent billing fetch should not push a scrollback message"
    );
}

#[test]
fn billing_fetched_non_silent_pushes_scrollback_message() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    let bal = crate::views::credit_bar::CreditBalance {
        pay_as_you_go: true,
        on_demand_cap_cents: Some(1000),
        on_demand_used_cents: Some(350),
        period_end_display: Some("Jul 1, 00:00".into()),
        ..test_bal(75.5)
    };
    dispatch_billing(&mut app, Some(bal), false, None);
    assert_eq!(
        agent_scrollback_len(&app),
        before + 1,
        "non-silent billing fetch should push a scrollback message"
    );
}

#[test]
fn billing_fetched_none_balance_shows_no_data_message() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch_billing(&mut app, None, false, None);
    assert_eq!(agent_scrollback_len(&app), before + 1);
}

#[test]
fn billing_fetched_none_balance_clears_cached() {
    let mut app = test_app_with_agent();
    // Seed a known balance and polling, as a prior successful fetch would
    dispatch_billing(&mut app, Some(test_bal(80.0)), true, None);
    app.billing_poll_wanted = true;
    // A response carrying no billing config clears the cached balance and polling so the status bar agrees with the "No billing data" message
    // Parse/transport failures route to BillingError, not here
    dispatch_billing(&mut app, None, false, None);
    assert!(
        app.credit_balance.is_none(),
        "None balance should clear the cached credit balance"
    );
    assert!(
        !app.billing_poll_wanted,
        "None balance should disable billing polling"
    );
}

#[test]
fn billing_fetched_high_usage_enables_poll() {
    let mut app = test_app_with_agent();
    assert!(!app.billing_poll_wanted);
    dispatch_billing(&mut app, Some(test_bal(99.5)), true, None);
    assert!(
        app.billing_poll_wanted,
        "usage >= 99% should enable billing polling"
    );
}

#[test]
fn billing_fetched_low_usage_disables_poll() {
    let mut app = test_app_with_agent();
    app.billing_poll_wanted = true;
    dispatch_billing(&mut app, Some(test_bal(50.0)), true, None);
    assert!(
        !app.billing_poll_wanted,
        "usage < 99% should disable billing polling"
    );
}

#[test]
fn billing_fetched_propagates_balance_to_agent() {
    let mut app = test_app_with_agent();
    let bal = crate::views::credit_bar::CreditBalance {
        effective_usage_pct: 60.0,
        pay_as_you_go: true,
        on_demand_cap_cents: Some(5000),
        on_demand_used_cents: Some(1200),
        period_end_display: Some("Aug 15, 00:00".into()),
        ..test_bal(88.0)
    };
    dispatch_billing(&mut app, Some(bal), true, None);
    let agent_bal = app
        .agents
        .get(&AgentId(0))
        .unwrap()
        .credit_balance
        .as_ref()
        .unwrap();
    assert_eq!(agent_bal.usage_pct, 88.0);
    assert_eq!(agent_bal.effective_usage_pct, 60.0);
    assert!(agent_bal.pay_as_you_go);
    assert_eq!(agent_bal.on_demand_cap_cents, Some(5000));
    assert_eq!(agent_bal.on_demand_used_cents, Some(1200));
}

#[test]
fn billing_fetched_stores_autotopup_on_app_and_agent() {
    let mut app = test_app_with_agent();
    let bal = crate::views::credit_bar::CreditBalance {
        prepaid_balance_cents: Some(1500),
        ..test_bal(100.0)
    };
    let autotopup = crate::views::credit_bar::AutoTopupInfo {
        enabled: true,
        topup_amount_cents: Some(2000),
        max_amount_cents: Some(10000),
    };
    dispatch(
        Action::TaskComplete(TaskResult::BillingFetched {
            agent_id: AgentId(0),
            balance: Some(bal),
            silent: true,
            subscription_tier: None,
            autotopup: crate::views::credit_bar::AutoTopupFetch::Resolved(autotopup),
            nonce: Default::default(),
        }),
        &mut app,
    );
    assert!(app.auto_topup.as_ref().is_some_and(|at| at.enabled));
    let agent_at = app.agents.get(&AgentId(0)).unwrap().auto_topup.as_ref();
    assert_eq!(agent_at.and_then(|at| at.max_amount_cents), Some(10000));
}

#[test]
fn billing_fetched_unchanged_autotopup_keeps_cached_rule() {
    let mut app = test_app_with_agent();
    let bal = || crate::views::credit_bar::CreditBalance {
        prepaid_balance_cents: Some(1500),
        ..test_bal(100.0)
    };
    let resolved = crate::views::credit_bar::AutoTopupFetch::Resolved(
        crate::views::credit_bar::AutoTopupInfo {
            enabled: true,
            topup_amount_cents: Some(2000),
            max_amount_cents: None,
        },
    );
    dispatch(
        Action::TaskComplete(TaskResult::BillingFetched {
            agent_id: AgentId(0),
            balance: Some(bal()),
            silent: true,
            subscription_tier: None,
            autotopup: resolved,
            nonce: Default::default(),
        }),
        &mut app,
    );
    // A later refresh whose auto-topup fetch failed must not clear the rule.
    dispatch(
        Action::TaskComplete(TaskResult::BillingFetched {
            agent_id: AgentId(0),
            balance: Some(bal()),
            silent: true,
            subscription_tier: None,
            autotopup: crate::views::credit_bar::AutoTopupFetch::Unchanged,
            nonce: Default::default(),
        }),
        &mut app,
    );
    assert!(app.auto_topup.as_ref().is_some_and(|at| at.enabled));
    let agent_at = app.agents.get(&AgentId(0)).unwrap().auto_topup.as_ref();
    assert!(agent_at.is_some_and(|at| at.enabled));
}

#[test]
fn billing_fetched_cleared_autotopup_resets_cache() {
    let mut app = test_app_with_agent();
    // Seed a known rule while credits exist.
    dispatch(
        Action::TaskComplete(TaskResult::BillingFetched {
            agent_id: AgentId(0),
            balance: Some(crate::views::credit_bar::CreditBalance {
                prepaid_balance_cents: Some(1500),
                ..test_bal(100.0)
            }),
            silent: true,
            subscription_tier: None,
            autotopup: crate::views::credit_bar::AutoTopupFetch::Resolved(
                crate::views::credit_bar::AutoTopupInfo {
                    enabled: true,
                    topup_amount_cents: Some(2000),
                    max_amount_cents: None,
                },
            ),
            nonce: Default::default(),
        }),
        &mut app,
    );
    // Credits gone: `Cleared` resets the cached rule to "unknown" so a later credits period can't read a stale rule
    dispatch(
        Action::TaskComplete(TaskResult::BillingFetched {
            agent_id: AgentId(0),
            balance: Some(test_bal(50.0)),
            silent: true,
            subscription_tier: None,
            autotopup: crate::views::credit_bar::AutoTopupFetch::Cleared,
            nonce: Default::default(),
        }),
        &mut app,
    );
    assert!(app.auto_topup.is_none());
    assert!(app.agents.get(&AgentId(0)).unwrap().auto_topup.is_none());
}

#[test]
fn app_billing_fetched_stores_autotopup() {
    let mut app = test_app_with_agent();
    let bal = crate::views::credit_bar::CreditBalance {
        prepaid_balance_cents: Some(500),
        ..test_bal(0.0)
    };
    dispatch(
        Action::TaskComplete(TaskResult::AppBillingFetched {
            balance: Some(bal),
            autotopup: crate::views::credit_bar::AutoTopupFetch::Resolved(
                crate::views::credit_bar::AutoTopupInfo::disabled(),
            ),
            nonce: 0,
        }),
        &mut app,
    );
    assert_eq!(
        app.credit_balance.and_then(|b| b.prepaid_balance_cents),
        Some(500)
    );
    assert!(app.auto_topup.is_some_and(|at| !at.enabled));
}

/// A failed app-level fetch is not a "no billing data" answer: the cached balance behind the welcome warning stays put.
#[test]
fn app_billing_error_keeps_cached_balance() {
    let mut app = test_app_with_agent();
    app.credit_balance = Some(test_bal(77.0));
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AppBillingError {
            error: "timeout".to_string(),
            nonce: 0,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(app.credit_balance.as_ref().map(|b| b.usage_pct), Some(77.0));
}

#[test]
fn billing_error_silent_does_not_push_scrollback() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch(
        Action::TaskComplete(TaskResult::BillingError {
            agent_id: AgentId(0),
            error: "network timeout".into(),
            silent: true,
            nonce: Default::default(),
        }),
        &mut app,
    );
    assert_eq!(
        agent_scrollback_len(&app),
        before,
        "silent billing error should not push a scrollback message"
    );
}

#[test]
fn billing_error_non_silent_pushes_error_message() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch(
        Action::TaskComplete(TaskResult::BillingError {
            agent_id: AgentId(0),
            error: "service unavailable".into(),
            silent: false,
            nonce: Default::default(),
        }),
        &mut app,
    );
    assert_eq!(
        agent_scrollback_len(&app),
        before + 1,
        "non-silent billing error should push an error message"
    );
}

#[test]
fn free_usage_error_detected_by_embedded_code() {
    // parse_error_bytes flattens the 429 body to "<code>: <message>".
    assert!(is_free_usage_exhausted_error(
        "API error (status 429 Too Many Requests): \
         subscription:free-usage-exhausted: You have used all your free usage."
    ));
    // Generic rate limits and other WKE codes must not match.
    assert!(!is_free_usage_exhausted_error(
        "API error (status 429 Too Many Requests): Rate limit exceeded"
    ));
    assert!(!is_free_usage_exhausted_error(
        "unauthorized:missing-acl: nope"
    ));
}

#[test]
fn free_usage_upsell_displaces_feedback_before_opening_question() {
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.feedback_modal = Some(crate::views::feedback_modal::FeedbackModalState::new(
        crate::views::feedback_modal::OpenFeedbackModal {
            text: Some("unsent report".to_owned()),
            ..Default::default()
        },
    ));

    open_free_usage_upsell(agent, None);

    assert!(agent.feedback_modal.is_none());
    assert!(agent.question_view.is_some());
}

#[test]
fn free_usage_upsell_shows_three_options_with_exact_labels() {
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    open_free_usage_upsell(agent, None);

    let qv = agent_qv(&app);
    assert!(matches!(
        qv.local_kind,
        Some(
            crate::views::question_view::LocalQuestionKind::FreeUsageUpsell {
                source: distill_telemetry::events::SuperGrokUpsell::FreeUsagePaywall,
            }
        )
    ));
    let q = first_question(qv);
    assert_eq!(q.question, "You hit your free usage limit.");
    let expected = [
        (
            "Upgrade to SuperGrok",
            "For everyday coding and productivity tasks",
            Some(UPSELL_URL_UPGRADE),
        ),
        (
            "Upgrade to SuperGrok Plus",
            "Significantly higher usage and rate limits",
            Some(UPSELL_URL_UPGRADE),
        ),
        (
            "Upgrade to SuperGrok Heavy",
            "Get the most out of Distill. Highest usage limits.",
            Some(UPSELL_URL_UPGRADE),
        ),
    ];
    assert_eq!(q.options.len(), expected.len());
    for (opt, (label, desc, id)) in q.options.iter().zip(expected) {
        assert_eq!(opt.label, label);
        assert_eq!(opt.description, desc);
        assert_eq!(opt.id.as_deref(), id);
    }
}

/// Replay the REAL free-usage sequence (send, `RetryState::Retrying`, `Exhausted`, PromptResponse error) through the production handlers.
/// The paywall modal must open on the turn-end error.
#[test]
fn free_usage_failure_opens_paywall_modal() {
    use crate::app::acp_handler::apply_session_event_for_test;
    use distill_shell::extensions::notification::{RetryState, SessionUpdate};

    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // 1. Real send.
    let effects = dispatch(Action::SendPrompt("draw me a cat".into()), &mut app);
    assert!(
        matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "draw me a cat"),
        "send must dispatch: {effects:?}"
    );
    let prompt_id = test_agent(&app, id).session.current_prompt_id.clone();
    assert!(prompt_id.is_some(), "send must mint a prompt id");

    // 2 and 3. Real notification sequence through the production handler.
    {
        let agent = app.agents.get_mut(&id).unwrap();
        apply_session_event_for_test(
            &SessionUpdate::RetryState(RetryState::Retrying {
                attempt: 1,
                max_retries: 2,
                reason: "429 Too Many Requests".into(),
                error_type: None,
            }),
            &mut agent.session,
            &mut agent.scrollback,
        );
        apply_session_event_for_test(
            &SessionUpdate::RetryState(RetryState::Exhausted {
                attempts: 2,
                reason: "API error (status 429 Too Many Requests): \
                         subscription:free-usage-exhausted: You have used all your free usage."
                    .into(),
                is_rate_limited: true,
            }),
            &mut agent.session,
            &mut agent.scrollback,
        );
        assert!(agent.session.free_usage_blocked);
    }

    // 4. Turn-end RPC error opens the upsell modal.
    let _ = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("rate limited".into()),
            http_status: Some(429),
            prompt_id,
        }),
        &mut app,
    );
    assert!(
        test_agent(&app, id).question_view.is_some(),
        "paywall modal must open"
    );
}

/// Answer translation: every upgrade option opens the upgrade URL.
#[test]
fn free_usage_translate_local_submit_maps_options() {
    use crate::app::agent_view::translate_local_submit_for_test;
    use crate::app::app_view::InputOutcome;
    use crate::views::question_view::{LocalQuestionKind, QuestionSelection};

    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    open_free_usage_upsell(agent, None);
    let mut qv = agent.question_view.take().unwrap();
    let kind = || LocalQuestionKind::FreeUsageUpsell {
        source: distill_telemetry::events::SuperGrokUpsell::FreeUsagePaywall,
    };

    for idx in [0, 1, 2] {
        set_first_selection(&mut qv, QuestionSelection::Single(Some(idx)));
        match translate_local_submit_for_test(&qv, kind(), false) {
            InputOutcome::Action(Action::OpenUrl(url)) => assert_eq!(url, UPSELL_URL_UPGRADE),
            other => panic!("expected OpenUrl for option {idx}, got {other:?}"),
        }
    }
}

/// Submitting a tier-restricted command opens the three-option SuperGrok upsell and neither runs the command nor leaks the text to the model.
#[test]
fn restricted_command_submit_opens_three_option_upsell() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .set_restricted_commands(&["imagine".to_string()]);

    let effects = dispatch(Action::SendPrompt("/imagine a sunset".into()), &mut app);

    assert!(
        effects.is_empty(),
        "restricted command must not produce a SendPrompt: {effects:?}"
    );
    let agent = test_agent(&app, id);
    assert!(
        agent.session.pending_prompts.is_empty(),
        "restricted command must not be enqueued"
    );
    assert!(agent.prompt.text().is_empty(), "composer consumed");

    let qv = agent_qv(&app);
    assert!(matches!(
        qv.local_kind,
        Some(
            crate::views::question_view::LocalQuestionKind::FreeUsageUpsell {
                source: distill_telemetry::events::SuperGrokUpsell::RestrictedCommand,
            }
        )
    ));
    let q = first_question(qv);
    assert_eq!(q.question, "Unlock all features with SuperGrok.");
    assert_eq!(q.options.len(), 3);
    assert_eq!(option_at(q, 0).label, "Upgrade to SuperGrok");
    assert_eq!(option_at(q, 0).id.as_deref(), Some(UPSELL_URL_UPGRADE));
    assert_eq!(option_at(q, 1).label, "Upgrade to SuperGrok Plus");
    assert_eq!(option_at(q, 1).id.as_deref(), Some(UPSELL_URL_UPGRADE));
    assert_eq!(option_at(q, 2).label, "Upgrade to SuperGrok Heavy");
    assert_eq!(option_at(q, 2).id.as_deref(), Some(UPSELL_URL_UPGRADE));
}

/// Aliases of a restricted command hit the same upsell (deny-list matching covers aliases via the registry).
#[test]
fn restricted_command_alias_also_upsells() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .set_restricted_commands(&["usage".to_string()]);

    let effects = dispatch(Action::SendPrompt("/cost".into()), &mut app);

    assert!(effects.is_empty());
    assert!(
        test_agent(&app, id).question_view.is_some(),
        "upsell must open"
    );
}

/// A restricted submit while ANOTHER question modal is already open must not silently drop the typed text.
/// The upsell can't open (the guard never displaces a modal), so the composer keeps the text for a resubmit after the modal closes.
/// No passthrough, nothing enqueued, and the existing modal survives untouched.
#[test]
fn restricted_command_with_open_modal_keeps_composer_text() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.set_restricted_commands(&["imagine".to_string()]);
        // A question modal is already up (credit-limit upsell).
        open_credit_limit_upsell(agent, CreditLimitUpsellMode::UnifiedCredits, false);
        assert!(agent.question_view.is_some());
        // The user typed the restricted command into the composer.
        agent.prompt.set_text("/imagine a sunset");
    }

    let effects = dispatch(Action::SendPrompt("/imagine a sunset".into()), &mut app);

    assert!(effects.is_empty(), "no passthrough / send: {effects:?}");
    let agent = test_agent(&app, id);
    assert_eq!(
        agent.prompt.text(),
        "/imagine a sunset",
        "composer text must be preserved for a later resubmit"
    );
    assert!(
        matches!(
            agent
                .question_view
                .as_ref()
                .and_then(|qv| qv.local_kind.as_ref()),
            Some(crate::views::question_view::LocalQuestionKind::CreditLimitUpsell { .. })
        ),
        "the pre-existing modal must survive (no second modal)"
    );
    assert!(
        agent.session.pending_prompts.is_empty(),
        "nothing may be enqueued"
    );
}

/// Regression: genuinely unknown (non-restricted) commands keep the PassThrough behavior shell/ACP commands rely on.
#[test]
fn unknown_non_restricted_command_still_passes_through() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .set_restricted_commands(&["imagine".to_string()]);

    let effects = dispatch(Action::SendPrompt("/frobnicate arg".into()), &mut app);

    assert_eq!(effects.len(), 1);
    assert!(
        matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "/frobnicate arg"),
        "unknown command must still pass through: {effects:?}"
    );
    assert!(
        test_agent(&app, id).question_view.is_none(),
        "no upsell for genuinely unknown commands"
    );
}

/// `Action::OpenUrl` for a billing CTA must push a scrollback system message that includes the full URL when the OS browser opener cannot run.
/// The opener failure is simulated via a broken `GROK_TEST_OPEN_URL_FILE` path.
/// On a headless VM the opener always fails, so without this message the Upgrade and Buy-more-credits buttons do nothing visible.
#[serial_test::serial(GROK_TEST_OPEN_URL_FILE)]
#[test]
fn open_url_shows_manual_url_when_browser_unavailable() {
    // Point `GROK_TEST_OPEN_URL_FILE` at a path whose parent dir does not exist so the write fails and `open_url` returns false (BrowserUnavailable)
    let bad = std::env::temp_dir().join(format!(
        "grok-open-url-missing-{}/out.txt",
        std::process::id()
    ));
    // SAFETY: serialized via `serial_test` so no other test races the env var.
    unsafe { std::env::set_var("GROK_TEST_OPEN_URL_FILE", &bad) };

    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    let url = UPSELL_URL_UPGRADE;
    let effects = dispatch(Action::OpenUrl(url.to_string()), &mut app);
    assert!(effects.is_empty());

    assert_eq!(
        agent_scrollback_len(&app),
        before + 1,
        "must push a system message with the URL"
    );
    let text = last_system_text(&app, AgentId(0));
    assert_eq!(
        text,
        crate::app::link_opener::browser_unavailable_message(url)
    );
    let toast = test_agent(&app, AgentId(0))
        .toast
        .as_ref()
        .map(|(m, _)| m.as_str());
    assert_eq!(toast, Some("Browser unavailable - URL shown above"));

    // SAFETY: serialized via `serial_test`; restore the env for other tests.
    unsafe { std::env::remove_var("GROK_TEST_OPEN_URL_FILE") };
}

/// A successful open (the `GROK_TEST_OPEN_URL_FILE` write succeeds) must not spam a fallback system message.
#[serial_test::serial(GROK_TEST_OPEN_URL_FILE)]
#[test]
fn open_url_does_not_show_fallback_when_opener_succeeds() {
    let url_file =
        std::env::temp_dir().join(format!("grok-open-url-ok-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&url_file);
    // SAFETY: serialized via `serial_test`.
    unsafe { std::env::set_var("GROK_TEST_OPEN_URL_FILE", &url_file) };

    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    let url = UPSELL_URL_PAYG;
    let _ = dispatch(Action::OpenUrl(url.to_string()), &mut app);

    assert_eq!(
        agent_scrollback_len(&app),
        before,
        "successful open must not push a fallback system message"
    );
    let recorded = std::fs::read_to_string(&url_file).unwrap_or_default();
    assert!(
        recorded.lines().any(|l| l == url),
        "opener seam must record the URL; got {recorded:?}"
    );

    // SAFETY: serialized via `serial_test`.
    unsafe { std::env::remove_var("GROK_TEST_OPEN_URL_FILE") };
    let _ = std::fs::remove_file(&url_file);
}

/// Welcome has no scrollback: browser-unavailable OpenUrl must put up a single-line toast that includes the full URL.
/// No `\n`; the welcome painter is one row. Privacy-banner Terms/Policy clicks hit this path.
#[serial_test::serial(GROK_TEST_OPEN_URL_FILE)]
#[test]
fn open_url_welcome_toasts_single_line_url_when_browser_unavailable() {
    let bad = std::env::temp_dir().join(format!(
        "grok-open-url-welcome-missing-{}/out.txt",
        std::process::id()
    ));
    // SAFETY: serialized via `serial_test` so no other test races the env var.
    unsafe { std::env::set_var("GROK_TEST_OPEN_URL_FILE", &bad) };

    let mut app = test_app();
    assert!(
        matches!(app.active_view, ActiveView::Welcome),
        "fixture must start on welcome"
    );

    use crate::app::link_opener::browser_unavailable_line;

    let terms = crate::views::privacy_banner::PRIVACY_BANNER_TERMS_URL;
    let effects = dispatch(Action::OpenUrl(terms.to_string()), &mut app);
    assert!(effects.is_empty());
    let toast = app
        .welcome_toast
        .as_ref()
        .map(|(m, _)| m.as_str())
        .unwrap_or("");
    // Structure only: clipboard delivery varies by host, so do not lock the exact "copied" phrase here (constructor unit tests cover both arms)
    assert!(toast.starts_with(terms), "{toast}");
    assert!(!toast.contains('\n'), "{toast}");
    assert!(
        toast == browser_unavailable_line(terms, true)
            || toast == browser_unavailable_line(terms, false),
        "welcome toast must match a delivery-honest line form: {toast}"
    );

    // Policy URL is shorter than terms (compile-time constants); second toast replaces the first in welcome toast state
    let policy = crate::views::privacy_banner::PRIVACY_BANNER_POLICY_URL;
    let _ = dispatch(Action::OpenUrl(policy.to_string()), &mut app);
    let toast = app
        .welcome_toast
        .as_ref()
        .map(|(m, _)| m.as_str())
        .unwrap_or("");
    assert!(toast.starts_with(policy), "{toast}");
    assert!(
        toast == browser_unavailable_line(policy, true)
            || toast == browser_unavailable_line(policy, false),
        "second welcome toast must match a delivery-honest line form: {toast}"
    );

    // SAFETY: serialized via `serial_test`; restore the env for other tests.
    unsafe { std::env::remove_var("GROK_TEST_OPEN_URL_FILE") };
}

/// Credit-limit upsell Q&A submit routes through OpenUrl; when the browser is unavailable the full option URL must land in scrollback.
#[serial_test::serial(GROK_TEST_OPEN_URL_FILE)]
#[test]
fn credit_limit_upsell_submit_shows_url_when_browser_unavailable() {
    use crate::app::agent_view::translate_local_submit_for_test;
    use crate::app::app_view::InputOutcome;
    use crate::views::question_view::{LocalQuestionKind, QuestionSelection};

    let bad = std::env::temp_dir().join(format!(
        "grok-open-url-upsell-missing-{}/out.txt",
        std::process::id()
    ));
    // SAFETY: serialized via `serial_test`.
    unsafe { std::env::set_var("GROK_TEST_OPEN_URL_FILE", &bad) };

    let mut app = test_app_with_agent();
    open_upsell_qa(&mut app, CreditLimitUpsellMode::UnifiedCredits);
    let mut qv = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .question_view
        .take()
        .expect("expected credit-limit upsell modal");
    // Select option 1, "Buy more credits" (credits / usage URL)
    set_first_selection(&mut qv, QuestionSelection::Single(Some(1)));
    let kind = LocalQuestionKind::CreditLimitUpsell {
        choices: vec![
            distill_telemetry::events::CreditLimitChoice::UpgradeTier,
            distill_telemetry::events::CreditLimitChoice::PurchaseCredits,
            distill_telemetry::events::CreditLimitChoice::RetryLastPrompt,
        ],
    };
    let InputOutcome::Action(Action::OpenUrl(url)) =
        translate_local_submit_for_test(&qv, kind, false)
    else {
        panic!("expected OpenUrl from upsell submit");
    };
    assert_eq!(url, UPSELL_URL_PAYG);

    let before = agent_scrollback_len(&app);
    let _ = dispatch(Action::OpenUrl(url.clone()), &mut app);
    let text = last_system_text(&app, AgentId(0));
    assert_eq!(agent_scrollback_len(&app), before + 1);
    assert!(
        text.contains(&url),
        "upsell URL missing from fallback: {text}"
    );

    // SAFETY: serialized via `serial_test`.
    unsafe { std::env::remove_var("GROK_TEST_OPEN_URL_FILE") };
}

#[test]
fn billing_fetched_clears_usage_modal_loading() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    dispatch_billing(
        &mut app,
        Some(test_bal(50.0)),
        true,
        Some("SuperGrok".into()),
    );
    let agent = test_agent(&app, AgentId(0));
    let Some(crate::views::modal::ActiveModal::UsageInfo { state }) = agent.active_modal.as_ref()
    else {
        panic!("expected the usage modal to be open");
    };
    assert!(!state.billing_loading);
    assert!(state.billing_error.is_none());
    assert_eq!(state.ctx.subscription_tier.as_deref(), Some("SuperGrok"));
    // The modal renders from the agent's cached billing mirrors.
    assert_eq!(agent.credit_balance.as_ref().unwrap().usage_pct, 50.0);
}

#[test]
fn background_billing_reply_does_not_settle_modal_loading() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    // A turn-end refresh (nonce 0) lands while the modal's own fetch is in flight: mirrors update, but the modal's loading/error flags don't
    dispatch(
        Action::TaskComplete(TaskResult::BillingError {
            agent_id: AgentId(0),
            error: "background boom".to_string(),
            silent: true,
            nonce: Default::default(),
        }),
        &mut app,
    );
    let Some(crate::views::modal::ActiveModal::UsageInfo { state }) =
        test_agent(&app, AgentId(0)).active_modal.as_ref()
    else {
        panic!("expected the usage modal to be open");
    };
    assert!(state.billing_loading, "still waiting on its own fetch");
    assert!(state.billing_error.is_none());
}

#[test]
fn billing_error_surfaces_in_usage_modal_without_scrollback() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    let before = agent_scrollback_len(&app);
    let nonce = open_usage_modal_nonce(&app);
    dispatch(
        Action::TaskComplete(TaskResult::BillingError {
            agent_id: AgentId(0),
            error: "billing boom".to_string(),
            silent: true,
            nonce,
        }),
        &mut app,
    );
    let Some(crate::views::modal::ActiveModal::UsageInfo { state }) =
        test_agent(&app, AgentId(0)).active_modal.as_ref()
    else {
        panic!("expected the usage modal to be open");
    };
    assert!(!state.billing_loading);
    assert_eq!(state.billing_error.as_deref(), Some("billing boom"));
    assert_eq!(agent_scrollback_len(&app), before);
}
