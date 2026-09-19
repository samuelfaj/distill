//! What the provider surfaces report, read from the same sources the lanes
//! themselves use, so the menu cannot disagree with the runtime.
//!
//! None of these performs a sign-in the harness does not own: the Codex
//! subscription belongs to the Codex CLI, the OpenRouter key belongs to the
//! environment, and the cheap lane belongs to `[jev.local]` in the config. Each
//! reports the live state and the exact next step — reporting a wrong state would
//! be worse than reporting nothing.

/// What the Codex subscription looks like right now.
pub fn codex_status() -> String {
    let path = xai_grok_shell::codex_auth::auth_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.codex/auth.json".to_owned());
    match xai_grok_shell::codex_auth::read_codex_auth() {
        Some(auth) => {
            let account = auth
                .account_id
                .as_deref()
                .map(|id| format!(" for account {id}"))
                .unwrap_or_default();
            format!(
                "Codex: signed in{account}.\n\n\
                 The harness uses the Codex CLI's own sign-in ({path}) — no second login.\n\
                 To switch account, run `codex login` and reopen this screen.\n\
                 Models that run on it are the ones pointed at the Codex backend."
            )
        }
        None => format!(
            "Codex: not signed in.\n\n\
             The harness reads the Codex CLI's credentials, so the sign-in happens there:\n\
             1. run `codex login` in a terminal;\n\
             2. come back and pick a Codex model (or open this screen again).\n\n\
             Expected file: {path}"
        ),
    }
}

/// What the OpenRouter key and the cheap lane it feeds look like right now.
pub fn openrouter_status() -> String {
    let key_present = std::env::var("OPENROUTER_API_KEY")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let key_line = if key_present {
        "OpenRouter key: present in OPENROUTER_API_KEY.".to_owned()
    } else {
        "OpenRouter key: not set.\n\
         Put it in the environment (never in the repo):\n  \
         export OPENROUTER_API_KEY=sk-or-v1-…\n\
         Add it to your shell profile so every session has it."
            .to_owned()
    };
    format!("{key_line}\n\n{}", cheap_lane_status())
}

/// The cheap lane's model, its notes, and what else could serve it.
pub fn cheap_lane_status() -> String {
    let local = xai_grok_shell::jev::local_config_cached();
    let mut out = match local
        .model
        .as_deref()
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
    {
        // A comma is a priority chain, not a list of options: say the order.
        Some(spec) if spec.contains(',') => format!(
            "Cheap lane model: {} (each one is tried after the previous fails)",
            spec.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .collect::<Vec<_>>()
                .join(" → "),
        ),
        Some(spec) => format!("Cheap lane model: {spec}"),
        None => format!(
            "Cheap lane model: (unset — the shipped chain is used: {})",
            xai_grok_shell::jev_cheap::default_model_spec().replace(',', " → "),
        ),
    };
    if let Some(notes) = local.notes.as_deref().filter(|notes| !notes.trim().is_empty()) {
        out.push_str(&format!("\n  {notes}"));
    }
    let candidates = openrouter_entries();
    if candidates.is_empty() {
        out.push_str(
            "\n\nNo OpenRouter model entry is configured. Add one to ~/.grok/config.toml:\n  \
             [model.openrouter-qwen37]\n  model = \"qwen/qwen3.7-flash\"\n  \
             base_url = \"https://openrouter.ai/api/v1\"\n  env_key = \"OPENROUTER_API_KEY\"",
        );
    } else {
        out.push_str("\n\nOpenRouter entries available for the cheap lane:");
        for (key, model) in &candidates {
            out.push_str(&format!("\n  - {key} ({model})"));
        }
    }
    out.push_str(
        "\n\nSet it with `/cheap-model <entry>` or `/cheap-model <id>,<id>,<id>` \
         (writes `[jev.local] model` in ~/.grok/config.toml; applies next session). \
         A comma-separated list is a fallback chain, tried in order. \
         `/cheap-model clear` goes back to the shipped chain.",
    );
    out
}

/// The configured model entries that point at OpenRouter, as `(key, model)`.
///
/// Read from the same user config the session resolves, so the list cannot
/// disagree with what `[jev.local]` could actually route to. The key is what
/// `[jev.local] model` holds, so it is also what `/cheap-model` accepts.
pub fn openrouter_entries() -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let Ok(layers) = xai_grok_shell::config::ConfigLayers::load() else {
        return entries;
    };
    let merged = layers.effective_config_base_without_overlay();
    let Some(models) = merged.get("model").and_then(toml::Value::as_table) else {
        return entries;
    };
    for (key, entry) in models {
        let url = entry
            .get("base_url")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        if !url.contains("openrouter.ai") {
            continue;
        }
        let model = entry
            .get("model")
            .and_then(toml::Value::as_str)
            .unwrap_or("(no model)");
        entries.push((key.clone(), model.to_owned()));
    }
    entries.sort();
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codex_status_says_which_file_it_reads_and_what_to_do() {
        let status = codex_status();
        assert!(status.starts_with("Codex:"), "{status}");
        assert!(status.contains("codex login"), "the fix is named: {status}");
        assert!(status.contains("auth.json"), "{status}");
        // No token material, ever: the status reports the account, not the token.
        assert!(!status.contains("eyJ"), "no token material: {status}");
    }

    #[test]
    fn the_openrouter_status_reports_the_key_without_ever_showing_one() {
        let status = openrouter_status();
        assert!(status.starts_with("OpenRouter key:"), "{status}");
        assert!(status.contains("Cheap lane model:"), "{status}");
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY")
            && !key.trim().is_empty()
        {
            assert!(!status.contains(&key), "the key value must not be echoed");
            assert!(status.contains("present"), "{status}");
        }
    }

    #[test]
    fn the_cheap_lane_status_names_the_model_the_candidates_and_the_knob() {
        let status = cheap_lane_status();
        assert!(status.contains("Cheap lane model:"), "{status}");
        assert!(status.contains("jev.local"), "the knob is named: {status}");
        assert!(status.contains("cheap-model"), "and the command: {status}");
        // A candidate is an OpenRouter entry or nothing: another provider's entry
        // would be a wrong answer to "which cheap model".
        for (key, _) in openrouter_entries() {
            assert!(key.contains("openrouter"), "{key}");
        }
    }
}
