// Modified for Distill by Samuel Fajreldines, 2026.
//! Key-gated proof that the requests the sampler builds turn into real provider cache reads.
//! The mapping tests show each request extends the one before it byte for byte; only a live API shows that the
//! provider then serves that prefix from cache, which is what every cache lever here is for.
//!
//! Every test is `#[ignore]`d (it spends real tokens) and skips with a message when its credential is missing:
//! `cargo test -p distill-shell --lib real_api_cache -- --ignored --nocapture`.
//! - Anthropic: the Claude subscription login in the Distill home, read once and never refreshed or written (the
//!   bearer goes in as a fixed key, not through the resolver that renews the stored token before each send). An
//!   expired token skips; any Distill run refreshes it.
//! - OpenRouter: `OPENROUTER_API_KEY`.
//! - ChatGPT: the ChatGPT login in the Distill home, read per request and never refreshed.
//!
//! `DISTILL_CACHE_TEST_{ANTHROPIC,OPENROUTER,CHATGPT}_MODEL` pick another model. Each system prompt is padded just past
//! the model's minimum cacheable length and output is capped, so a run costs about a cent on OpenRouter.

use serde_json::json;

use distill_sampler::config::AuthScheme;
use distill_sampler::{ApiBackend, SamplerConfig, SamplingClient};
use distill_sampling_types::{
    ConversationItem, ConversationRequest, ConversationResponse, MessagesCacheOptions, TokenUsage,
    ToolSpec, build_messages_request_with, supports_system_messages, supports_tool_changes,
};

/// A provider caches in blocks (OpenAI's are 128 tokens) and may leave a short uncached suffix of an unchanged prefix.
const CACHE_TAIL_TOKENS: u32 = 256;
/// The tool loop runs at least this many requests, so at least two of them must read their predecessor.
const MIN_REQUESTS: usize = 3;
const MAX_REQUESTS: usize = 6;
const MAX_OUTPUT_TOKENS: u32 = 256;
/// What the lookup tool returns for every key; the final answer must repeat it.
const LOOKUP_VALUE: &str = "falcon-42";
const CONVERT_VALUE: &str = "39.37 feet";

struct Provider {
    name: &'static str,
    client: SamplingClient,
    model: String,
    /// The shortest prefix the provider caches for `model`; system prompts are padded just past it.
    min_cacheable_tokens: usize,
}

fn skip(provider: &str, why: &str) {
    eprintln!("skipping the {provider} prompt-cache test: {why}");
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Anthropic's minimum cacheable prompt per model (the prompt caching docs). An unknown model gets 2,048, which covers
/// every model but the 4,096-token ones.
fn anthropic_min_cacheable_tokens(model: &str) -> usize {
    let model = model.rsplit('/').next().unwrap_or(model).replace('.', "-");
    if ["haiku", "opus-4-5", "opus-4-6"]
        .iter()
        .any(|name| model.contains(*name))
    {
        4096
    } else if [
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
        "claude-sonnet-5-5",
    ]
    .iter()
    .any(|base| model.starts_with(*base))
    {
        512
    } else {
        2048
    }
}

/// The Claude subscription, as the shell sends it (`anthropic-version`, the subscription betas, and the Claude Code
/// identity the sampler prepends for them), with the stored bearer as a fixed key so nothing refreshes or rewrites it.
fn anthropic() -> Option<Provider> {
    let credentials = match crate::claude_auth::load_credentials() {
        Ok(Some(credentials)) => credentials,
        Ok(None) => {
            skip("Anthropic", "no Claude login (`distill login --claude`)");
            return None;
        }
        Err(error) => {
            skip(
                "Anthropic",
                &format!("cannot read the Claude login: {error}"),
            );
            return None;
        }
    };
    let model = env_or("DISTILL_CACHE_TEST_ANTHROPIC_MODEL", "claude-sonnet-5-5");
    let mut extra_headers = indexmap::IndexMap::new();
    extra_headers.insert(
        "anthropic-version".to_owned(),
        crate::claude_auth::ANTHROPIC_VERSION.to_owned(),
    );
    extra_headers.insert(
        "anthropic-beta".to_owned(),
        crate::claude_auth::subscription_beta_header(),
    );
    let config = SamplerConfig {
        api_key: Some(credentials.access_token),
        base_url: format!("https://{}/v1", crate::claude_auth::CLAUDE_INFERENCE_HOST),
        model: model.clone(),
        api_backend: ApiBackend::Messages,
        auth_scheme: AuthScheme::Bearer,
        max_completion_tokens: Some(MAX_OUTPUT_TOKENS),
        max_retries: Some(2),
        extra_headers,
        ..SamplerConfig::default()
    };
    Some(Provider {
        name: "Anthropic",
        client: SamplingClient::new(config).expect("Anthropic sampler config"),
        min_cacheable_tokens: anthropic_min_cacheable_tokens(&model),
        model,
    })
}

/// OpenRouter on Chat Completions, where `anthropic/*` and `google/gemini*` models get `cache_control` breakpoints and
/// every request carries the session routing key. The default, Claude Haiku 4.5, reports exact cache reads.
fn openrouter() -> Option<Provider> {
    let Some(api_key) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        skip("OpenRouter", "OPENROUTER_API_KEY is not set");
        return None;
    };
    let model = env_or(
        "DISTILL_CACHE_TEST_OPENROUTER_MODEL",
        "anthropic/claude-haiku-4.5",
    );
    let min_cacheable_tokens = if model.starts_with("anthropic/") {
        anthropic_min_cacheable_tokens(&model)
    } else {
        2048
    };
    let config = SamplerConfig {
        api_key: Some(api_key),
        base_url: "https://openrouter.ai/api/v1".to_owned(),
        model: model.clone(),
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        max_completion_tokens: Some(MAX_OUTPUT_TOKENS),
        max_retries: Some(2),
        ..SamplerConfig::default()
    };
    Some(Provider {
        name: "OpenRouter",
        client: SamplingClient::new(config).expect("OpenRouter sampler config"),
        model,
        min_cacheable_tokens,
    })
}

/// The ChatGPT subscription on the Codex Responses endpoint, configured by the shell's own [`apply_codex_backend`],
/// whose resolver reads the stored token per request and never refreshes it.
///
/// [`apply_codex_backend`]: crate::codex_auth::apply_codex_backend
fn chatgpt() -> Option<Provider> {
    match crate::codex_auth::load_credentials() {
        Ok(Some(_)) => {}
        Ok(None) => {
            skip("ChatGPT", "no ChatGPT login (`distill login --chatgpt`)");
            return None;
        }
        Err(error) => {
            skip(
                "ChatGPT",
                &format!("cannot read the ChatGPT login: {error}"),
            );
            return None;
        }
    }
    let model = env_or("DISTILL_CACHE_TEST_CHATGPT_MODEL", "gpt-6-luna");
    let mut config = SamplerConfig {
        base_url: crate::codex_auth::inference_base_url(),
        model: model.clone(),
        max_completion_tokens: Some(MAX_OUTPUT_TOKENS),
        max_retries: Some(2),
        ..SamplerConfig::default()
    };
    crate::codex_auth::apply_codex_backend(&mut config);
    Some(Provider {
        name: "ChatGPT",
        client: SamplingClient::new(config).expect("ChatGPT sampler config"),
        model,
        // OpenAI caches prompts of 1,024 tokens and more.
        min_cacheable_tokens: 1024,
    })
}

/// `instructions` followed by reference records the tests never ask about, about `tokens` of them: a stand-in for the
/// long, stable system prompt a real session sends, sized just past the provider's minimum cacheable prefix.
fn padded_system_prompt(instructions: &str, min_cacheable_tokens: usize) -> String {
    // A record line is about 18 tokens; 16 keeps the estimate on the long side.
    let lines = (min_cacheable_tokens + min_cacheable_tokens / 4) / 16 + 1;
    let colors = ["amber", "cobalt", "olive", "scarlet", "ivory", "teal"];
    let mut prompt = format!("{instructions}\n\n<reference_records>\n");
    for i in 0..lines {
        let color = colors.get(i % colors.len()).copied().unwrap_or("grey");
        prompt.push_str(&format!(
            "record-{i:03}: shelf {i} holds the {color} binder for project kestrel-{i}.\n"
        ));
    }
    prompt.push_str("</reference_records>");
    prompt
}

fn lookup_tool() -> ToolSpec {
    ToolSpec {
        name: "lookup".to_owned(),
        description: Some("Look up the stored value for a key.".to_owned()),
        parameters: json!({
            "type": "object",
            "properties": {"key": {"type": "string", "description": "The key to look up."}},
            "required": ["key"],
            "additionalProperties": false
        }),
    }
}

fn convert_units_tool() -> ToolSpec {
    ToolSpec {
        name: "convert_units".to_owned(),
        description: Some("Convert a length from one unit to another.".to_owned()),
        parameters: json!({
            "type": "object",
            "properties": {
                "value": {"type": "number"},
                "from": {"type": "string"},
                "to": {"type": "string"}
            },
            "required": ["value", "from", "to"],
            "additionalProperties": false
        }),
    }
}

fn run_tool(name: &str, arguments: &str) -> String {
    match name {
        "lookup" => {
            let key = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|args| args.get("key")?.as_str().map(str::to_owned))
                .unwrap_or_default();
            format!("value({key}) = {key}-{LOOKUP_VALUE}")
        }
        "convert_units" => CONVERT_VALUE.to_owned(),
        other => format!("Tool `{other}` does not exist."),
    }
}

/// One conversation against a live provider: each step sends the whole history, as a session does, and keeps the usage.
struct Session<'a> {
    provider: &'a Provider,
    /// The routing key and conversation id of every request, as the main call of a session sends them.
    id: String,
    items: Vec<ConversationItem>,
    tools: Vec<ToolSpec>,
    deferred_tools: Vec<ToolSpec>,
    usages: Vec<TokenUsage>,
}

impl<'a> Session<'a> {
    fn new(provider: &'a Provider, system: String, tools: Vec<ToolSpec>) -> Self {
        Self {
            provider,
            id: format!("cache-e2e-{}", uuid::Uuid::new_v4()),
            items: vec![ConversationItem::system(system)],
            tools,
            deferred_tools: Vec::new(),
            usages: Vec::new(),
        }
    }

    fn request(&self) -> ConversationRequest {
        ConversationRequest {
            items: self.items.clone(),
            tools: self.tools.clone(),
            deferred_tools: self.deferred_tools.clone(),
            model: Some(self.provider.model.clone()),
            max_output_tokens: Some(MAX_OUTPUT_TOKENS),
            prompt_cache_key: Some(self.id.clone()),
            x_grok_conv_id: Some(self.id.clone()),
            x_grok_session_id: Some(self.id.clone()),
            ..Default::default()
        }
    }

    /// Sends the history, appends the response and a result for each tool call. `None` skips the test: the provider
    /// refused the stored credential on the first request (expired; this test never refreshes it).
    async fn step(&mut self) -> Option<ConversationResponse> {
        let name = self.provider.name;
        let number = self.usages.len() + 1;
        let response = match self
            .provider
            .client
            .conversation_collect(self.request())
            .await
        {
            Ok(response) => response,
            Err(error) if self.usages.is_empty() && error.is_auth_error() => {
                skip(
                    name,
                    &format!(
                        "the stored login was refused ({error}); run Distill once to refresh it"
                    ),
                );
                return None;
            }
            Err(error) => panic!("{name} request {number} failed: {error}"),
        };
        let usage = response
            .usage
            .clone()
            .unwrap_or_else(|| panic!("{name} request {number} reported no usage"));
        eprintln!(
            "{name} request {number}: prompt {} tokens, cache read {}, cache write {}",
            usage.prompt_tokens, usage.cached_prompt_tokens, usage.cache_creation_prompt_tokens
        );
        self.usages.push(usage);
        self.items.extend(response.items.iter().cloned());
        for call in response.tool_calls() {
            self.items.push(ConversationItem::tool_result(
                call.id.to_string(),
                run_tool(&call.name, &call.arguments),
            ));
        }
        Some(response)
    }

    /// The last request read everything the one before it sent from cache, less the provider's block tail.
    /// That is the whole point of keeping each request an extension of the last: anything less re-bills history.
    fn assert_last_reads_the_previous(&self, what: &str) {
        let name = self.provider.name;
        let (Some(current), Some(previous)) = (
            self.usages.last(),
            self.usages
                .len()
                .checked_sub(2)
                .and_then(|i| self.usages.get(i)),
        ) else {
            panic!("{name} {what}: needs two requests");
        };
        assert!(
            current.cached_prompt_tokens > 0,
            "{name} {what}: no cache read at all (prompt {} tokens)",
            current.prompt_tokens
        );
        assert!(
            current.cached_prompt_tokens + CACHE_TAIL_TOKENS >= previous.prompt_tokens,
            "{name} {what}: read {} cached tokens of the {} the previous request sent",
            current.cached_prompt_tokens,
            previous.prompt_tokens
        );
    }

    /// The opening request was long enough to be cached at all, so a later miss is the harness's fault, not padding.
    fn assert_first_is_cacheable(&self) {
        let name = self.provider.name;
        let first = self.usages.first().expect("a request was sent");
        assert!(
            first.prompt_tokens as usize >= self.provider.min_cacheable_tokens,
            "{name}: the first prompt ({} tokens) is under the {} the model caches; raise the padding",
            first.prompt_tokens,
            self.provider.min_cacheable_tokens
        );
    }
}

/// The body the sampler sends to `api.anthropic.com` for `request` while the API takes system messages and deferred tools.
fn direct_messages_body(request: &ConversationRequest) -> serde_json::Value {
    let options = MessagesCacheOptions {
        anchor: true,
        extended_ttl: false,
        system_messages: true,
        deferred_tools: true,
    };
    serde_json::to_value(build_messages_request_with(request, options)).expect("serializable body")
}

const LOOKUP_INSTRUCTIONS: &str = "You are a terse assistant in an automated cache test. Follow instructions literally. \
When asked to look something up, call the lookup tool with the requested key and wait for its result before going on. \
Look up exactly one key per reply. Never invent a value the tool has not returned. When every requested key is looked \
up, answer in one short sentence that repeats each returned value verbatim. No markdown, no questions.";

/// A multi-step tool loop, the shape of most agent turns: every request after the first must read the whole of the
/// request before it from cache, or each round re-bills the conversation.
async fn tool_loop_reads_each_previous_request(provider: &Provider) {
    let system = padded_system_prompt(LOOKUP_INSTRUCTIONS, provider.min_cacheable_tokens);
    let mut session = Session::new(provider, system, vec![lookup_tool()]);
    session.items.push(ConversationItem::user(
        "Look up the keys alpha, bravo and charlie with the lookup tool, one key per reply, then tell me the values.",
    ));
    let mut more_keys = ["delta", "echo", "foxtrot"].into_iter();
    let answered = loop {
        let Some(response) = session.step().await else {
            return;
        };
        if session.usages.len() > 1 {
            session.assert_last_reads_the_previous(&format!("request {}", session.usages.len()));
        }
        let answered = response.tool_calls().is_empty();
        if session.usages.len() >= MAX_REQUESTS
            || (answered && session.usages.len() >= MIN_REQUESTS)
        {
            break answered;
        }
        if answered {
            // The model looked several keys up at once: one more lookup keeps the loop going.
            let key = more_keys.next().unwrap_or("golf");
            session.items.push(ConversationItem::user(format!(
                "Now look up the key {key} the same way and tell me its value."
            )));
        }
    };
    session.assert_first_is_cacheable();
    if answered {
        let text = session
            .items
            .iter()
            .rev()
            .find_map(|item| match item {
                ConversationItem::Assistant(a) => Some(a.content.to_string()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            text.contains(LOOKUP_VALUE),
            "{}: the tool results never reached the answer: {text:?}",
            provider.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Anthropic API with the Claude login; run with -- --ignored"]
async fn anthropic_tool_loop_reads_each_previous_request() {
    if let Some(provider) = anthropic() {
        tool_loop_reads_each_previous_request(&provider).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real OpenRouter API with OPENROUTER_API_KEY; run with -- --ignored"]
async fn openrouter_tool_loop_reads_each_previous_request() {
    if let Some(provider) = openrouter() {
        tool_loop_reads_each_previous_request(&provider).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real ChatGPT (Codex) API with the ChatGPT login; run with -- --ignored"]
async fn chatgpt_tool_loop_reads_each_previous_request() {
    if let Some(provider) = chatgpt() {
        tool_loop_reads_each_previous_request(&provider).await;
    }
}

const JOIN_INSTRUCTIONS: &str = "You are a terse assistant in an automated cache test. Follow instructions literally. \
Use the tools you are given when asked, one call per reply, and wait for each result. Never invent a value a tool has \
not returned. Answer in one short sentence that repeats the returned value verbatim. No markdown, no questions.";

/// A tool family that joins mid-session (`p1_tool_family`) is declared deferred from the first request and offered by a
/// `tool_addition` system message after the turn that needs it. The request with the join must still read the whole
/// conversation before it, and the model must be able to call the tool it was just offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Anthropic API with the Claude login; run with -- --ignored"]
async fn anthropic_tool_join_keeps_the_earlier_conversation_cached() {
    let Some(provider) = anthropic() else {
        return;
    };
    if !supports_tool_changes(&provider.model) {
        skip(
            "Anthropic tool join",
            &format!("{} takes no mid-conversation tool changes", provider.model),
        );
        return;
    }
    let system = padded_system_prompt(JOIN_INSTRUCTIONS, provider.min_cacheable_tokens);
    let mut session = Session::new(&provider, system, vec![lookup_tool()]);
    session.deferred_tools = vec![convert_units_tool()];
    session.items.push(ConversationItem::user(
        "Look up the key alpha with the lookup tool and tell me its value.",
    ));
    // The lookup round and its answer: the history the join must not invalidate.
    for _ in 0..3 {
        let Some(response) = session.step().await else {
            return;
        };
        if response.tool_calls().is_empty() {
            break;
        }
    }
    session.assert_first_is_cacheable();

    // The join, as the shell makes it: the family moves into the tools in effect and a tool addition follows the turn.
    session.items.push(ConversationItem::user(
        "Convert 12 meters to feet with the convert_units tool and tell me the result.",
    ));
    session
        .items
        .push(ConversationItem::tool_addition(["convert_units"]));
    session.tools.push(convert_units_tool());
    session.deferred_tools.clear();
    let body = direct_messages_body(&session.request());
    let str_at = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let declared_deferred = body
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                str_at(tool, "name").as_deref() == Some("convert_units")
                    && tool
                        .get("defer_loading")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
            })
        });
    let offered = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                str_at(message, "role").as_deref() == Some("system")
                    && message
                        .get("content")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                str_at(block, "type").as_deref() == Some("tool_addition")
                            })
                        })
            })
        });
    assert!(
        declared_deferred && offered,
        "the join request is not a deferred tool plus a tool_addition: {body}"
    );

    let Some(response) = session.step().await else {
        return;
    };
    session.assert_last_reads_the_previous(
        "the request that offers the joined tool (a miss here means the API refused deferred tools and the sampler fell back)",
    );
    assert!(
        response
            .tool_calls()
            .iter()
            .any(|call| call.name == "convert_units"),
        "the model never called the tool the addition offered: {:?}",
        response.assistant_text()
    );
    if session.step().await.is_none() {
        return;
    }
    session.assert_last_reads_the_previous("the request after the joined tool ran");
}

const MARKER_V1: &str = "You are a terse assistant in an automated cache test. Answer every message in one short \
sentence, then end the reply with the marker word ALPHA on its own line. No markdown, no questions.";
const MARKER_V2: &str = "You are a terse assistant in an automated cache test. Answer every message in one short \
sentence, then end the reply with the marker word OMEGA on its own line. Use no other marker word. No markdown, no \
questions.";

/// A system prompt that changes mid-session is appended as a system message after the next user turn instead of
/// rewriting the opening prompt. The request that carries it must still read the whole conversation before it, and the
/// model must follow the new prompt, not the one the conversation opened with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Anthropic API with the Claude login; run with -- --ignored"]
async fn anthropic_system_prompt_update_keeps_the_history_cached_and_is_followed() {
    let Some(provider) = anthropic() else {
        return;
    };
    if !supports_system_messages(&provider.model) {
        skip(
            "Anthropic system prompt update",
            &format!("{} takes no system messages", provider.model),
        );
        return;
    }
    let mut session = Session::new(
        &provider,
        padded_system_prompt(MARKER_V1, provider.min_cacheable_tokens),
        Vec::new(),
    );
    session
        .items
        .push(ConversationItem::user("Say hello in five words or fewer."));
    let Some(first) = session.step().await else {
        return;
    };
    session.assert_first_is_cacheable();
    assert!(
        first.assistant_text().contains("ALPHA"),
        "the opening prompt was not followed, so the switch proves nothing: {:?}",
        first.assistant_text()
    );
    session
        .items
        .push(ConversationItem::user("Name one primary colour."));
    if session.step().await.is_none() {
        return;
    }
    session.assert_last_reads_the_previous("the second turn");

    // The update lands at the turn boundary, as the shell appends it, and maps after the next user message.
    session.items.push(ConversationItem::system_prompt_update(
        padded_system_prompt(MARKER_V2, provider.min_cacheable_tokens),
    ));
    session
        .items
        .push(ConversationItem::user("Name one planet."));
    let body = direct_messages_body(&session.request());
    let in_history = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("system")
            })
        });
    assert!(
        in_history,
        "the update is not a system message in the history: {body}"
    );
    let Some(updated) = session.step().await else {
        return;
    };
    session.assert_last_reads_the_previous(
        "the request with the system prompt update (a miss here means the API refused system messages and the sampler fell back)",
    );
    let text = updated.assistant_text();
    assert!(
        text.contains("OMEGA") && !text.contains("ALPHA"),
        "the model did not follow the updated system prompt: {text:?}"
    );
}
