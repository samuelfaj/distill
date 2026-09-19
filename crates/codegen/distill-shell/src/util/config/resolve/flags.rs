// Modified for Distill by Samuel Fajreldines, 2026.
//! Standalone flag resolvers with no `[toolset.*]` key.

const ENV_UNCHARGED_401_PARK: &str = "GROK_UNCHARGED_401_PARK";

/// Credential-less-401 park kill switch: remote `uncharged_401_park` > default `true`;
/// `GROK_UNCHARGED_401_PARK=0` forces off. Env (and any config/pin tier — hence not in
/// `FEATURES`) must never mask a fleet-wide remote kill.
pub fn resolve_uncharged_401_park(remote: Option<bool>) -> bool {
    resolve_uncharged_401_park_tiers(distill_config::env_bool(ENV_UNCHARGED_401_PARK), remote)
}

fn resolve_uncharged_401_park_tiers(env: Option<bool>, remote: Option<bool>) -> bool {
    if env == Some(false) {
        return false;
    }
    remote.unwrap_or(true)
}

#[cfg(test)]
mod uncharged_401_park_tests {
    use super::resolve_uncharged_401_park_tiers;

    #[test]
    fn remote_false_kills_absent_defaults_on_env_forces_off_only() {
        assert!(
            resolve_uncharged_401_park_tiers(None, None),
            "absent must default on"
        );
        assert!(resolve_uncharged_401_park_tiers(None, Some(true)));
        assert!(
            !resolve_uncharged_401_park_tiers(None, Some(false)),
            "remote false is the kill switch"
        );
        assert!(
            !resolve_uncharged_401_park_tiers(Some(false), Some(true)),
            "env off-path override beats remote"
        );
        assert!(
            !resolve_uncharged_401_park_tiers(Some(true), Some(false)),
            "env on must not mask a fleet-wide remote kill"
        );
        assert!(
            resolve_uncharged_401_park_tiers(Some(true), None),
            "env on is a no-op, not an enable tier"
        );
    }
}
