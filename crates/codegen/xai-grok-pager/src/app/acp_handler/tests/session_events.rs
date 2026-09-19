#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    // ── apply_session_event ────────────────────────────────────────────

    #[test]
    fn apply_compaction_started_sets_activity() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "hi".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(1),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        let update = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 90000,
            context_window: 131072,
            percentage: 85,
            reason: "threshold".into(),
        };
        assert!(apply_session_event(&update, &mut session, &mut scrollback, false));
        assert!(
            session.in_flight_prompt.is_none(),
            "compaction start implies server activity — cancel must not rewind prompt"
        );
        assert_eq!(
            session.compact_held_prompt.as_ref().map(|p| p.text.as_str()),
            Some("hi"),
            "hold prompt text for re-auth auto-resubmit if compact fails with auth"
        );
        match last_session_event(&scrollback) {
            Some(SessionEvent::CompactionStarted { percentage, reason }) => {
                assert_eq!(percentage, 85);
                assert_eq!(reason, "threshold", "threshold compact keeps its reason");
            }
            other => panic!("expected CompactionStarted, got {other:?}"),
        }
    }

    #[test]
    fn family_switch_compact_banner_and_idle_loader() {
        use crate::app::agent::AgentCommand;
        use xai_grok_shell::extensions::notification::MODEL_FAMILY_SWITCH_COMPACT_BANNER;

        let mut agent = make_agent(Some("s1"));
        agent.running_wake_turn = Some(crate::app::agent_view::RunningWakeTurn {
            prompt_id: "task-completed-wake".into(),
            cancel_sent: false,
        });
        let update = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 12_000,
            context_window: 200_000,
            percentage: 6,
            reason: MODEL_FAMILY_SWITCH_COMPACT_BANNER.into(),
        };
        assert!(apply_child_view_session_event(&mut agent, &update, false));
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::CompactionStarted { percentage, reason }) => {
                assert_eq!(percentage, 6);
                assert_eq!(reason, MODEL_FAMILY_SWITCH_COMPACT_BANNER);
            }
            other => panic!("expected family-switch CompactionStarted, got {other:?}"),
        }
        assert!(
            agent.session.state.is_compact_running(),
            "idle family-switch compact must own the turn-status command so the loader shows"
        );
        assert!(
            matches!(
                agent.session.state,
                crate::app::agent::AgentState::CommandRunning {
                    command: AgentCommand::SwitchModelCompact,
                    ..
                }
            ),
            "got {:?}",
            agent.session.state
        );
        assert!(
            agent.turn_started_at.is_some(),
            "turn timer needs turn_started_at, same as /compact"
        );
        assert!(
            agent.running_wake_turn.is_none(),
            "family-switch compact must drop a leftover wake marker like /compact"
        );

        let completed = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(12_000),
            tokens_after: 4_000,
            elapsed_ms: Some(1_500),
            summary_preview: None,
        };
        assert!(apply_child_view_session_event(&mut agent, &completed, false));
        assert!(
            agent.session.state.is_idle(),
            "family-switch compact must return to idle when it finishes"
        );
        assert!(agent.turn_started_at.is_none());
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::CompactionCompleted {
                tokens_before,
                tokens_after,
                elapsed_ms,
            }) => {
                assert_eq!(tokens_before, Some(12_000));
                assert_eq!(tokens_after, 4_000);
                assert_eq!(elapsed_ms, Some(1_500));
            }
            other => panic!("idle family-switch compact must flush the outcome immediately, got {other:?}"),
        }
    }

    #[test]
    fn family_switch_compact_failed_returns_to_idle() {
        use crate::app::agent::AgentCommand;
        use xai_grok_shell::extensions::notification::MODEL_FAMILY_SWITCH_COMPACT_BANNER;

        let mut agent = make_agent(Some("s1"));
        let started = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 12_000,
            context_window: 200_000,
            percentage: 6,
            reason: MODEL_FAMILY_SWITCH_COMPACT_BANNER.into(),
        };
        assert!(apply_child_view_session_event(&mut agent, &started, false));
        assert!(matches!(
            agent.session.state,
            crate::app::agent::AgentState::CommandRunning {
                command: AgentCommand::SwitchModelCompact,
                ..
            }
        ));

        let failed = XaiSessionUpdate::AutoCompactFailed {
            error: "compaction failed".into(),
        };
        assert!(apply_child_view_session_event(&mut agent, &failed, false));
        assert!(
            agent.session.state.is_idle(),
            "family-switch compact must return to idle on failure"
        );
        assert!(agent.turn_started_at.is_none());
    }

    #[test]
    fn family_switch_compact_replay_does_not_start_command() {
        use xai_grok_shell::extensions::notification::MODEL_FAMILY_SWITCH_COMPACT_BANNER;

        let mut agent = make_agent(Some("s1"));
        agent.session.loading_replay = true;
        let update = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 12_000,
            context_window: 200_000,
            percentage: 6,
            reason: MODEL_FAMILY_SWITCH_COMPACT_BANNER.into(),
        };
        assert!(apply_child_view_session_event(&mut agent, &update, false));
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::CompactionStarted { reason, .. }) => {
                assert_eq!(reason, MODEL_FAMILY_SWITCH_COMPACT_BANNER);
            }
            other => panic!("replay must still paint the banner, got {other:?}"),
        }
        assert!(
            agent.session.state.is_idle(),
            "replayed family-switch compact must not own CommandRunning, got {:?}",
            agent.session.state
        );
        assert!(agent.turn_started_at.is_none());

        let completed = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(12_000),
            tokens_after: 4_000,
            elapsed_ms: Some(1_500),
            summary_preview: None,
        };
        assert!(apply_child_view_session_event(&mut agent, &completed, false));
        assert!(
            agent.session.state.is_idle(),
            "replay completed must leave the pane idle"
        );
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::CompactionCompleted { tokens_after, .. }) => {
                assert_eq!(tokens_after, 4_000);
            }
            other => panic!("replay must flush the compact outcome immediately, got {other:?}"),
        }
    }

    #[test]
    fn threshold_compact_while_idle_does_not_start_switch_model_command() {
        let mut agent = make_agent(Some("s1"));
        let update = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 90_000,
            context_window: 131_072,
            percentage: 85,
            reason: "threshold".into(),
        };
        assert!(apply_child_view_session_event(&mut agent, &update, false));
        assert!(
            agent.session.state.is_idle(),
            "threshold compact must not steal the /model compact command, got {:?}",
            agent.session.state
        );
        assert!(agent.turn_started_at.is_none());
    }

    /// Compact failure keeps the hold; the reauth gate in the PromptResponse handler decides whether to stash it.
    #[test]
    fn apply_compaction_failed_keeps_held_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.compact_held_prompt = Some(InFlightPrompt {
            text: "retry after login".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(1),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        for error in [
            "authentication problem — re-authenticate using /login and retry.",
            "this conversation is too large to compact.",
        ] {
            let update = XaiSessionUpdate::AutoCompactFailed {
                error: error.into(),
            };
            assert!(apply_session_event(&update, &mut session, &mut scrollback, false));
            assert_eq!(
                session.compact_held_prompt.as_ref().map(|p| p.text.as_str()),
                Some("retry after login"),
            );
        }
    }

    /// `ImageDropped` joins notes with `\n` and pushes a system block.
    /// Pin the `\n` separator so a `notes.join(" ")` regression is caught.
    #[test]
    fn apply_image_dropped_pushes_scrollback_block() {
        use crate::scrollback::block::RenderBlock;
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        let before = scrollback.len();
        let notes = vec![
            "Image 1 was dropped: corrupt.".to_string(),
            "Image 2 was dropped: too small (4×3).".to_string(),
        ];
        let update = XaiSessionUpdate::ImageDropped {
            notes: notes.clone(),
        };
        let changed = apply_session_event(&update, &mut session, &mut scrollback, false);
        assert!(changed);
        assert_eq!(scrollback.len(), before + 1);
        let entry = scrollback.entries_mut().last().expect("entry pushed");
        match &entry.block {
            RenderBlock::System(b) => {
                assert!(b.text.contains(
                    notes
                        .first()
                        .unwrap_or_else(|| panic!("missing note"))
                        .as_str()
                ));
                assert!(b.text.contains(
                    notes
                        .get(1)
                        .unwrap_or_else(|| panic!("missing note"))
                        .as_str()
                ));
                assert!(
                    b.text.contains('\n'),
                    "expected \\n separator between dropped notes, got: {:?}",
                    b.text
                );
            }
            other => panic!("expected System block, got {other:?}"),
        }
    }

    /// A successful compression needs no user action: log-only, no toast, no scrollback block, no redraw.
    /// The same holds live and on session replay.
    #[test]
    fn image_compressed_is_invisible_in_tui() {
        for replay in [false, true] {
            let mut agent = make_agent(Some("s1"));
            agent.session.loading_replay = replay;
            assert!(!apply_image_compressed(
                &mut agent,
                &[compressed_entry(1), compressed_entry(2)],
                "Compressed Image 1: 4.2 MB (3024x1964) \u{2192} 780 KB (1568x1018)",
            ));
            assert!(agent.toast.is_none(), "no toast (replay={replay})");
            assert_eq!(agent.scrollback.len(), 0, "no block (replay={replay})");
        }
    }

    /// The re-encode fallback (empty `images`) means the oversized original was kept.
    /// That warrants a persistent warning line, not a transient toast.
    #[test]
    fn image_compressed_fallback_warning_stays_in_scrollback() {
        use crate::scrollback::block::RenderBlock;
        let mut agent = make_agent(Some("s1"));
        let msg = "Image 1 could not be re-encoded under the 1.5 MB limit; the original attachment was kept.";
        assert!(apply_image_compressed(&mut agent, &[], msg));
        assert!(agent.toast.is_none(), "warning must not be transient");
        let entry = agent.scrollback.entries_mut().last().expect("block pushed");
        match &entry.block {
            RenderBlock::System(b) => assert_eq!(b.text, msg),
            other => panic!("expected System block, got {other:?}"),
        }
    }

    #[test]
    fn apply_retry_state_retrying_clears_in_flight_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "retry me".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(2),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        let retry = RetryState::Retrying {
            attempt: 1,
            max_retries: 3,
            reason: "rate limited".into(),
            error_type: None,
        };
        apply_retry_state(&retry, &mut session, &mut scrollback, false);
        assert!(
            session.in_flight_prompt.is_none(),
            "RetryState bypasses session/update in_flight hook"
        );
    }

    #[test]
    fn retry_exhausted_rate_limited_sets_flag() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();

        assert!(!session.rate_limited);
        apply_retry_state(
            &RetryState::Exhausted {
                attempts: 3,
                reason: "rate limited".into(),
                is_rate_limited: true,
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.rate_limited,
            "rate_limited flag must be set when is_rate_limited is true"
        );
    }

    #[test]
    fn retry_exhausted_rate_limited_empty_reason_uses_oauth_fallback() {
        use xai_grok_shell::sampling::error::RATE_LIMITED_USER_MESSAGE_OAUTH;

        let empty = RetryState::Exhausted {
            attempts: 3,
            reason: "".into(),
            is_rate_limited: true,
        };

        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(&empty, &mut session, &mut scrollback, false);
        match last_session_event(&scrollback) {
            Some(SessionEvent::RetryFailed { error, .. }) => {
                assert_eq!(error, RATE_LIMITED_USER_MESSAGE_OAUTH);
            }
            other => panic!("expected empty-rate-limit RetryFailed, got {other:?}"),
        }
    }

    /// Production `RetryState::Exhausted.reason` is `SamplingError::Api`'s Display: `API error (status 429 Too Many Requests): …`.
    #[test]
    fn retry_exhausted_rate_limited_surfaces_server_detail() {
        let body = "The model is currently at capacity due to high demand. Please try again.";
        let reason = format!("API error (status 429 Too Many Requests): {body}");
        let exhausted = RetryState::Exhausted {
            attempts: 3,
            reason: reason.clone(),
            is_rate_limited: true,
        };

        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(&exhausted, &mut session, &mut scrollback, false);
        match last_session_event(&scrollback) {
            Some(SessionEvent::RetryFailed { error, .. }) => {
                assert_eq!(error, body);
                assert!(!error.contains("API error (status"));
            }
            other => panic!("expected detail RetryFailed, got {other:?}"),
        }
    }

    #[test]
    fn retry_exhausted_api_key_rewrites_consumer_subscription_upsell() {
        use xai_grok_shell::sampling::error::RATE_LIMITED_USER_MESSAGE_API_KEY;

        let rpm = RetryState::Exhausted {
            attempts: 2,
            reason: "API error (status 429 Too Many Requests): \
                     Some resource has been exhausted: You are sending requests too quickly. \
                     Please slow down, or upgrade to a Grok subscription for higher limits: \
                     https://grok.com/supergrok"
                .into(),
            is_rate_limited: true,
        };

        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(&rpm, &mut session, &mut scrollback, true);
        match last_session_event(&scrollback) {
            Some(SessionEvent::RetryFailed { error, .. }) => {
                assert_eq!(error, RATE_LIMITED_USER_MESSAGE_API_KEY);
                assert!(!error.contains("grok.com/supergrok"));
            }
            other => panic!("expected API-key rate-limit RetryFailed, got {other:?}"),
        }
    }

    #[test]
    fn retry_exhausted_non_rate_limited_does_not_set_flag() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();

        apply_retry_state(
            &RetryState::Exhausted {
                attempts: 3,
                reason: "server error".into(),
                is_rate_limited: false,
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            !session.rate_limited,
            "rate_limited flag must not be set when is_rate_limited is false"
        );
    }

    /// A rate-limit exhaustion whose flattened reason carries the free-usage code sets both flags and pushes NO generic block.
    /// The driver shows the paywall modal on PromptResponse; viewers keep no marker.
    #[test]
    fn retry_exhausted_free_usage_sets_paywall_flag_without_marker() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "try me again".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(2),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });

        apply_retry_state(
            &RetryState::Exhausted {
                attempts: 0,
                reason: "API error (status 429 Too Many Requests): \
                         subscription:free-usage-exhausted: You have used all your free usage."
                    .into(),
                is_rate_limited: true,
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.rate_limited,
            "free-usage keeps rate_limited (TurnFailed/toast suppression)"
        );
        assert!(session.free_usage_blocked);
        assert_eq!(
            scrollback.len(),
            0,
            "no RetryFailed marker — the paywall modal shows instead"
        );
        assert!(
            session.in_flight_prompt.is_none(),
            "free-usage exhaustion clears in_flight_prompt like other failures"
        );
    }

    #[test]
    fn apply_retry_state_disk_full_pushes_session_event() {
        use xai_grok_shell::extensions::notification::{
            DISK_FULL_ERROR_TYPE, DISK_FULL_USER_MESSAGE,
        };
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: DISK_FULL_ERROR_TYPE.into(),
                message: DISK_FULL_USER_MESSAGE.into(),
            },
            &mut session,
            &mut scrollback,
            false,
        );
        match scrollback.last().map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                assert!(matches!(ev.event, SessionEvent::DiskFull));
                assert_eq!(ev.event.message(), DISK_FULL_USER_MESSAGE);
            }
            other => panic!("expected DiskFull session event, got {other:?}"),
        }
    }

    #[test]
    fn apply_retry_state_credit_limit_exhausted_preserves_in_flight_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "stash me".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(2),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        apply_retry_state(
            &RetryState::Exhausted {
                attempts: 3,
                reason: "status 403: run out of credits".into(),
                is_rate_limited: false,
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.credit_limit_blocked,
            "credit_limit_blocked must be set for credit-limit 403"
        );
        assert!(
            session.in_flight_prompt.is_some(),
            "in_flight_prompt must be preserved so PromptResponse handler can stash it"
        );
        assert_eq!(session.in_flight_prompt.unwrap().text, "stash me");
    }

    #[test]
    fn apply_retry_state_credit_limit_failed_preserves_in_flight_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "stash me too".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(3),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message: "status 403: run out of credits".into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.credit_limit_blocked,
            "credit_limit_blocked must be set for credit-limit 403"
        );
        assert!(
            session.in_flight_prompt.is_some(),
            "in_flight_prompt must be preserved so PromptResponse handler can stash it"
        );
        assert_eq!(session.in_flight_prompt.unwrap().text, "stash me too");
    }

    #[test]
    fn apply_retry_state_pool_402_sets_credit_limit_blocked() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "pool blocked".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(5),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message:
                    "API error (status 402 Payment Required): Remote-Code usage balance exhausted"
                        .into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.credit_limit_blocked,
            "credit_limit_blocked must be set for pool 402 balance exhausted"
        );
        assert!(session.in_flight_prompt.is_some());
    }

    #[test]
    fn apply_retry_state_non_credit_limit_failed_clears_in_flight_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "gone".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(4),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message: "internal server error".into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            !session.credit_limit_blocked,
            "credit_limit_blocked must NOT be set for non-credit-limit errors"
        );
        assert!(
            session.in_flight_prompt.is_none(),
            "in_flight_prompt must be cleared for non-credit-limit errors"
        );
    }

    #[test]
    fn is_reauthable_failure_matrix() {
        assert!(is_reauthable_failure(Some("auth"), "Unauthorized (401)"));
        assert!(is_reauthable_failure(
            Some("api"),
            "Unauthorized (401) from https://proxy/v1/responses"
        ));
        assert!(is_reauthable_failure(None, "Unauthorized (401)"));
        // legacy_auth carries its own migration guidance, so it is excluded
        assert!(!is_reauthable_failure(
            Some("legacy_auth"),
            "Unauthorized (401) ... deprecated authentication method"
        ));
        // auth_transient means the shell says the failure self-heals (refreshable credential, no sticky verdict, e.g. a post-wake network gap).
        // Even with a 401 in the message, the `/login` banner must not fire
        assert!(!is_reauthable_failure(
            Some("auth_transient"),
            "Unauthorized (401)\n\nAuthentication is temporarily unavailable"
        ));
        // Unrelated failures must not be treated as re-authable.
        assert!(!is_reauthable_failure(
            Some("api"),
            "internal server error"
        ));
        assert!(!is_reauthable_failure(Some("api"), "model not found"));
    }

    /// A 401 with `error_type == "auth"` shows the actionable re-auth prompt instead of the raw "Retry failed: Unauthorized (401) …" dump.
    #[test]
    fn apply_retry_state_auth_failure_pushes_reauth_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: "auth".into(),
                message: "Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/messages: \
                          no auth context"
                    .into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            matches!(
                last_session_event(&scrollback),
                Some(SessionEvent::ReAuthRequired)
            ),
            "auth 401 must surface the actionable re-auth prompt"
        );
        assert!(!session.credit_limit_blocked);
    }

    /// A recoverable auth failure preserves `in_flight_prompt` so the PromptResponse handler can stash it for auto-resubmit after re-auth.
    #[test]
    fn apply_retry_state_auth_failure_preserves_in_flight_prompt() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.in_flight_prompt = Some(InFlightPrompt {
            text: "retry after login".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(5),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        apply_retry_state(
            &RetryState::Failed {
                error_type: "auth".into(),
                message: "Unauthorized (401) from https://proxy/v1/messages".into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.in_flight_prompt.is_some(),
            "in_flight_prompt must be preserved on a recoverable auth failure"
        );
        assert_eq!(session.in_flight_prompt.unwrap().text, "retry after login");
    }

    /// A 401 reported with a non-auth `error_type` but an "Unauthorized (401)" message (the `SamplingErrorKind::Api` path) also prompts.
    #[test]
    fn apply_retry_state_401_message_without_auth_type_prompts_reauth() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message: "Unauthorized (401) from https://proxy/v1/responses: invalid credentials"
                    .into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(matches!(
            last_session_event(&scrollback),
            Some(SessionEvent::ReAuthRequired)
        ));
    }

    /// Legacy WebLogin auth keeps its verbose message (with `grok logout` / `grok login` guidance), not the generic re-auth prompt.
    #[test]
    fn apply_retry_state_legacy_auth_keeps_detailed_message() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: "legacy_auth".into(),
                message: "Unauthorized (401) ... deprecated authentication method (WebLogin) ... \
                          run `grok logout` then `grok login`"
                    .into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(matches!(
            last_session_event(&scrollback),
            Some(SessionEvent::RetryFailed { .. })
        ));
    }

    /// Non-auth terminal failures render the formatted RequestFailed banner (same visual treatment as 401 re-auth), not a raw RetryFailed dump.
    #[test]
    fn apply_retry_state_generic_failure_shows_request_failed_banner() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message: r#"API error (status 500 Internal Server Error): {"error":"upstream exploded"}"#.into(),
            },
            &mut session,
            &mut scrollback, false);
        match last_session_event(&scrollback) {
            Some(SessionEvent::RequestFailed {
                status,
                headline,
                detail,
            }) => {
                assert_eq!(status, Some(500));
                assert_eq!(headline, "Server error (500)");
                assert_eq!(
                    detail,
                    "Something went wrong on our side. Wait a minute and send again."
                );
            }
            other => panic!("expected RequestFailed, got {other:?}"),
        }
    }

    #[test]
    fn apply_retry_state_403_shows_clean_denied_banner() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message: "API error (status 403 Forbidden): Access to the chat endpoint is denied"
                    .into(),
            },
            &mut session,
            &mut scrollback,
            false,
        );
        match last_session_event(&scrollback) {
            Some(SessionEvent::RequestFailed {
                status,
                headline,
                detail,
            }) => {
                assert_eq!(status, Some(403));
                assert_eq!(headline, "Request denied (403)");
                assert_eq!(detail, "Access to the chat endpoint is denied");
            }
            other => panic!("expected RequestFailed, got {other:?}"),
        }
    }

    /// A context overflow shows the actionable `ContextTooLarge` prompt (not the raw `RetryFailed`).
    /// `PromptResponse` then suppresses the redundant `TurnFailed`.
    #[test]
    fn apply_retry_state_context_length_shows_context_too_large() {
        use xai_grok_shell::extensions::notification::CONTEXT_LENGTH_ERROR_TYPE;
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: CONTEXT_LENGTH_ERROR_TYPE.into(),
                message: "API error (status 500): the prompt is too long for this model's \
                          context window"
                    .into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            matches!(
                last_session_event(&scrollback),
                Some(SessionEvent::ContextTooLarge)
            ),
            "context overflow must surface the actionable ContextTooLarge prompt"
        );
    }

    /// A message worded like an overflow but without `error_type=context_length` must not take the ContextTooLarge path.
    /// The shell is what tags overflow.
    #[test]
    fn apply_retry_state_overflow_copy_without_type_is_not_context_too_large() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        apply_retry_state(
            &RetryState::Failed {
                error_type: "api".into(),
                message: "API error (status 500): the prompt is too long for this model's \
                          context window"
                    .into(),
            },
            &mut session,
            &mut scrollback,
            false,
        );
        assert!(
            matches!(
                last_session_event(&scrollback),
                Some(SessionEvent::RequestFailed { .. })
            ),
            "without error_type=context_length this is a generic banner, not overflow UX"
        );
    }

    /// The compaction handler may already have shown its "too large to compact" message.
    /// The overflow path then does NOT stack a second `ContextTooLarge` prompt on top.
    #[test]
    fn apply_retry_state_context_length_does_not_duplicate_compaction_failed() {
        use xai_grok_shell::extensions::notification::CONTEXT_LENGTH_ERROR_TYPE;
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        scrollback.push_block(RenderBlock::session_event(SessionEvent::CompactionFailed {
            error: "this conversation is too large to compact.".into(),
        }));
        apply_retry_state(
            &RetryState::Failed {
                error_type: CONTEXT_LENGTH_ERROR_TYPE.into(),
                message: "the prompt is too long for this model's context window".into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            matches!(
                last_session_event(&scrollback),
                Some(SessionEvent::CompactionFailed { .. })
            ),
            "must not push a duplicate prompt on top of CompactionFailed"
        );
    }

    #[test]
    fn apply_compaction_completed_defers_message_until_turn_end() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        session.set_compaction_activity(Some(TurnActivity::AutoCompacting));
        let update = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(858_000),
            tokens_after: 66_000,
            elapsed_ms: Some(500),
            summary_preview: None,
        };
        assert!(apply_session_event(&update, &mut session, &mut scrollback, false));
        assert_eq!(
            scrollback.len(),
            0,
            "live compaction completion must be deferred, not pushed immediately"
        );

        session.note_context_used(43_000);

        session.finish_turn(&mut scrollback,
        );
        match last_session_event(&scrollback) {
            Some(SessionEvent::CompactionCompleted {
                tokens_before,
                tokens_after,
                ..
            }) => {
                assert_eq!(tokens_before, Some(858_000));
                assert_eq!(
                    tokens_after, 43_000,
                    "must flush the model-confirmed count, not the 66k estimate"
                );
            }
            other => panic!("expected deferred CompactionCompleted, got {other:?}"),
        }
    }

    #[test]
    fn apply_compaction_completed_falls_back_to_estimate_without_confirmation() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        let update = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(90_000),
            tokens_after: 20_000,
            elapsed_ms: Some(500),
            summary_preview: None,
        };
        assert!(apply_session_event(&update, &mut session, &mut scrollback, false));
        session.finish_turn(&mut scrollback,
        );
        match last_session_event(&scrollback) {
            Some(SessionEvent::CompactionCompleted { tokens_after, .. }) => {
                assert_eq!(
                    tokens_after, 20_000,
                    "fallback to estimate when unconfirmed"
                );
            }
            other => panic!("expected fallback CompactionCompleted, got {other:?}"),
        }
    }

    #[test]
    fn apply_compaction_completed_renders_immediately_during_replay() {
        let mut session = make_session(Some("s1"));
        session.loading_replay = true;
        let mut scrollback = ScrollbackState::new();
        let update = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(90_000),
            tokens_after: 20_000,
            elapsed_ms: Some(500),
            summary_preview: None,
        };
        assert!(apply_session_event(&update, &mut session, &mut scrollback, false));
        match last_session_event(&scrollback) {
            Some(SessionEvent::CompactionCompleted { tokens_after, .. }) => {
                assert_eq!(
                    tokens_after, 20_000,
                    "replay renders the recorded count immediately"
                );
            }
            other => panic!("expected immediate CompactionCompleted on replay, got {other:?}"),
        }
    }

    #[test]
    fn deferred_compaction_flushes_confirmed_count_over_estimate_refresh() {
        let mut agent = make_agent(Some("s1"));
        agent
            .session
            .set_compaction_activity(Some(TurnActivity::AutoCompacting));

        let update = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(858_000),
            tokens_after: 66_000,
            elapsed_ms: Some(500),
            summary_preview: None,
        };
        assert!(apply_session_event(
            &update,
            &mut agent.session,
            &mut agent.scrollback, false));

        refresh_context_used(&mut agent, 66_000);
        confirm_context_used(&mut agent, 43_000);

        agent.session.finish_turn(&mut agent.scrollback,
        );
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::CompactionCompleted {
                tokens_before,
                tokens_after,
                ..
            }) => {
                assert_eq!(tokens_before, Some(858_000));
                assert_eq!(
                    tokens_after, 43_000,
                    "deferred line must flush the confirmed 43k, not the 66k \
                     estimate refresh that updated the bar first"
                );
            }
            other => panic!("expected deferred CompactionCompleted, got {other:?}"),
        }
    }

    #[test]
    fn compaction_lifecycle_refreshes_context_bar_from_trigger_counts() {
        // Started refreshes with the count the trigger fired on; the banner percentage derives from it
        // Completed refreshes with the post-compact count
        // Failed/Cancelled carry no count; the next meta.totalTokens restamps
        let started = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 460_231,
            context_window: 500_000,
            percentage: 92,
            reason: "threshold".into(),
        };
        assert_eq!(compaction_context_refresh(&started), Some(460_231));
        let completed = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(460_231),
            tokens_after: 21_502,
            elapsed_ms: Some(500),
            summary_preview: None,
        };
        assert_eq!(compaction_context_refresh(&completed), Some(21_502));
        let failed = XaiSessionUpdate::AutoCompactFailed { error: "e".into() };
        assert_eq!(compaction_context_refresh(&failed), None);
        let cancelled = XaiSessionUpdate::AutoCompactCancelled {
            reason: xai_grok_shell::extensions::notification::AutoCompactCancelReason::UserCancelled,
        };
        assert_eq!(compaction_context_refresh(&cancelled), None);
    }

    #[test]
    fn apply_unhandled_event_returns_false() {
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();
        let update = XaiSessionUpdate::MemoryFlushStarted;
        assert!(!apply_session_event(&update, &mut session, &mut scrollback, false));
    }

    // ── handle_child_session_notification ──────────────────────────────

    #[test]
    fn child_compact_completed_updates_subagent_info() {
        let mut agent = make_agent(Some("root-sess"));
        let child_sid = "child-sess-1";
        agent
            .subagent_sessions
            .insert(child_sid.into(), make_subagent_info(child_sid));
        let child_view = make_agent(Some(child_sid));
        agent
            .insert_test_child(child_sid.into(), Box::new(child_view));

        let update = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(90000),
            tokens_after: 25000,
            elapsed_ms: Some(300),
            summary_preview: None,
        };
        let changed = handle_child_session_notification(update, child_sid, &mut agent, false);
        assert!(changed);

        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert_eq!(info.attempt.tokens_used, Some(25000));
        // 25000 tokens of the default 131072 window rounds to 19 percent
        assert_eq!(info.attempt.context_usage_pct, Some(19));

        // The child view's context_state.used (context-bar numerator) must also be reset; see handle_child_session_notification
        let child_view = agent.subagent_views.get(child_sid).unwrap();
        assert_eq!(
            child_view.context_state.as_ref().map(|c| c.used),
            Some(25000)
        );
    }

    #[test]
    fn child_compact_started_refreshes_context_used() {
        // Started carries the count the trigger fired on; the child view must refresh like the root path so the subagent bar matches the banner
        let mut agent = make_agent(Some("root-sess"));
        let child_sid = "child-sess-3";
        agent
            .subagent_sessions
            .insert(child_sid.into(), make_subagent_info(child_sid));
        let mut child_view = make_agent(Some(child_sid));
        child_view.context_state = Some(xai_grok_shell::session::ContextInfo::from_notification(
            90_000, 131_072,
        ));
        agent
            .insert_test_child(child_sid.into(), Box::new(child_view));

        let update = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 95_000,
            context_window: 131_072,
            percentage: 72,
            reason: "threshold".into(),
        };
        let _ = handle_child_session_notification(update, child_sid, &mut agent, false);

        let child_view = agent.subagent_views.get(child_sid).unwrap();
        assert_eq!(
            child_view.context_state.as_ref().map(|c| c.used),
            Some(95_000)
        );
    }

    #[test]
    fn child_notification_without_view_returns_false() {
        let mut agent = make_agent(Some("root-sess"));
        // No child view is registered
        let update = XaiSessionUpdate::AutoCompactStarted {
            tokens_used: 90000,
            context_window: 131072,
            percentage: 85,
            reason: "threshold".into(),
        };
        let changed = handle_child_session_notification(update, "unknown-child", &mut agent, false);
        assert!(!changed);
    }

    #[test]
    fn child_compact_completed_without_view_returns_false() {
        let mut agent = make_agent(Some("root-sess"));
        let child_sid = "child-sess-2";
        // SubagentInfo exists but no child view (race between notification and spawn).
        agent
            .subagent_sessions
            .insert(child_sid.into(), make_subagent_info(child_sid));

        let update = XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(90000),
            tokens_after: 25000,
            elapsed_ms: Some(300),
            summary_preview: None,
        };
        let changed = handle_child_session_notification(update, child_sid, &mut agent, false);
        // No child_view means nothing visible changed, so it must not trigger a redraw
        assert!(!changed);
        // SubagentInfo is still updated for data correctness even though nothing redraws
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert_eq!(info.attempt.tokens_used, Some(25000));
        assert_eq!(info.attempt.context_usage_pct, Some(19));
    }

    #[test]
    fn child_unknown_event_returns_false() {
        let mut agent = make_agent(Some("root-sess"));
        let update = XaiSessionUpdate::MemoryFlushStarted;
        let changed = handle_child_session_notification(update, "child-1", &mut agent, false);
        assert!(!changed);
    }

    #[test]
    fn tool_call_delta_chunk_sets_writing_activity() {
        let mut app = make_app_with_agent("sess-1");
        app.agents.get_mut(&AgentId(0)).unwrap().session.state = AgentState::TurnRunning;

        let changed = handle(
            make_ext_session_notification(
                "sess-1",
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("spawn_subagent".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        assert!(changed, "first delta must request a redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let Some(TurnActivity::WritingToolCall(writing)) = agent.session.tracker.activity() else {
            panic!("expected WritingToolCall activity");
        };
        assert_eq!(writing.label(), "Writing subagent prompt…");
    }

    /// A delta-first turn still counts as first activity: the rewind stash drops and the TTFA timestamp is stamped.
    #[test]
    fn tool_call_delta_chunk_clears_in_flight_prompt() {
        let mut app = make_app_with_agent("sess-1");
        let started = Instant::now();
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.turn_started_at = Some(started);
            agent.session.in_flight_prompt = Some(InFlightPrompt {
                text: "hi".into(),
                images: Vec::new(),
                scrollback_entry: EntryId::new(1),
                combined_scrollback_entries: Vec::new(),
                chip_elements: Vec::new(),
            });
        }

        let _ = handle(
            make_ext_session_notification(
                "sess-1",
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("write".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.in_flight_prompt.is_none(),
            "first activity on the delta rail must drop the rewind stash"
        );
        assert_eq!(
            agent.first_activity_logged_for,
            Some(started),
            "TTFA must be stamped for a delta-first turn"
        );
    }

    /// Hook / image-intake diagnostics must not consume the Ctrl+C rewind stash.
    #[test]
    fn hook_and_image_intake_notifications_keep_in_flight_prompt() {
        let updates = [
            XaiSessionUpdate::HookExecution {
                event_name: "user_prompt_submit".into(),
                tool_name: None,
                prompt_id: Some("p1".into()),
                runs: vec![],
            },
            XaiSessionUpdate::ImageCompressed {
                images: vec![],
                message: "resized".into(),
            },
            XaiSessionUpdate::ImageDropped { notes: vec![] },
        ];
        for update in updates {
            let label = format!("{update:?}");
            let mut app = make_app_with_agent("sess-1");
            {
                let agent = app.agents.get_mut(&AgentId(0)).unwrap();
                agent.session.state = AgentState::TurnRunning;
                agent.turn_started_at = Some(Instant::now());
                agent.session.in_flight_prompt = Some(InFlightPrompt {
                    text: "hi".into(),
                    images: Vec::new(),
                    scrollback_entry: EntryId::new(1),
                    combined_scrollback_entries: Vec::new(),
                    chip_elements: Vec::new(),
                });
            }

            let _ = handle(make_ext_session_notification("sess-1", update), &mut app);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert!(
                agent.session.in_flight_prompt.is_some(),
                "intake diagnostic must not eat the rewind stash: {label}"
            );
        }
    }

    /// Deltas carry no prompt id, so while a wake turn is in flight the chunk is dropped whole.
    /// That means no tracker write, no rewind-stash consumption, no TTFA stamp.
    #[test]
    fn wake_gated_delta_chunk_is_fully_inert() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.turn_started_at = Some(Instant::now());
            agent.session.in_flight_prompt = Some(InFlightPrompt {
                text: "hi".into(),
                images: Vec::new(),
                scrollback_entry: EntryId::new(1),
                combined_scrollback_entries: Vec::new(),
                chip_elements: Vec::new(),
            });
            agent
                .session
                .tracker
                .set_retry_activity(Some(crate::acp::tracker::TurnActivity::Retrying {
                    attempt: 1,
                    max_retries: 3,
                    reason: "overloaded".into(),
                    error_type: None,
                }));
            agent.running_wake_turn = Some(crate::app::agent_view::RunningWakeTurn {
                prompt_id: "task-completed-1".into(),
                cancel_sent: false,
            });
        }

        let changed = handle(
            make_ext_session_notification(
                "sess-1",
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("write".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        assert!(!changed);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.in_flight_prompt.is_some(),
            "a dropped delta must not eat the rewind stash"
        );
        assert_eq!(agent.first_activity_logged_for, None);
        assert!(
            matches!(
                agent.session.tracker.activity(),
                Some(TurnActivity::Retrying { .. })
            ),
            "wake-attributable delta must not clear the local turn's retry override"
        );
    }

    /// Defense-in-depth: the shell never emits replay-marked deltas.
    #[test]
    fn tool_call_delta_chunk_ignored_during_replay() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.loading_replay = true;
            agent.session.in_flight_prompt = Some(InFlightPrompt {
                text: "hi".into(),
                images: Vec::new(),
                scrollback_entry: EntryId::new(1),
                combined_scrollback_entries: Vec::new(),
                chip_elements: Vec::new(),
            });
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let payload = SessionNotification {
            session_id: acp::SessionId::new("sess-1"),
            update: XaiSessionUpdate::ToolCallDeltaChunk {
                tool_call_id: Some("call_1".into()),
                tool_index: 0,
                name: Some("write".into()),
                arguments_delta: Some("{".into()),
            },
            meta: Some(serde_json::json!({ "isReplay": true })),
        };
        let raw = serde_json::value::to_raw_value(&payload).unwrap();
        let request = acp::ExtNotification::new("x.ai/session_notification", raw.into());
        let changed = handle(
            AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
                request,
                response_tx: tx,
            }),
            &mut app,
        );
        assert!(!changed);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.session.tracker.activity(),
            None,
            "replayed delta must not set a writing label"
        );
        assert!(
            agent.session.in_flight_prompt.is_some(),
            "a dropped delta must not eat the rewind stash"
        );
    }

    // ── apply_retry_state ─────────────────────────────────────────────

    #[test]
    fn retry_failed_encrypted_content_sets_model_incompatible() {
        use xai_grok_shell::extensions::notification::RetryState;
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();

        assert!(!session.model_incompatible);
        apply_retry_state(
            &RetryState::Failed {
                error_type: "encrypted_content_mismatch".into(),
                message: "incompatible history".into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            session.model_incompatible,
            "encrypted_content_mismatch should set model_incompatible flag"
        );
    }

    #[test]
    fn retry_failed_other_type_does_not_set_model_incompatible() {
        use xai_grok_shell::extensions::notification::RetryState;
        let mut session = make_session(Some("s1"));
        let mut scrollback = ScrollbackState::new();

        apply_retry_state(
            &RetryState::Failed {
                error_type: "api_400".into(),
                message: "bad request".into(),
            },
            &mut session,
            &mut scrollback, false);
        assert!(
            !session.model_incompatible,
            "non-encrypted_content error types must not set model_incompatible"
        );
    }

    fn summary_generated_ext(
        session_id: &str,
        title: &str,
        title_is_manual: bool,
    ) -> acp::ExtNotification {
        let meta = if title_is_manual {
            Some(xai_grok_shell::extensions::notification::title_is_manual_meta())
        } else {
            None
        };
        let notif = SessionNotification {
            session_id: acp::SessionId::new(session_id),
            update: XaiSessionUpdate::SessionSummaryGenerated {
                session_summary: title.into(),
            },
            meta,
        };
        let raw = serde_json::value::to_raw_value(&notif).unwrap();
        acp::ExtNotification::new("x.ai/session_notification", std::sync::Arc::from(raw))
    }

    #[test]
    fn manual_title_notification_sets_display_name_without_entity_decode() {
        let mut app = make_app_with_agent("sess-1");
        let changed = handle_session_notification(
            &summary_generated_ext("sess-1", "a &amp; b", true),
            &mut app,
        );
        assert!(changed);
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(
            agent.display_name.as_deref(),
            Some("a &amp; b"),
            "manual meta must set display_name from the raw title"
        );
        assert_eq!(
            agent.generated_session_title.as_deref(),
            Some("a &amp; b"),
            "manual meta must skip HTML-entity decode"
        );
    }

    #[test]
    fn auto_title_blank_after_sanitize_does_not_clear_existing() {
        let mut app = make_app_with_agent("sess-1");
        app.agents.get_mut(&AgentId(0)).unwrap().generated_session_title =
            Some("Keep Me".into());
        assert!(handle_session_notification(
            &summary_generated_ext("sess-1", "\u{1b}\u{07}", false),
            &mut app,
        ));
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"))
                .generated_session_title
                .as_deref(),
            Some("Keep Me"),
            "control-only auto replay must not wipe an existing title"
        );
    }

    #[test]
    fn auto_title_notification_does_not_set_display_name() {
        let mut app = make_app_with_agent("sess-1");
        let changed = handle_session_notification(
            &summary_generated_ext("sess-1", "a &amp; b", false),
            &mut app,
        );
        assert!(changed);
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(
            agent.display_name.is_none(),
            "auto titles must not promote to display_name"
        );
        assert_eq!(
            agent.generated_session_title.as_deref(),
            Some("a & b"),
            "auto titles still HTML-entity-decode"
        );
    }

    #[test]
    fn auto_title_notification_does_not_clobber_existing_display_name() {
        let mut app = make_app_with_agent("sess-1");
        app.agents.get_mut(&AgentId(0)).unwrap().display_name = Some("Pinned".into());
        let changed = handle_session_notification(
            &summary_generated_ext("sess-1", "a &amp; b", false),
            &mut app,
        );
        assert!(changed);
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(agent.display_name.as_deref(), Some("Pinned"));
        assert_eq!(agent.generated_session_title.as_deref(), Some("a & b"));
    }

    #[test]
    fn manual_meta_false_clears_display_name() {
        let mut app = make_app_with_agent("sess-1");
        app.agents.get_mut(&AgentId(0)).unwrap().display_name = Some("Pinned".into());
        app.agents.get_mut(&AgentId(0)).unwrap().generated_session_title =
            Some("Pinned".into());
        let n = SessionNotification {
            session_id: acp::SessionId::new("sess-1"),
            update: XaiSessionUpdate::SessionSummaryGenerated {
                session_summary: String::new(),
            },
            meta: Some(serde_json::json!({ "x.ai/titleIsManual": false })),
        };
        let raw = serde_json::value::to_raw_value(&n).unwrap();
        let notif = acp::ExtNotification::new("x.ai/session_notification", std::sync::Arc::from(raw));
        assert!(handle_session_notification(&notif, &mut app));
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(
            agent.display_name.is_none(),
            "explicit unpin meta must clear display_name"
        );
        assert!(
            agent.generated_session_title.is_none(),
            "empty unpin summary must drop the follower's manual generated title"
        );
    }

    #[test]
    fn manual_meta_false_empty_summary_keeps_leftover_auto_title() {
        let mut app = make_app_with_agent("sess-1");
        app.agents.get_mut(&AgentId(0)).unwrap().display_name = Some("Pinned".into());
        app.agents.get_mut(&AgentId(0)).unwrap().generated_session_title =
            Some("Auto".into());
        let n = SessionNotification {
            session_id: acp::SessionId::new("sess-1"),
            update: XaiSessionUpdate::SessionSummaryGenerated {
                session_summary: String::new(),
            },
            meta: Some(serde_json::json!({ "x.ai/titleIsManual": false })),
        };
        let raw = serde_json::value::to_raw_value(&n).unwrap();
        let notif = acp::ExtNotification::new("x.ai/session_notification", std::sync::Arc::from(raw));
        assert!(handle_session_notification(&notif, &mut app));
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(
            agent.display_name.is_none(),
            "explicit unpin meta must still clear display_name"
        );
        assert_eq!(
            agent.generated_session_title.as_deref(),
            Some("Auto"),
            "empty unpin fan-out must not wipe a leftover auto title"
        );
    }

    #[test]
    fn auto_title_notification_strips_controls_and_caps() {
        use xai_grok_shell::session::persistence::MAX_TITLE_SCALARS;
        let mut app = make_app_with_agent("sess-1");
        let dirty = format!(
            "ok\u{1b}]0;PWNED\u{07}{}",
            "é".repeat(MAX_TITLE_SCALARS + 5)
        );
        assert!(handle_session_notification(
            &summary_generated_ext("sess-1", &dirty, false),
            &mut app,
        ));
        const PREFIX: &str = "ok]0;PWNED";
        let expected = format!(
            "{PREFIX}{}",
            "é".repeat(MAX_TITLE_SCALARS - PREFIX.chars().count())
        );
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(
            agent.display_name.is_none(),
            "auto titles must not promote to display_name"
        );
        assert_eq!(
            agent.generated_session_title.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn manual_title_notification_strips_controls_and_caps() {
        use xai_grok_shell::session::persistence::MAX_TITLE_SCALARS;
        let mut app = make_app_with_agent("sess-1");
        let dirty = format!("ok\u{1b}]0;PWNED\u{07}{}", "é".repeat(MAX_TITLE_SCALARS + 5));
        assert!(handle_session_notification(
            &summary_generated_ext("sess-1", &dirty, true),
            &mut app,
        ));
        const PREFIX: &str = "ok]0;PWNED";
        let expected = format!(
            "{PREFIX}{}",
            "é".repeat(MAX_TITLE_SCALARS - PREFIX.chars().count())
        );
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(agent.display_name.as_deref(), Some(expected.as_str()));
        assert_eq!(
            agent.generated_session_title.as_deref(),
            Some(expected.as_str())
        );
    }

    // ── HooksChanged (x.ai/session/update push) ─────────────────────────

    fn hooks_changed_ext(
        session_id: &str,
        hooks: Vec<xai_hooks_plugins_types::HookInfo>,
    ) -> acp::ExtNotification {
        let notif = SessionNotification {
            session_id: acp::SessionId::new(session_id),
            update: XaiSessionUpdate::HooksChanged {
                hooks,
                project_trusted: true,
                load_errors: Vec::new(),
            },
            meta: None,
        };
        let raw = serde_json::value::to_raw_value(&notif).unwrap();
        acp::ExtNotification::new("x.ai/session_notification", std::sync::Arc::from(raw))
    }

    fn push_hook(name: &str, source_dir: &str) -> xai_hooks_plugins_types::HookInfo {
        xai_hooks_plugins_types::HookInfo {
            name: name.to_string(),
            event: xai_hooks_plugins_types::HookEvent::PreToolUse,
            handler_type: xai_hooks_plugins_types::HookHandlerType::Command,
            matcher: None,
            command: Some("/bin/true".to_string()),
            url: None,
            timeout_ms: 10_000,
            source_dir: source_dir.to_string(),
            disabled: false,
            pinned: false,
            removable: false,
        }
    }

    /// The `HooksChanged` push is the channel late-arriving hooks come through.
    /// Driven end-to-end through `handle_session_notification`: an empty first push must not finish group-collapse seeding, the first non-empty push applies the collapsed default, and later pushes preserve expand state.
    #[test]
    fn hooks_changed_push_seeds_group_collapse_on_first_non_empty_delivery() {
        use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab, TabDataState};

        let mut app = make_app_with_agent("sess-1");
        app.agents.get_mut(&AgentId(0)).unwrap().extensions_modal =
            Some(ExtensionsModalState::new(ExtensionsTab::Hooks));

        // Empty first push: loads data but must leave seeding open.
        assert!(handle_session_notification(
            &hooks_changed_ext("sess-1", Vec::new()),
            &mut app,
        ));
        {
            let modal = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).extensions_modal.as_ref().unwrap();
            assert!(matches!(modal.hooks_data, TabDataState::Loaded(_)));
            assert!(
                modal.hooks_collapsed_groups.is_empty(),
                "empty first push must not seed any collapsed groups"
            );
        }

        // First non-empty push: every source group gets the collapsed default.
        let hooks = vec![push_hook("a", "/src1"), push_hook("b", "/src2")];
        assert!(handle_session_notification(
            &hooks_changed_ext("sess-1", hooks.clone()),
            &mut app,
        ));
        {
            let modal = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).extensions_modal.as_ref().unwrap();
            assert!(modal.hooks_collapsed_groups.contains("/src1"));
            assert!(modal.hooks_collapsed_groups.contains("/src2"));
        }

        // User expands /src1; a later push must not re-collapse it.
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .extensions_modal
            .as_mut()
            .unwrap()
            .hooks_collapsed_groups
            .remove("/src1");
        assert!(handle_session_notification(
            &hooks_changed_ext("sess-1", hooks),
            &mut app,
        ));
        let modal = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).extensions_modal.as_ref().unwrap();
        assert!(!modal.hooks_collapsed_groups.contains("/src1"));
        assert!(modal.hooks_collapsed_groups.contains("/src2"));
    }

