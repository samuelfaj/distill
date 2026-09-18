pub const FIRST_PARTY_CREDENTIAL_ENV_VARS: &[&str] = &[
    "GROK_AUTH",
    "GROK_AUTH_PATH",
    "XAI_API_KEY",
    "GROK_DEPLOYMENT_KEY",
    "GROK_CODE_XAI_API_KEY",
    "GROK_EXTRA_AUTH_KEY",
    "GROK_TRACE_UPLOAD_CREDENTIALS_FILE",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "GROK_INTERNAL_OTLP_HEADERS",
    // Jev/TypeSafe bearer token (plan §1.2 item B): scrubbed from child/hook
    // environments by default. NOTE: this list only covers the BYOK login
    // helper; the default shell environment policy is what keeps it away from
    // agent-run commands, and that is tracked in the plan's risk R-005.
    "JEV_API_KEY",
];

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

pub fn env_bool(name: &str) -> Option<bool> {
    parse_bool(&std::env::var(name).ok()?)
}

pub fn env_string(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvVarGuard;

    #[test]
    fn parse_bool_reads_known_spellings() {
        for on in ["1", "true", "YES", "On", "enabled"] {
            assert_eq!(parse_bool(on), Some(true), "{on}");
        }
        for off in ["0", "false", "NO", "Off", "disabled"] {
            assert_eq!(parse_bool(off), Some(false), "{off}");
        }
        for none in ["", "  ", "maybe", "2"] {
            assert_eq!(parse_bool(none), None, "{none:?}");
        }
    }

    #[test]
    fn env_string_trims_and_treats_blank_as_unset() {
        let guard = EnvVarGuard::set("GROK_TEST_ENV_STRING", "  hi  ");
        assert_eq!(env_string("GROK_TEST_ENV_STRING"), Some("hi".to_string()));
        guard.set_value("   ");
        assert_eq!(env_string("GROK_TEST_ENV_STRING"), None);
    }
}
