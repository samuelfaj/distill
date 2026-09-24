// Modified for Distill by Samuel Fajreldines, 2026.
//! What the provider surfaces report, read from the same sources the lanes
//! themselves use, so the menu cannot disagree with the runtime.
//!
//! The ChatGPT sign-in is the harness's own (`Distill login --chatgpt`, the
//! OAuth this fork carries); OpenRouter uses its saved key or the environment, the
//! Grok sign-in to the login flow, and the utility model to `[jev.local]`. Each
//! reports the live state and the exact next step — reporting a wrong state
//! would be worse than reporting nothing.

/// What the ChatGPT (Codex) sign-in looks like right now.
pub fn codex_status() -> String {
    let path = distill_shell::codex_auth::auth_file_path();
    let shown = path.display();
    match distill_shell::codex_auth::load_credentials() {
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
                 Sign out with `Distill logout --chatgpt`."
            )
        }
        Ok(None) => format!(
            "ChatGPT: not signed in.\n\n\
             The harness runs its own OAuth, so the sign-in happens here:\n  \
             `Distill login --chatgpt`\n\
             It opens the browser (or prints a code to enter), and writes {shown}.\n\
             Until then, ChatGPT models stay unavailable and everything else works."
        ),
        Err(error) => format!(
            "ChatGPT: the credential file could not be read ({error}).\n\
             Expected at {shown}. Sign in again with `Distill login --chatgpt`."
        ),
    }
}

/// What the OpenRouter key and the utility model it feeds look like right now.
pub fn openrouter_status() -> String {
    let key_line = match distill_shell::openrouter_auth::api_key() {
        Ok(Some(_)) => "OpenRouter key: present. Ready to use.".to_owned(),
        Ok(None) => "OpenRouter key: not set. Select Log in with OpenRouter or run /login-openrouter to connect in your browser. OPENROUTER_API_KEY is also supported.".to_owned(),
        Err(error) => format!("OpenRouter key: could not read saved credentials ({error}). Run /login-openrouter to sign in again."),
    };
    format!("{key_line}\n\n{}", cheap_lane_status())
}

/// The utility model, its notes, and what else could serve it.
pub fn cheap_lane_status() -> String {
    let local = distill_shell::jev::local_config_cached();
    let mut out = match local
        .model
        .as_deref()
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
    {
        // A comma is a priority chain, not a list of options: say the order.
        Some(spec) if spec.contains(',') => format!(
            "Utility model: {} (each one is tried after the previous fails)",
            spec.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .collect::<Vec<_>>()
                .join(", then "),
        ),
        Some(spec) => format!("Utility model: {spec}"),
        None => format!(
            "Utility model: (default chain: {})",
            distill_shell::jev_cheap::default_model_spec().replace(',', ", "),
        ),
    };
    if let Some(notes) = local
        .notes
        .as_deref()
        .filter(|notes| !notes.trim().is_empty())
    {
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
        out.push_str("\n\nOpenRouter entries available for the utility model:");
        for (key, model) in &candidates {
            out.push_str(&format!("\n  - {key} ({model})"));
        }
    }
    out.push_str(
        "\n\nSaved in `[jev.local] model`. Set it with `/utility-model <entry>` or `/utility-model <id>,<id>,<id>`. \
         A comma-separated list is a fallback chain, tried in order. \
         `/utility-model clear` goes back to the default chain.",
    );
    out
}

/// The main, reasoning and utility models.
///
/// `session_model` is the session's own model id, which only the caller knows:
/// it is the main model, which runs every step.
pub fn tier_status(session_model: Option<&str>) -> String {
    let main = session_model.unwrap_or("(no session model yet)");
    let mut out = format!("Main model: {main}");
    out.push_str(
        "\n  Required. Runs every session and every step. Set it with `/model <name-or-id>`. \
         A configured OpenRouter model can also be entered as `vendor/model`.",
    );
    match crate::acp::ModelState::configured_reasoning_model() {
        Some(reasoning) => out.push_str(&format!(
            "\n\nReasoning model: {}\n  Optional. The main model consults it to plan or \
             review a step it cannot do alone. Set it with `/reasoning-model <name-or-id>`; \
             `/reasoning-model clear` removes it.",
            reasoning.0
        )),
        None => out.push_str(
            "\n\nReasoning model: (not set)\n  Optional. Without it the main model works alone. \
             Set it with `/reasoning-model <name-or-id>`.",
        ),
    }
    out.push_str("\n\n");
    out.push_str(&cheap_lane_status());
    out.push_str(
        "\n  Handles short, repetitive, or fallback work at lower cost. Set it with \
         `/utility-model <entry>` or `/utility-model <id>,<id>`.\n\n\
         You can edit all three fields in the Model tiers screen, or use `/tiers main`, \
         `/tiers reasoning`, and `/tiers utility`.",
    );
    out
}

/// The configured model entries that point at OpenRouter, as `(key, model)`.
///
/// Read from the same user config the session resolves, so the list cannot
/// disagree with what `[jev.local]` could actually route to. The key is what
/// `[jev.local] model` holds, so it is also what `/utility-model` accepts.
pub fn openrouter_entries() -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let Ok(layers) = distill_shell::config::ConfigLayers::load() else {
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
        assert!(status.starts_with("ChatGPT:"), "{status}");
        assert!(
            status.contains("Distill login --chatgpt")
                || status.contains("Distill logout --chatgpt"),
            "{status}"
        );
        assert!(status.contains("auth.json"), "{status}");
        // No token material, ever: the status reports the account, not the token.
        assert!(!status.contains("eyJ"), "no token material: {status}");
    }

    #[test]
    fn the_openrouter_status_reports_the_key_without_ever_showing_one() {
        let status = openrouter_status();
        assert!(status.starts_with("OpenRouter key:"), "{status}");
        assert!(status.contains("Utility model:"), "{status}");
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY")
            && !key.trim().is_empty()
        {
            assert!(!status.contains(&key), "the key value must not be echoed");
            assert!(status.contains("present"), "{status}");
        }
    }

    /// The status names the main model first, then the optional reasoning and
    /// the utility models, each with the command that sets it.
    #[test]
    fn the_tier_status_names_main_reasoning_and_utility_models() {
        let text = tier_status(Some("grok-4.6"));
        let main = text.find("Main model: grok-4.6").expect("main model named");
        let reasoning = text.find("Reasoning model:").expect("reasoning tier named");
        let utility = text.find("Utility model:").expect("utility tier named");
        assert!(main < reasoning && reasoning < utility, "{text}");
        assert!(text.contains("/model") && text.contains("/reasoning-model"), "{text}");
    }

    #[test]
    fn the_cheap_lane_status_names_the_model_the_candidates_and_the_knob() {
        let status = cheap_lane_status();
        assert!(status.contains("Utility model:"), "{status}");
        assert!(status.contains("jev.local"), "the knob is named: {status}");
        assert!(
            status.contains("utility-model"),
            "and the command: {status}"
        );
        // A candidate is an OpenRouter entry or nothing: another provider's entry
        // would be a wrong answer to "which cheap model".
        for (key, _) in openrouter_entries() {
            assert!(key.contains("openrouter"), "{key}");
        }
    }
}
