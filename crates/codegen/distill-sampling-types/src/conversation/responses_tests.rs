use super::test_support::*;
use super::*;
use crate::tool_overrides::*;
use assert_matches::assert_matches;

#[test]
fn test_conversation_request_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System prompt"),
        ConversationItem::user("User message"),
    ])
    .with_model("grok-3")
    .with_temperature(0.7);

    let responses_req: rs::CreateResponse = (&req).into();
    assert_eq!(responses_req.model, Some("grok-3".to_string()));
    assert_eq!(responses_req.temperature, Some(0.7));

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn function_tool_colliding_with_hosted_web_search_is_dropped() {
    let mut req =
        ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_tools(vec![
            ToolSpec {
                name: "web_search".to_string(),
                description: Some("local web search".to_string()),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "read_file".to_string(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        ]);
    req.hosted_tools = vec![HostedTool::WebSearch { options: None }];

    let responses_req: rs::CreateResponse = (&req).into();
    let tools = responses_req.tools.expect("tools should be set");

    // web_search is emitted as a raw-JSON `extra_tool_entries` entry, so it never appears as a native `rs::Tool::WebSearch`
    // The raw entry can carry `excluded_domains`, which async_openai's typed filter omits
    let web_search_count = tools
        .iter()
        .filter(|t| matches!(t, rs::Tool::WebSearch(_)))
        .count();
    assert_eq!(
        web_search_count, 0,
        "web_search is not a native tool: {tools:?}"
    );
    let function_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| match t {
            rs::Tool::Function(f) => Some(f.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        function_names,
        vec!["read_file"],
        "colliding function tool must be dropped"
    );

    // The hosted web_search is emitted as a raw entry instead.
    let entries = extra_tool_entries(&req.hosted_tools);
    assert_eq!(entries, vec![serde_json::json!({"type": "web_search"})]);
}

#[test]
fn function_tool_colliding_with_hosted_x_search_is_dropped() {
    let mut req =
        ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_tools(vec![
            ToolSpec {
                name: "x_search".to_string(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        ]);
    req.hosted_tools = vec![HostedTool::XSearch { options: None }];

    let responses_req: rs::CreateResponse = (&req).into();
    let tools = responses_req.tools.unwrap_or_default();
    assert!(tools.is_empty(), "expected no tools, got: {tools:?}");
    let entries = extra_tool_entries(&req.hosted_tools);
    assert_eq!(entries, vec![serde_json::json!({"type": "x_search"})]);
}

/// The hosted `web_search` domain policy only reaches the API through this raw entry (async_openai's typed filters model no blocklist).
/// Both filters must survive the conversion from `HostedTool` to `extra_tool_entries`.
/// An empty or absent policy must stay byte-identical to the bare tool.
#[test]
fn web_search_domain_filters_reach_the_tool_entry() {
    let hosted = |options: Option<WebSearchOptions>| {
        extra_tool_entries(&[HostedTool::WebSearch { options }])
    };
    assert_eq!(
        hosted(Some(WebSearchOptions {
            allowed_domains: Some(vec!["docs.x.ai".into(), "arxiv.org".into()]),
            excluded_domains: None,
        })),
        vec![serde_json::json!({
            "type": "web_search",
            "filters": { "allowed_domains": ["docs.x.ai", "arxiv.org"] },
        })]
    );
    assert_eq!(
        hosted(Some(WebSearchOptions {
            allowed_domains: None,
            excluded_domains: Some(vec!["reddit.com".into()]),
        })),
        vec![serde_json::json!({
            "type": "web_search",
            "filters": { "excluded_domains": ["reddit.com"] },
        })]
    );

    // No policy (absent, default, or empty lists) emits the bare tool.
    let bare = vec![serde_json::json!({ "type": "web_search" })];
    assert_eq!(hosted(None), bare);
    assert_eq!(hosted(Some(WebSearchOptions::default())), bare);
    assert_eq!(
        hosted(Some(WebSearchOptions {
            allowed_domains: Some(vec![]),
            excluded_domains: Some(vec![]),
        })),
        bare
    );
}

#[test]
fn x_search_serializes_to_the_tool_entry() {
    // A full bound reaches the flat snake_case entry; an empty or `None` bound emits the bare entry.
    let dated = extra_tool_entries(&[HostedTool::XSearch {
        options: Some(XSearchOptions {
            date_bound: Some(
                SearchDateBound::new(Some("2024-01-01".into()), Some("2024-03-15".into())).unwrap(),
            ),
        }),
    }]);
    assert_eq!(
        dated,
        vec![serde_json::json!({
            "type": "x_search",
            "from_date": "2024-01-01",
            "to_date": "2024-03-15",
        })]
    );
    let bare = vec![serde_json::json!({"type": "x_search"})];
    assert_eq!(
        extra_tool_entries(&[HostedTool::XSearch {
            options: Some(XSearchOptions {
                date_bound: Some(SearchDateBound::new(None, None).unwrap()),
            }),
        }]),
        bare
    );
    assert_eq!(
        extra_tool_entries(&[HostedTool::XSearch { options: None }]),
        bare
    );
}

#[test]
fn function_web_search_kept_when_no_hosted_tools() {
    let req = ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_tools(vec![
        ToolSpec {
            name: "web_search".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        },
    ]);

    let responses_req: rs::CreateResponse = (&req).into();
    let tools = responses_req.tools.expect("tools should be set");
    let function_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| match t {
            rs::Tool::Function(f) => Some(f.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(function_names, vec!["web_search"]);
}

#[test]
fn test_responses_api_response_to_conversation_item() {
    use crate::rs;

    // Create a Response with text output
    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_123".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![rs::OutputItem::Message(rs::OutputMessage {
            content: vec![rs::OutputMessageContent::OutputText(
                rs::OutputTextContent {
                    text: "Hello from Responses API!".to_string(),
                    annotations: vec![],
                    logprobs: None,
                },
            )],
            id: "msg_123".to_string(),
            role: rs::AssistantRole::Assistant,
            status: rs::OutputStatus::Completed,
        })],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response);
    let item = items
        .into_iter()
        .next_back()
        .expect("response produces at least a trailing Assistant");
    assert_eq!(item.text_content(), "Hello from Responses API!");
    let ConversationItem::Assistant(a) = &item else {
        panic!("Expected Assistant item");
    };
    assert_eq!(a.model_id, Some("grok-3".to_string()));
    assert_eq!(
        a.reasoning_effort, None,
        "no reasoning config on the response => no effort recorded"
    );

    // Response with function call
    let response_with_fc = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_456".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![rs::OutputItem::FunctionCall(rs::FunctionToolCall {
            arguments: r#"{"path": "/bar.txt"}"#.to_string(),
            call_id: "call_789".to_string(),
            name: "read_file".to_string(),
            id: None,
            status: None,
        })],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response_with_fc);
    let item = items
        .into_iter()
        .next_back()
        .expect("response produces at least a trailing Assistant");
    let ConversationItem::Assistant(a) = &item else {
        panic!("Expected Assistant item");
    };
    let [tc] = a.tool_calls.as_slice() else {
        panic!("expected one tool call: {:?}", a.tool_calls);
    };
    assert_eq!(tc.id.as_ref(), "call_789");
    assert_eq!(tc.name, "read_file");
}

#[test]
fn test_response_reasoning_effort_stamped_on_assistant() {
    use crate::rs;

    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_eff".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: Some(rs::Reasoning {
            effort: Some(rs::ReasoningEffort::Xhigh),
            summary: None,
        }),
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response);
    let ConversationItem::Assistant(a) = items.last().expect("trailing Assistant") else {
        panic!("Expected Assistant item");
    };
    assert_eq!(a.reasoning_effort, Some(crate::ReasoningEffort::Xhigh));

    // Round-trips through the persisted representation.
    let json = serde_json::to_string(&items.last().unwrap()).unwrap();
    assert!(json.contains(r#""reasoning_effort":"xhigh""#), "{json}");
    let back: ConversationItem = serde_json::from_str(&json).unwrap();
    let ConversationItem::Assistant(b) = back else {
        panic!("Expected Assistant item");
    };
    assert_eq!(b.reasoning_effort, Some(crate::ReasoningEffort::Xhigh));
}

#[test]
fn test_tool_calls_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System"),
        ConversationItem::user("User"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.into(),
        }]),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fc_items: Vec<_> = items
        .iter()
        .filter(|item| matches!(item, rs::InputItem::Item(rs::Item::FunctionCall(_))))
        .collect();

    assert_eq!(fc_items.len(), 1, "Expected exactly one FunctionCall item");

    let Some(rs::InputItem::Item(rs::Item::FunctionCall(fc))) = fc_items.first() else {
        panic!("Expected FunctionCall item: {fc_items:?}");
    };
    assert_eq!(fc.call_id, "call_1");
    assert_eq!(fc.name, "bash");
    assert_eq!(fc.arguments, r#"{"command": "ls"}"#);
}

#[test]
fn test_tool_result_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System"),
        ConversationItem::user("User"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.into(),
        }]),
        ConversationItem::tool_result("call_1", "file1.txt\nfile2.txt\nfile3.txt"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco_items: Vec<_> = items
        .iter()
        .filter(|item| matches!(item, rs::InputItem::Item(rs::Item::FunctionCallOutput(_))))
        .collect();

    assert_eq!(
        fco_items.len(),
        1,
        "Expected exactly one FunctionCallOutput item"
    );

    let Some(rs::InputItem::Item(rs::Item::FunctionCallOutput(fco))) = fco_items.first() else {
        panic!("Expected FunctionCallOutput item: {fco_items:?}");
    };
    assert_eq!(fco.call_id, "call_1");
    let rs::FunctionCallOutput::Text(text) = &fco.output else {
        panic!("Expected Text output");
    };
    assert_eq!(text, "file1.txt\nfile2.txt\nfile3.txt");
}

#[test]
fn test_multiple_tool_results_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Run these commands"),
        ConversationItem::assistant_tool_calls(vec![
            ToolCall {
                id: "call_1".into(),
                name: "bash".to_string(),
                arguments: r#"{"command": "ls"}"#.into(),
            },
            ToolCall {
                id: "call_2".into(),
                name: "bash".to_string(),
                arguments: r#"{"command": "pwd"}"#.into(),
            },
        ]),
        ConversationItem::tool_result("call_1", "output1"),
        ConversationItem::tool_result("call_2", "output2"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco)
            } else {
                None
            }
        })
        .collect();

    let [f0, f1] = fco_items.as_slice() else {
        panic!("expected two function call outputs: {fco_items:?}");
    };
    assert_eq!(f0.call_id, "call_1");
    assert_eq!(f1.call_id, "call_2");
}

#[test]
fn test_responses_api_with_encrypted_reasoning() {
    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_456".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![
            rs::OutputItem::Reasoning(rs::ReasoningItem {
                id: "reasoning_enc".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "Visible thinking summary".to_string(),
                })],
                content: None,
                encrypted_content: Some("enc_base64_encrypted_reasoning_data_here".to_string()),
                status: Some(rs::OutputStatus::Completed),
            }),
            rs::OutputItem::Message(rs::OutputMessage {
                content: vec![rs::OutputMessageContent::OutputText(
                    rs::OutputTextContent {
                        text: "My response based on reasoning.".to_string(),
                        annotations: vec![],
                        logprobs: None,
                    },
                )],
                id: "msg_456".to_string(),
                role: rs::AssistantRole::Assistant,
                status: rs::OutputStatus::Completed,
            }),
        ],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    // Exercise the flat-list path: reasoning lives as a sibling
    let items = response_to_conversation_items(response);
    let assistant_idx = items
        .iter()
        .position(|i| matches!(i, ConversationItem::Assistant(_)))
        .expect("assistant present");
    let Some(ConversationItem::Assistant(a)) = items.get(assistant_idx) else {
        panic!("expected assistant at {assistant_idx}: {items:?}");
    };
    assert_eq!(a.content.as_ref(), "My response based on reasoning.");

    let reasoning_sibling = items
        .iter()
        .find_map(|i| match i {
            ConversationItem::Reasoning(r) => Some(r),
            _ => None,
        })
        .expect("reasoning sibling present");
    // Both the text summary and the encrypted content survive
    assert_eq!(
        reasoning_sibling.summary.first().map(|sp| match sp {
            rs::SummaryPart::SummaryText(t) => t.text.as_str(),
        }),
        Some("Visible thinking summary")
    );
    assert_eq!(
        reasoning_sibling.encrypted_content.as_deref(),
        Some("enc_base64_encrypted_reasoning_data_here")
    );
}

#[test]
fn test_responses_api_with_only_encrypted_reasoning() {
    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_789".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![
            rs::OutputItem::Reasoning(rs::ReasoningItem {
                id: "reasoning_only_enc".to_string(),
                summary: vec![],
                content: None,
                encrypted_content: Some("enc_only_encrypted_no_visible_summary".to_string()),
                status: Some(rs::OutputStatus::Completed),
            }),
            rs::OutputItem::Message(rs::OutputMessage {
                content: vec![rs::OutputMessageContent::OutputText(
                    rs::OutputTextContent {
                        text: "Response.".to_string(),
                        annotations: vec![],
                        logprobs: None,
                    },
                )],
                id: "msg_789".to_string(),
                role: rs::AssistantRole::Assistant,
                status: rs::OutputStatus::Completed,
            }),
        ],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    // Flat-list path: reasoning sibling carries the encrypted blob, empty summary maps to an empty `Vec<SummaryPart>`
    let items = response_to_conversation_items(response);
    let reasoning_sibling = items
        .iter()
        .find_map(|i| match i {
            ConversationItem::Reasoning(r) => Some(r),
            _ => None,
        })
        .expect("reasoning sibling present");
    assert!(reasoning_sibling.summary.is_empty());
    assert_eq!(
        reasoning_sibling.encrypted_content.as_deref(),
        Some("enc_only_encrypted_no_visible_summary")
    );
}

#[test]
fn test_encrypted_reasoning_included_in_responses_api_request() {
    // Encrypted reasoning is crucial for context continuity across turns
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are helpful"),
        ConversationItem::user("What is 2+2?"),
        // Previous reasoning and assistant: reasoning is a sibling
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r1".to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: "Let me calculate 2+2...".to_string(),
            })],
            content: None,
            encrypted_content: Some("enc_secret_reasoning_chain".to_string()),
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            content: "The answer is 4.".into(),
            tool_calls: vec![],
            model_id: Some("grok-3".to_string()),
            model_fingerprint: None,
            reasoning_effort: None,
        }),
        ConversationItem::user("Now what is 3+3?"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let reasoning_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::Reasoning(r)) = item {
                Some(r)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        reasoning_items.len(),
        1,
        "Should have exactly one reasoning item"
    );

    let [reasoning] = reasoning_items.as_slice() else {
        panic!("expected one reasoning item: {reasoning_items:?}");
    };
    assert_eq!(
        reasoning.encrypted_content,
        Some("enc_secret_reasoning_chain".to_string())
    );

    // Verify summary text is included
    let [rs::SummaryPart::SummaryText(summary)] = reasoning.summary.as_slice() else {
        panic!("expected one summary: {:?}", reasoning.summary);
    };
    assert_eq!(summary.text, "Let me calculate 2+2...");
}

#[test]
fn test_only_encrypted_reasoning_included_in_request() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Hello"),
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: String::new(),
            summary: vec![],
            content: None,
            encrypted_content: Some("enc_hidden_thoughts".to_string()),
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            content: "Hi!".into(),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let reasoning_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::Reasoning(r)) = item {
                Some(r)
            } else {
                None
            }
        })
        .collect();

    let [reasoning] = reasoning_items.as_slice() else {
        panic!("expected one reasoning item: {reasoning_items:?}");
    };

    assert_eq!(
        reasoning.encrypted_content,
        Some("enc_hidden_thoughts".to_string())
    );

    assert!(reasoning.summary.is_empty());
}

#[test]
fn test_no_reasoning_item_when_no_reasoning() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Hello"),
        ConversationItem::assistant("Hi!"), // No reasoning
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let reasoning_items: Vec<_> = items
        .iter()
        .filter(|item| matches!(item, rs::InputItem::Item(rs::Item::Reasoning(_))))
        .collect();

    assert!(reasoning_items.is_empty(), "Should have no reasoning items");
}

#[test]
fn test_conversation_request_with_tools_to_responses_api() {
    let tools = vec![ToolSpec {
        name: "search".to_string(),
        description: Some("Search the codebase".to_string()),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            }
        }),
    }];

    let req = ConversationRequest::from_items(vec![ConversationItem::user("Find TODO comments")])
        .with_tools(tools);

    let responses_req: rs::CreateResponse = (&req).into();
    assert!(responses_req.tools.is_some());
    let tools = responses_req.tools.unwrap();
    assert_eq!(tools.len(), 1);

    let Some(rs::Tool::Function(ft)) = tools.first() else {
        panic!("Expected Function tool: {tools:?}");
    };
    assert_eq!(ft.name, "search");
    assert_eq!(ft.description, Some("Search the codebase".to_string()));
}

#[test]
fn test_tool_choice_to_responses_api() {
    // Test Auto
    let req = ConversationRequest::from_items(vec![ConversationItem::user("test")])
        .with_tool_choice(ConversationToolChoice::Auto);
    let responses_req: rs::CreateResponse = (&req).into();
    assert_matches!(
        responses_req.tool_choice,
        Some(rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto))
    );

    // Test Required
    let req = ConversationRequest::from_items(vec![ConversationItem::user("test")])
        .with_tool_choice(ConversationToolChoice::Required);
    let responses_req: rs::CreateResponse = (&req).into();
    assert_matches!(
        responses_req.tool_choice,
        Some(rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Required))
    );

    // Test Function
    let req = ConversationRequest::from_items(vec![ConversationItem::user("test")])
        .with_tool_choice(ConversationToolChoice::Function("bash".to_string()));
    let responses_req: rs::CreateResponse = (&req).into();
    let Some(rs::ToolChoiceParam::Function(fc)) = responses_req.tool_choice else {
        panic!("Expected Function tool choice");
    };
    assert_eq!(fc.name, "bash");
}

#[test]
fn test_malformed_tool_arguments_sanitized_in_responses_api() {
    let bad_args = r#"{"file_path": "/testbed/cxx_polynomial/include/emsr/remez.h", "old_string": "", new_string": "x"}"#;

    let tool_call = ToolCall {
        id: "call_bad".into(),
        name: "search_replace".to_string(),
        arguments: bad_args.into(),
    };

    let item = ConversationItem::assistant_tool_calls(vec![tool_call]);
    let req = ConversationRequest {
        items: vec![item],
        ..Default::default()
    };

    let rs_req: crate::rs::CreateResponse = (&req).into();

    // The FunctionCall input item must carry sanitized arguments.
    let crate::rs::InputParam::Items(items) = rs_req.input else {
        panic!("Expected InputParam::Items");
    };
    let fc_args = items.iter().find_map(|inp| {
        if let crate::rs::InputItem::Item(crate::rs::Item::FunctionCall(fc)) = inp {
            Some(fc.arguments.clone())
        } else {
            None
        }
    });

    let fc_args = fc_args.expect("should find a FunctionCall input item");
    assert_eq!(
        fc_args, "{}",
        "malformed arguments must be replaced with {{}} in Responses API path"
    );
}

#[test]
fn test_responses_request_carries_reasoning_effort_nested() {
    for (variant, expected) in [
        (crate::ReasoningEffort::None, "none"),
        (crate::ReasoningEffort::Minimal, "minimal"),
        (crate::ReasoningEffort::Low, "low"),
        (crate::ReasoningEffort::Medium, "medium"),
        (crate::ReasoningEffort::High, "high"),
        (crate::ReasoningEffort::Xhigh, "xhigh"),
        (crate::ReasoningEffort::Max, "max"),
    ] {
        let req = ConversationRequest {
            reasoning_effort: Some(variant),
            ..ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_model("test")
        };
        let resp: crate::rs::CreateResponse = (&req).into();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json.pointer("/reasoning/effort").and_then(|v| v.as_str()),
            Some(expected),
            "{variant:?} should serialize as reasoning.effort={expected:?}; got: {json:#}",
        );
    }
}

#[test]
fn test_responses_request_omits_effort_when_unset() {
    let req =
        ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_model("test");
    let resp: crate::rs::CreateResponse = (&req).into();
    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.pointer("/reasoning/effort").is_none(),
        "reasoning.effort must be absent when unset; got: {json:#}",
    );
}

#[test]
fn test_btw_cross_api_responses_no_regressions() {
    let items = btw_prepare_items(btw_mid_turn_conversation());
    let req = ConversationRequest::from_items(items);
    let resp: rs::CreateResponse = (&req).into();
    let json = serde_json::to_value(&resp).unwrap();

    let rs::InputParam::Items(input_items) = &resp.input else {
        panic!("Expected InputParam::Items");
    };

    // Count FunctionCall and FunctionCallOutput items.
    let function_calls: Vec<_> = input_items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCall(fc)) = item {
                Some(fc.call_id.clone())
            } else {
                None
            }
        })
        .collect();
    let function_outputs: Vec<_> = input_items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco.call_id.clone())
            } else {
                None
            }
        })
        .collect();

    // call_1 must be present as both FunctionCall and FunctionCallOutput.
    assert!(
        function_calls.contains(&"call_1".to_string()),
        "completed FunctionCall call_1 must survive; got calls: {function_calls:?}"
    );
    assert!(
        function_outputs.contains(&"call_1".to_string()),
        "completed FunctionCallOutput call_1 must survive; got outputs: {function_outputs:?}"
    );

    assert!(
        !function_calls.contains(&"call_2".to_string()),
        "orphaned FunctionCall call_2 must be removed"
    );

    // No Reasoning items (reasoning was stripped).
    let has_reasoning = input_items
        .iter()
        .any(|item| matches!(item, rs::InputItem::Item(rs::Item::Reasoning(_))));
    assert!(!has_reasoning, "reasoning items must be stripped");

    assert!(
        json.get("temperature").is_none()
            || json.pointer("/temperature").is_some_and(|v| v.is_null()),
        "temperature must be absent; got: {json:#}",
    );
}

#[test]
fn test_transform_cwd_rewrites_reasoning_sibling() {
    // Reasoning siblings are subject to CWD rewriting via `transform_conversation_cwd` (see the `Reasoning(_)` arm)
    let worktree = "/workspace/.grok/worktrees/project/ab-uuid-a";
    let root = "/workspace/project";

    let mut items = vec![
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: "rs_1".to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: format!("thinking about {worktree}"),
            })],
            content: None,
            encrypted_content: None,
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            content: format!("I edited {worktree}/src/main.rs").into(),
            tool_calls: vec![],
            model_id: Some("grok-3".to_string()),
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];

    transform_conversation_cwd(&mut items, worktree, root);

    assert_eq!(
        items.get(1).map(|i| i.text_content()),
        Some(format!("I edited {root}/src/main.rs"))
    );
    let Some(ConversationItem::Reasoning(r)) = items.first() else {
        panic!("expected Reasoning sibling: {items:?}");
    };
    let Some(rs::SummaryPart::SummaryText(t)) = r.summary.first() else {
        panic!("expected summary text: {:?}", r.summary)
    };
    assert!(
        !t.text.contains(worktree),
        "reasoning sibling text should be rewritten"
    );
    assert!(t.text.contains(root));
}

// ── Tool result with images tests ──────────────────────────────────────────

#[test]
fn test_tool_result_with_images_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System"),
        ConversationItem::user("Read this image"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "read_file".to_string(),
            arguments: r#"{"target_file": "photo.png"}"#.into(),
        }]),
        ConversationItem::tool_result_with_images(
            "call_1",
            "Read image file: photo.png",
            vec![ContentPart::Image {
                url: "data:image/png;base64,iVBOR".into(),
            }],
        ),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco)
            } else {
                None
            }
        })
        .collect();

    let [fco] = fco_items.as_slice() else {
        panic!("expected one function call output: {fco_items:?}");
    };
    assert_eq!(fco.call_id, "call_1");

    // Should be Content variant, not Text
    let rs::FunctionCallOutput::Content(parts) = &fco.output else {
        panic!("Expected Content output with images, got Text");
    };
    let [p0, p1] = parts.as_slice() else {
        panic!("Expected text + 1 image: {parts:?}");
    };
    assert!(matches!(p0, rs::InputContent::InputText(t) if t.text == "Read image file: photo.png"));
    assert!(
        matches!(p1, rs::InputContent::InputImage(img) if img.image_url.as_deref() == Some("data:image/png;base64,iVBOR"))
    );
}

#[test]
fn test_tool_result_without_images_stays_text() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Run ls"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.into(),
        }]),
        ConversationItem::tool_result("call_1", "file1.txt\nfile2.txt"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();
    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco = items
        .iter()
        .find_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco)
            } else {
                None
            }
        })
        .unwrap();

    assert!(matches!(&fco.output, rs::FunctionCallOutput::Text(t) if t == "file1.txt\nfile2.txt"));
}

#[test]
fn responses_api_conversion_preserves_model_fingerprint() {
    use std::collections::HashMap;

    let mut metadata = HashMap::new();
    metadata.insert("system_fingerprint".into(), "fp_abc123".into());

    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 0,
        completed_at: None,
        error: None,
        id: "resp_test".into(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: Some(metadata),
        model: "grok-4.5".into(),
        object: "response".into(),
        output: vec![rs::OutputItem::Message(rs::OutputMessage {
            content: vec![rs::OutputMessageContent::OutputText(
                rs::OutputTextContent {
                    text: "hello".into(),
                    annotations: vec![],
                    logprobs: None,
                },
            )],
            id: "msg_test".into(),
            role: rs::AssistantRole::Assistant,
            status: rs::OutputStatus::Completed,
        })],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response);
    let item = items
        .into_iter()
        .next_back()
        .expect("response produces at least a trailing Assistant");
    assert_matches!(item, ConversationItem::Assistant(ref a) => {
        assert_eq!(a.model_fingerprint.as_deref(), Some("fp_abc123"));
        assert_eq!(a.model_id.as_deref(), Some("grok-4.5"));
        assert_eq!(a.content.as_ref(), "hello");
    });
}

#[test]
fn empty_reason_reasoning_only() {
    // A response with a Reasoning sibling but empty Assistant content is classified as ReasoningOnly so the retry logic resamples
    let response = ConversationResponse {
        items: vec![
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r1".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "thinking but no text output".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            ConversationItem::Assistant(AssistantItem {
                content: String::new().into(),
                tool_calls: Vec::new(),
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ],
        stop_reason: Some(StopReason::Stop),
        usage: None,
        cost_usd_ticks: None,
        message_chunks_emitted: 0,
        doom_loop_signals: Vec::new(),
        stop_message: None,
        message_id: None,
        raw_stop_reason: None,
        stop_sequence: None,
    };
    assert_eq!(
        response.empty_reason(),
        Some(crate::error::EmptyReason::ReasoningOnly)
    );
    assert!(response.is_empty());
}

#[test]
fn build_responses_input_preserves_multi_turn_ordering() {
    // 4-turn conversation where each assistant turn carries reasoning.
    // The wire-level item order must be
    // [Sys, U1, U2, U3, U4, U5, R, A1, R, A2, ...] which would shift the cache prefix every turn.
    fn r(text: &str) -> ConversationItem {
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: text.to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: text.to_string(),
            })],
            content: None,
            encrypted_content: Some(format!("enc_{text}")),
            status: None,
        })
    }
    let items: Vec<ConversationItem> = vec![
        ConversationItem::system("you are helpful"),
        ConversationItem::user("u1"),
        r("r1"),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        r("r2"),
        ConversationItem::assistant("a2"),
        ConversationItem::user("u3"),
        r("r3"),
        ConversationItem::assistant("a3"),
        ConversationItem::user("u4"),
        r("r4"),
        ConversationItem::assistant("a4"),
        ConversationItem::user("u5"),
    ];

    let req = ConversationRequest::from_items(items);
    let input = super::responses::build_responses_input(&req);
    let rs::InputParam::Items(wire_items) = input else {
        panic!("expected Items input");
    };

    // Walk the wire items and verify the expected pattern.
    // Roles per wire item: System, User, Reasoning(role=Assistant), Assistant, User, Reasoning, Assistant, ...
    let kinds: Vec<&'static str> = wire_items
        .iter()
        .map(|w| match w {
            rs::InputItem::EasyMessage(m) => match m.role {
                rs::Role::System => "Sys",
                rs::Role::User => "U",
                rs::Role::Assistant => "A",
                _ => "other",
            },
            rs::InputItem::Item(rs::Item::Reasoning(_)) => "R",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "Sys", "U", "R", "A", "U", "R", "A", "U", "R", "A", "U", "R", "A", "U",
        ],
        "multi-turn ordering must preserve interleaved Reasoning ↔ Assistant per turn"
    );
}

#[test]
fn upgrade_legacy_reasoning_singular_chat_completions_text_only() {
    // Chat-completions has only text (no encrypted, no id).
    let raw = serde_json::json!({
        "type": "assistant",
        "content": "answer",
        "reasoning": {"text": "step-by-step plain reasoning"}
    });
    let mut seen = std::collections::HashSet::new();
    let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
    assert_eq!(siblings.len(), 1);
    let Some(ConversationItem::Reasoning(r)) = siblings.first() else {
        panic!("expected Reasoning sibling: {siblings:?}");
    };
    assert_eq!(r.id, "");
    assert!(r.encrypted_content.is_none());
    let Some(rs::SummaryPart::SummaryText(s)) = r.summary.first() else {
        panic!("expected summary text: {:?}", r.summary)
    };
    assert_eq!(s.text, "step-by-step plain reasoning");
}

#[test]
fn upgrade_legacy_reasoning_v0_chat_request_message_shape() {
    // v0 on disk: top-level role and reasoning_content
    let raw = serde_json::json!({
        "role": "assistant",
        "content": "v0 answer",
        "reasoning_content": "v0-style plain text reasoning"
    });
    let mut seen = std::collections::HashSet::new();
    let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
    assert_eq!(siblings.len(), 1);
    let Some(ConversationItem::Reasoning(r)) = siblings.first() else {
        panic!("expected Reasoning sibling: {siblings:?}");
    };
    let Some(rs::SummaryPart::SummaryText(s)) = r.summary.first() else {
        panic!("expected summary text: {:?}", r.summary)
    };
    assert_eq!(s.text, "v0-style plain text reasoning");
}

#[test]
/// The Responses API refuses an empty, over-64-character or off-charset
/// `input[i].id`; a resumed session can carry all three, and one of them fails the
/// first request before any model output. Valid ids must survive byte-for-byte.
#[test]
fn patch_input_item_ids_repairs_only_what_the_api_refuses() {
    let long = "x".repeat(85);
    let valid_max = "y".repeat(64);
    let too_long = "z".repeat(65);
    let mut body = serde_json::json!({
        "input": [
            {"type": "reasoning", "id": "", "summary": []},
            {"type": "reasoning", "id": long, "summary": []},
            {"type": "reasoning", "id": "has space!", "summary": []},
            {"type": "reasoning", "id": valid_max, "summary": []},
            {"type": "reasoning", "id": too_long, "summary": []},
            {"type": "reasoning", "id": "rs_00000000-0000-7000-8000-000000000001", "summary": []},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]
    });
    patch_input_item_ids(&mut body);
    let items = body["input"].as_array().expect("input stays an array");

    for index in [0usize, 1, 2] {
        let id = items[index]["id"].as_str().expect("repaired ids are strings");
        assert!(id.starts_with("cid_"), "index {index} kept an invalid id: {id:?}");
        assert!(!id.is_empty() && id.len() <= 64);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }
    assert_eq!(items[3]["id"], valid_max, "a 64-character id is valid and stays");
    assert!(
        items[4]["id"].as_str().expect("string").starts_with("cid_"),
        "65 characters is one over the limit"
    );
    assert_eq!(
        items[5]["id"], "rs_00000000-0000-7000-8000-000000000001",
        "a valid provider id is left byte-for-byte alone"
    );
    assert!(items[6].get("id").is_none(), "an item without an id gains none");

    // Running it twice must not move the ids again: a retry of the same request
    // has to hash to the same repair, or every retry would look like new input.
    let once = body.clone();
    patch_input_item_ids(&mut body);
    assert_eq!(body, once, "the repair is idempotent");
}

fn patch_reasoning_text_types_injects_type_discriminator() {
    // Build a request body containing a reasoning item whose nested `content[]` entries lack the `type` field (the async-openai gap)
    let mut body = serde_json::json!({
        "input": [
            {
                "type": "reasoning",
                "id": "r1",
                "content": [
                    { "text": "thinking..." },
                    { "text": "more thinking" }
                ]
            },
            {
                "type": "message",
                "role": "user",
                "content": "hi"
            }
        ]
    });
    patch_reasoning_text_types(&mut body);
    let reasoning_content = body
        .pointer("/input/0/content")
        .and_then(|v| v.as_array())
        .expect("reasoning content array");
    for item in reasoning_content {
        assert_eq!(
            item.get("type").and_then(|t| t.as_str()),
            Some("reasoning_text"),
            "every nested content item must carry the discriminator"
        );
    }
    // Untouched: the user message stays as-is.
    assert_eq!(
        body.pointer("/input/1/content").and_then(|v| v.as_str()),
        Some("hi")
    );
}

#[test]
fn patch_reasoning_text_types_preserves_existing_type() {
    let mut body = serde_json::json!({
        "input": [
            {
                "type": "reasoning",
                "id": "r1",
                "content": [
                    // Post-upstream-fix shape: discriminator already present.
                    { "type": "reasoning_text", "text": "already tagged" },
                    // A hypothetical different discriminator must NOT be clobbered.
                    { "type": "some_future_variant", "text": "future shape" },
                    // Current gap: missing type gets filled in
                    { "text": "needs tag" }
                ]
            }
        ]
    });
    patch_reasoning_text_types(&mut body);
    let content = body
        .pointer("/input/0/content")
        .and_then(|v| v.as_array())
        .expect("reasoning content array");

    // Existing discriminators preserved verbatim (no clobber).
    assert_eq!(
        content
            .first()
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str()),
        Some("reasoning_text"),
    );
    assert_eq!(
        content
            .get(1)
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str()),
        Some("some_future_variant"),
        "a non-default upstream discriminator must be left untouched",
    );
    // Only the type-less item is filled in.
    assert_eq!(
        content
            .get(2)
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str()),
        Some("reasoning_text"),
    );

    // Object integrity: each item has exactly one `type` and its `text`.
    for item in content {
        let obj = item.as_object().expect("content item is an object");
        assert!(obj.contains_key("type") && obj.contains_key("text"));
    }
}

#[test]
fn build_responses_input_single_reasoning_sibling_lands_inline() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u1"),
        reasoning_sibling("r_abc", "thinking", Some("enc1")),
        ConversationItem::assistant("hi"),
    ]);

    let input = input_items_json(&req);
    let summary = summarise_input(&input);

    // Expected: [system, user, reasoning, assistant]
    let [s0, s1, s2, s3] = summary.as_slice() else {
        panic!("expected four summary items, got: {summary:?}");
    };
    assert_eq!(s0, "system:sys");
    assert_eq!(s1, "user:u1");
    assert_eq!(s2, "reasoning:r_abc");
    assert_eq!(s3, "assistant:hi");

    // No placeholder strings must appear
    let body_str = serde_json::to_string(&input).unwrap();
    assert!(
        !body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"),
        "no placeholder strings post-refactor"
    );

    assert_eq!(
        input
            .get(2)
            .and_then(|v| v.get("encrypted_content"))
            .and_then(|v| v.as_str()),
        Some("enc1"),
    );
}

#[test]
fn build_responses_input_multi_turn_reasoning_ordering() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "think 1", Some("enc1")),
        ConversationItem::assistant("a1"),
        ConversationItem::tool_result("tc1", "result1"),
        ConversationItem::user("u2"),
        reasoning_sibling("r2", "think 2", Some("enc2")),
        ConversationItem::assistant("a2"),
        ConversationItem::tool_result("tc2", "result2"),
        ConversationItem::user("u3"),
        reasoning_sibling("r3", "think 3", Some("enc3")),
        ConversationItem::assistant("a3"),
    ]);

    let input = input_items_json(&req);
    let summary = summarise_input(&input);

    // INVARIANT 1: There must be exactly N reasoning items for N siblings
    // The pre-refactor bug produced only 1
    let reasoning_count = summary
        .iter()
        .filter(|s| s.starts_with("reasoning:"))
        .count();
    assert_eq!(
        reasoning_count, 3,
        "must have 3 reasoning items, got {reasoning_count}. Items: {summary:?}"
    );

    // INVARIANT 2: Each reasoning must be BETWEEN its corresponding user message and the NEXT user message
    // Without this check, all reasoning items bunched at the end would still pass count
    let user_positions: Vec<usize> = summary
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("user:"))
        .map(|(i, _)| i)
        .collect();
    let reasoning_positions: Vec<usize> = summary
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("reasoning:"))
        .map(|(i, _)| i)
        .collect();

    assert_eq!(user_positions.len(), 3);
    assert_eq!(reasoning_positions.len(), 3);

    for (i, rp) in reasoning_positions.iter().enumerate() {
        let Some(user_pos) = user_positions.get(i).copied() else {
            panic!("missing user position {i}: {user_positions:?}");
        };
        assert!(
            *rp > user_pos,
            "reasoning {i} at position {rp} must be after user {i} at position {user_pos}. \
                 Items: {summary:?}",
        );
        if let Some(next_user) = user_positions.get(i + 1).copied() {
            assert!(
                *rp < next_user,
                "reasoning {i} at position {rp} must be before user {} at position {next_user}. \
                     Items: {summary:?}",
                i + 1,
            );
        }
    }

    // INVARIANT 3: encrypted_content per item is preserved 1:1.
    let mut enc_seen: Vec<&str> = Vec::new();
    for v in &input {
        if v.get("type").and_then(|t| t.as_str()) == Some("reasoning")
            && let Some(enc) = v.get("encrypted_content").and_then(|s| s.as_str())
        {
            enc_seen.push(enc);
        }
    }
    assert_eq!(enc_seen, vec!["enc1", "enc2", "enc3"]);
}

#[test]
fn backend_tool_call_position_stable() {
    let ws_a = ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
            id: "ws_a".to_string(),
            status: rs::WebSearchToolCallStatus::Completed,
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "alpha".to_string(),
                sources: Some(vec![]),
            }),
        }),
    });
    let ws_b = ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
            id: "ws_b".to_string(),
            status: rs::WebSearchToolCallStatus::Completed,
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "beta".to_string(),
                sources: Some(vec![]),
            }),
        }),
    });

    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "think a", Some("enc_a")),
        ws_a,
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        ws_b,
        ConversationItem::assistant("a2"),
    ]);

    let input = input_items_json(&req);
    let ws_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("web_search_call"))
        .collect();

    // Both backend tool calls must survive serialization.
    assert_eq!(
        ws_items.len(),
        2,
        "both web_search_call items must survive; got: {:?}",
        summarise_input(&input)
    );
    let ids: Vec<&str> = ws_items
        .iter()
        .filter_map(|v| v.get("id").and_then(|i| i.as_str()))
        .collect();
    assert_eq!(ids, vec!["ws_a", "ws_b"], "ordering preserved");
}

#[test]
fn empty_content_assistant_with_tool_calls_and_reasoning() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "must call a tool", Some("enc_pre_tool")),
        ConversationItem::Assistant(AssistantItem {
            content: Arc::<str>::from(""),
            tool_calls: vec![ToolCall {
                id: Arc::<str>::from("call_1"),
                name: "read_file".to_string(),
                arguments: Arc::<str>::from("{}"),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
        ConversationItem::tool_result("call_1", "file contents"),
    ]);

    let input = input_items_json(&req);
    let summary = summarise_input(&input);

    // (assistant message DROPPED because content is empty -- per conversation_item_to_input_items, lines 1718-1724) function_call_output (tool result)
    // No spurious extra reasoning items, no placeholder.
    let reasoning_count = summary
        .iter()
        .filter(|s| s.starts_with("reasoning:"))
        .count();
    assert_eq!(
        reasoning_count, 1,
        "exactly one reasoning item; got: {summary:?}"
    );
    assert!(
        summary.iter().any(|s| s == "function_call:call_1"),
        "function_call must appear; got: {summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|s| s.starts_with("type:function_call_output")),
        "function_call_output must appear; got: {summary:?}"
    );

    let body_str = serde_json::to_string(&input).unwrap();
    assert!(!body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"));
}

#[test]
fn serialized_body_contains_no_placeholder_strings() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "first", Some("enc1")),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        reasoning_sibling("r2", "second", Some("enc2")),
        ConversationItem::assistant("a2"),
        ConversationItem::user("u3"),
    ]);

    let cr: rs::CreateResponse = (&req).into();
    let mut body = serde_json::to_value(&cr).unwrap();
    patch_reasoning_text_types(&mut body);
    let body_str = serde_json::to_string(&body).unwrap();

    assert!(
        !body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"),
        "no placeholder strings post-sibling-Reasoning refactor"
    );

    // Both reasoning items must appear inline in the input array.
    let Some(input) = body.get("input").and_then(|v| v.as_array()) else {
        panic!("expected input array: {body:?}");
    };
    let reasoning_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
        .collect();
    assert_eq!(
        reasoning_items.len(),
        2,
        "both reasoning siblings must be present"
    );
}
