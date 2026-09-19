// Modified for Distill by Samuel Fajreldines, 2026.
use crate::AuthMode;
use crate::GrokAuth;
use crate::token_output::parse_token_output;
use std::time::Duration;
use distill_shell_base::util::subprocess::CommandLog;
use distill_shell_base::util::subprocess::RunError;
use distill_shell_base::util::subprocess::RunOptions;
use distill_shell_base::util::subprocess::run_detached_with_timeout;
use distill_shell_base::util::subprocess::shell_c;

/// Parse stdout into a session-credential `GrokAuth`.
pub fn parse_output(output: &std::process::Output) -> anyhow::Result<GrokAuth> {
    let parsed = parse_token_output(output)?;
    Ok(GrokAuth {
        key: parsed.access_token,
        auth_mode: AuthMode::External,
        create_time: chrono::Utc::now(),
        user_id: String::new(),
        email: None,
        first_name: None,
        last_name: None,
        profile_image_asset_id: None,
        principal_type: None,
        principal_id: None,
        team_id: None,
        team_name: None,
        team_role: None,
        organization_id: None,
        organization_name: None,
        organization_role: None,
        user_blocked_reason: None,
        team_blocked_reasons: vec![],
        coding_data_retention_opt_out: crate::default_coding_data_retention_opt_out(),
        has_grok_code_access: None,
        refresh_token: parsed.refresh_token,
        expires_at: parsed.expires_at,
        oidc_issuer: parsed.issuer,
        oidc_client_id: None,
    })
}

/// Short timeout for a mid-session refresh: it must not hang the session.
/// One run in `ExternalBinaryRefresher` gets this whole budget.
const EXTERNAL_AUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(7);

/// Why a headless refresh run produced no token. The refresher's verdict hangs on this split: a timeout is the contract's "needs interactive sign-in" signal, while every other failure (spawn error, non-zero exit, bad output) proves nothing about the credential and stays retryable.
#[derive(Debug, thiserror::Error)]
pub enum ExternalRefreshError {
    /// The run outlived `EXTERNAL_AUTH_REFRESH_TIMEOUT` — the documented
    /// shape of a provider blocking on interactive sign-in.
    #[error("external auth provider timed out")]
    TimedOut,
    /// The run failed without timing out; the message is diagnostic only.
    #[error("{0}")]
    Failed(String),
}

/// Runs the external auth binary for a headless mid-session refresh.
/// Initial, interactive sign-in takes a separate path (`flow::run_external_auth_provider`, which bridges the provider's stderr link).
/// This handles refresh only.
pub async fn run_external_refresh(command: &str) -> Result<GrokAuth, ExternalRefreshError> {
    tracing::info!(cmd = %command, timeout_secs = EXTERNAL_AUTH_REFRESH_TIMEOUT.as_secs(), "auth: running external auth provider (headless refresh)");

    let mut cmd = shell_c(command);
    cmd.env("GROK_AUTH_EXPIRED", "1");
    // Route through the group-killing runner so a provider that spawns helpers is torn down as a unit on timeout
    let output = match run_detached_with_timeout(
        cmd,
        EXTERNAL_AUTH_REFRESH_TIMEOUT,
        RunOptions {
            label: "external auth provider",
            command_log: CommandLog::Shown(command),
        },
    )
    .await
    {
        Ok(output) => output,
        Err(RunError::TimedOut) => {
            tracing::warn!(cmd = %command, "auth: external auth provider timed out (a timeout usually means it needs interactive sign-in)");
            return Err(ExternalRefreshError::TimedOut);
        }
        Err(RunError::SpawnFailed) => {
            tracing::warn!(cmd = %command, "auth: external auth provider failed to start");
            return Err(ExternalRefreshError::Failed(
                "external auth provider failed to start".into(),
            ));
        }
        Err(RunError::WaitFailed) => {
            tracing::warn!(cmd = %command, "auth: external auth provider errored while running");
            return Err(ExternalRefreshError::Failed(
                "external auth provider errored while running".into(),
            ));
        }
    };

    match parse_output(&output) {
        Ok(auth) => {
            tracing::info!("auth: external auth provider returned fresh token");
            Ok(auth)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth: external auth provider failed");
            Err(ExternalRefreshError::Failed(format!(
                "external auth provider {e:#}"
            )))
        }
    }
}

/// Run external auth provider, carrying forward `/user`-derived fields from previous auth.
pub async fn refresh_with_command(
    command: &str,
    prev_auth: &GrokAuth,
) -> Result<GrokAuth, ExternalRefreshError> {
    let mut auth = run_external_refresh(command).await?;
    auth.carry_user_profile_from(prev_auth);
    Ok(auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_nonzero_exit_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("false").status().unwrap(),
            stdout: b"token".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn parse_output_empty_stdout_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"  \n".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn parse_output_issuer_claim_enables_xai_auth() {
        let ok = |stdout: &str| std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        };

        // An x.ai issuer claim yields a first-party session (relay-eligible)
        let auth = parse_output(&ok(
            r#"{"access_token":"t","expires_in":900,"issuer":"https://auth.x.ai"}"#,
        ))
        .unwrap();
        assert_eq!(auth.oidc_issuer.as_deref(), Some("https://auth.x.ai"));
        assert!(auth.is_xai_auth());

        // Non-x.ai issuer is stored but stays third-party.
        let auth = parse_output(&ok(
            r#"{"access_token":"t","issuer":"https://idp.acme.example"}"#,
        ))
        .unwrap();
        assert_eq!(
            auth.oidc_issuer.as_deref(),
            Some("https://idp.acme.example")
        );
        assert!(!auth.is_xai_auth());

        // A missing, empty, or whitespace issuer stores None
        let auth = parse_output(&ok(r#"{"access_token":"t"}"#)).unwrap();
        assert_eq!(auth.oidc_issuer, None);
        assert!(!auth.is_xai_auth());
        let auth = parse_output(&ok(r#"{"access_token":"t","issuer":"  "}"#)).unwrap();
        assert_eq!(auth.oidc_issuer, None);

        // Bare-token output never carries an issuer.
        let auth = parse_output(&ok("bare-token")).unwrap();
        assert_eq!(auth.oidc_issuer, None);
        assert!(!auth.is_xai_auth());
    }

    #[test]
    fn parse_output_json_shaped_but_invalid_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"{not valid json}".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[tokio::test]
    async fn spawn_failure_is_a_non_timeout_failure() {
        // `sh -c` on a missing path is a fast non-zero exit, not a hang, so it
        // must classify as `Failed` (retryable), never `TimedOut`.
        match run_external_refresh("/nonexistent/binary").await {
            Err(ExternalRefreshError::Failed(_)) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sets_grok_auth_expired_env_on_refresh() {
        let auth = run_external_refresh("echo $GROK_AUTH_EXPIRED")
            .await
            .unwrap();
        assert_eq!(auth.key, "1");
    }

    #[tokio::test]
    async fn refresh_carries_zdr_flags_forward() {
        let prev = GrokAuth {
            user_blocked_reason: Some("BLOCKED_REASON_OTHER".into()),
            team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
            coding_data_retention_opt_out: true,
            organization_id: Some("org-1".into()),
            ..GrokAuth::test_default()
        };
        let auth = refresh_with_command("echo fresh-token", &prev)
            .await
            .unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "ZDR flag must survive refresh");
        assert!(auth.coding_data_retention_opt_out);
        assert_eq!(
            auth.user_blocked_reason.as_deref(),
            Some("BLOCKED_REASON_OTHER")
        );
        assert_eq!(auth.user_id, "test-user", "profile must survive refresh");
        assert_eq!(auth.organization_id.as_deref(), Some("org-1"));
    }

    #[tokio::test]
    async fn refresh_interactive_times_out() {
        // Binary writes a link to stderr then blocks; the 5s refresh timeout kills it.
        let cmd = r#"echo 'Visit http://example.com/auth' >&2; sleep 20; echo token"#;
        let start = std::time::Instant::now();
        let result = run_external_refresh(cmd).await;
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(ExternalRefreshError::TimedOut)),
            "should time out with the interactive-required classification, got {result:?}"
        );
        assert!(
            elapsed.as_secs() < 10,
            "refresh should use 5s timeout, not 60s (took {}s)",
            elapsed.as_secs()
        );
    }
}
