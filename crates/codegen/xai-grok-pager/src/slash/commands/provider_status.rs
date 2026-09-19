//! What the provider surfaces report, read from the same sources the lanes
//! themselves use, so the menu cannot disagree with the runtime.
//!
//! The ChatGPT sign-in is the harness's own (`remote-code login --chatgpt`, the
//! OAuth this fork carries); the OpenRouter key belongs to the environment, the
//! Grok sign-in to the login flow, and the cheap lane to `[jev.local]`. Each
//! reports the live state and the exact next step — reporting a wrong state
//! would be worse than reporting nothing.

/// What the ChatGPT (Codex) sign-in looks like right now.
pub fn codex_status() -> String {
    let path = xai_grok_shell::codex_auth::auth_file_path();
    let shown = path.display();
    match xai_grok_shell::codex_auth::load_credentials() {
        Ok(Some(credentials)) => {
            let label = credentials
                .email
                .clone()
                .or_else(|| credentials.account_id.clone())
                .unwrap_or_else(|| "this account".to_owned());
            format!(
                "ChatGPT: signed in as {label}.\n\n\
                 Credentials: {shown} — this harness's own OAuth, refreshed by the harness.\n\
                 Models: any `[model.<id>]` entry pointing at the Codex backend, e.g.\n  \
                 [model.chatgpt]\n  model = \"gpt-5.6-sol\"\n  base_url = \"https://chatgpt.com/backend-api/codex\"\n  \
                 api_backend = \"responses\"\n\
                 Sign out with `remote-code logout --chatgpt`."
            )
        }
        Ok(None) => format!(
            "ChatGPT: not signed in.\n\n\
             The harness runs its own OAuth, so the sign-in happens here:\n  \
             `remote-code login --chatgpt`\n\
             It opens the browser (or prints a code to enter), and writes {shown}.\n\
             Until then, ChatGPT models stay unavailable and everything else works."
        ),
        Err(error) => format!(
            "ChatGPT: the credential file could not be read ({error}).\n\
             Expected at {shown}. Sign in again with `remote-code login --chatgpt`."
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

/// The three tiers, in the order a call falls through them: the session's model,
/// its lighter sibling, then the cheap lanes.
///
/// `hard_model` is the session's own model id, which only the caller knows. The
/// sibling's usability comes from the shell, which is where the rule is enforced
/// — a tier the harness would refuse must not read as ready here.
pub fn tier_status(hard_model: Option<&str>) -> String {
    let hard = hard_model.unwrap_or("(no session model yet)");
    let mut out = format!("Hard model (the session's): {hard}");
    out.push_str("\n  Set it with `/model <name>` — the harness's own model picker.");
    match hard_model.map(xai_grok_shell::jev::light_tier_status) {
        Some(xai_grok_shell::jev::LightTierStatus::Ready { name, window, .. }) => {
            out.push_str(&format!(
                "\n\nLight model: {name} ({window} tokens of context)\n  \
                 Jev may route a single model call here when the step does not need \
                 the hard model. Same provider, same backend, same credential, same \
                 conversation."
            ));
        }
        Some(xai_grok_shell::jev::LightTierStatus::Refused(reason)) => {
            out.push_str(&format!(
                "\n\nLight model: refused — {reason}\n  \
                 The tier stays off for this pairing."
            ));
        }
        Some(xai_grok_shell::jev::LightTierStatus::Unset) | None => {
            out.push_str(
                "\n\nLight model: (none)\n  \
                 Set it with `/tiers light <model-id>`: the session model's lighter sibling, \
                 same provider and same conversation. A provider with a single model has none.",
            );
        }
    }
    out.push_str("\n\n");
    out.push_str(&cheap_lane_status());
    out.push_str("\n\nSet with `/tiers hard <name>`, `/tiers light <id|clear>`, `/tiers cheap <ids|clear>`.");
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

    /// The three tiers are read in the order a call falls through them, and a
    /// tier the shell refuses must not read as ready.
    #[test]
    fn the_tier_status_names_all_three_in_fall_through_order() {
        let text = tier_status(Some("grok-4.6"));
        let hard = text.find("Hard model").expect("hard tier named");
        let light = text.find("Light model").expect("light tier named");
        let cheap = text.find("Cheap lane model:").expect("cheap tier named");
        assert!(hard < light && light < cheap, "{text}");
        assert!(text.contains("grok-4.6"), "{text}");
        // Whatever the machine's config says, the three surfaces agree with the
        // shell: a refused tier carries its reason instead of a model name.
        if let Ok(light_id) = std::env::var("PROBE_TIER_LIGHT")
            && !light_id.trim().is_empty()
        {
            assert!(
                text.contains("refused") || text.contains(&light_id),
                "{text}"
            );
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
