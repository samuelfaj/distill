use super::*;
use pretty_assertions::assert_eq;

#[test]
fn from_env_defaults_clamps_and_halves_the_soft_notice() {
    let cases = [
        (None, DEFAULT_PROMPT_ACK_TIMEOUT, PROMPT_ACK_SOFT_NOTICE),
        (
            Some("0"),
            DEFAULT_PROMPT_ACK_TIMEOUT,
            PROMPT_ACK_SOFT_NOTICE,
        ),
        (
            Some("garbage"),
            DEFAULT_PROMPT_ACK_TIMEOUT,
            PROMPT_ACK_SOFT_NOTICE,
        ),
        // Below the floor: clamped, and the notice halves so it still precedes the deadline
        (
            Some("1"),
            Duration::from_secs(5),
            Duration::from_millis(2_500),
        ),
        (
            Some(" 120 "),
            Duration::from_secs(120),
            PROMPT_ACK_SOFT_NOTICE,
        ),
        (
            Some("99999999"),
            Duration::from_secs(MAX_PROMPT_ACK_TIMEOUT_SECS),
            PROMPT_ACK_SOFT_NOTICE,
        ),
    ];
    for (env, hard, soft) in cases {
        assert_eq!(
            PromptAckDeadlines { soft, hard },
            PromptAckDeadlines::from_env(env),
            "env {env:?}"
        );
    }
}

#[test]
fn poll_advances_armed_to_soft_notice_to_expired() {
    let deadlines = PromptAckDeadlines {
        soft: Duration::from_secs(10),
        hard: Duration::from_secs(60),
    };
    let t0 = Instant::now();
    let mut watch = PromptAckWatch::new("p1", t0);
    let outcomes =
        [9, 10, 30, 60].map(|secs| watch.poll(t0 + Duration::from_secs(secs), &deadlines));
    assert_eq!(
        [
            PromptAckOutcome::Waiting,
            PromptAckOutcome::SoftNotice {
                waited: Duration::from_secs(10)
            },
            PromptAckOutcome::Waiting,
            PromptAckOutcome::Expired {
                waited: Duration::from_secs(60)
            },
        ],
        outcomes,
        "the notice fires once; expiry reports on every poll past the deadline"
    );
    assert_eq!(
        t0 + Duration::from_secs(60),
        watch.hard_deadline(&deadlines)
    );
}
