// Modified for Distill by Samuel Fajreldines, 2026.
use super::{
    MCP_QUALIFIED_NAME_MAX_CHARS, McpToolAdmissionError, PROVIDER_TOOL_NAME_MAX_CHARS,
    parse_mcp_qualified_name, parse_mcp_tool_name, qualify_mcp_tool_name, validate_tool_name,
};

/// Prefixes that differ by 6 chars. Tools of length 32..=37 then overflow the
/// 64-char provider budget on the longer prefix only (`31+2+n` vs `25+2+n`).
const LONG_SERVER: &str = "sL_xxxxxxxxxxxxxxxxxxxxxxxxxxxx";
const SHORT_SERVER: &str = "sS_xxxxxxxxxxxxxxxxxxxxxx";

/// Fail `validate_tool_name` under [`LONG_SERVER`] and pass under [`SHORT_SERVER`].
const OVERFLOW_TOOLS: &[&str] = &[
    "t0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "t1_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "t2_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "t3_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "t4_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "t5_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "t6_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
];

#[test]
fn qualified_mcp_name_parser_accepts_structurally_valid_tool_ids() {
    for (name, expected) in [
        ("linear__list_issues", ("linear", "list_issues")),
        ("123__lookup", ("123", "lookup")),
        ("auth__2fa_enable", ("auth", "2fa_enable")),
        ("s__2", ("s", "2")),
        ("server:scope__tool", ("server:scope", "tool")),
    ] {
        let (id, server, tool) = parse_mcp_qualified_name(name).expect("valid qualified ID");
        assert_eq!(id.as_str(), name);
        assert_eq!((server, tool), expected);
        assert_eq!(
            parse_mcp_tool_name(name),
            Some((expected.0.to_owned(), expected.1.to_owned()))
        );
    }
}

#[test]
fn qualified_mcp_name_parser_rejects_malformed_names() {
    for name in [
        "server__part__tool",
        "server__tool__part",
        "foo___bar",
        "foo____bar",
        "__tool",
        "server__",
        "server",
        "",
        "server__bad.tool",
    ] {
        assert!(
            parse_mcp_qualified_name(name).is_none(),
            "unexpectedly accepted {name:?}"
        );
    }
}

#[test]
fn qualify_rejects_provider_invalid_segments_not_qualified_length() {
    // parse/ToolId accept a digit-leading server segment; catalog admission does
    // not — the qualified key would start with a digit.
    assert!(parse_mcp_qualified_name("123__lookup").is_some());
    assert!(distill_tool_protocol::ToolId::new("123__lookup").is_ok());
    assert!(matches!(
        qualify_mcp_tool_name("123", "lookup"),
        Err(McpToolAdmissionError::InvalidServerName { .. })
    ));
    assert!(matches!(
        qualify_mcp_tool_name("server:scope", "tool"),
        Err(McpToolAdmissionError::InvalidServerName { .. })
    ));
    assert!(matches!(
        qualify_mcp_tool_name("foo_", "bar"),
        Err(McpToolAdmissionError::InvalidOrAmbiguousQualifiedName { .. })
    ));

    let server_61 = format!("a{}", "b".repeat(60));
    let server_62 = format!("a{}", "b".repeat(61));
    let valid_64 = format!("{server_61}__b");
    let over_provider = format!("{server_62}__b");
    assert_eq!(valid_64.len(), PROVIDER_TOOL_NAME_MAX_CHARS);
    assert_eq!(over_provider.len(), PROVIDER_TOOL_NAME_MAX_CHARS + 1);
    assert!(validate_tool_name(&valid_64).is_ok());
    assert!(validate_tool_name(&over_provider).is_err());
    assert!(qualify_mcp_tool_name(&server_61, "b").is_ok());
    assert!(qualify_mcp_tool_name(&server_62, "b").is_ok());
}

#[test]
fn qualify_admits_digit_leading_tool_when_qualified_key_is_valid() {
    assert!(parse_mcp_qualified_name("auth__2fa_enable").is_some());
    assert!(distill_tool_protocol::ToolId::new("auth__2fa_enable").is_ok());
    assert_eq!(
        qualify_mcp_tool_name("auth", "2fa_enable").as_deref(),
        Ok("auth__2fa_enable")
    );
    assert_eq!(qualify_mcp_tool_name("s", "2").as_deref(), Ok("s__2"));
    assert!(validate_tool_name("2fa_enable").is_err());
    assert!(validate_tool_name("auth__2fa_enable").is_ok());
    assert!(matches!(
        qualify_mcp_tool_name("auth", "bad.tool"),
        Err(McpToolAdmissionError::InvalidToolName { .. })
    ));
}

#[test]
fn qualify_enforces_catalog_cap_not_provider_64() {
    let server = "a";
    let tool = "b".repeat(MCP_QUALIFIED_NAME_MAX_CHARS - 3);
    let ok = format!("{server}__{tool}");
    assert_eq!(ok.len(), MCP_QUALIFIED_NAME_MAX_CHARS);
    assert_eq!(
        qualify_mcp_tool_name(server, &tool).as_deref(),
        Ok(ok.as_str())
    );

    let too_long = format!("{tool}x");
    assert!(matches!(
        qualify_mcp_tool_name(server, &too_long),
        Err(McpToolAdmissionError::QualifiedNameTooLong {
            len,
            max: MCP_QUALIFIED_NAME_MAX_CHARS,
            ..
        }) if len == MCP_QUALIFIED_NAME_MAX_CHARS + 1
    ));
}

#[test]
fn long_prefix_tools_overflow_provider_64_and_fit_short_prefix() {
    assert_eq!(LONG_SERVER.len(), 31);
    assert_eq!(SHORT_SERVER.len(), 25);
    assert_eq!(LONG_SERVER.len() - SHORT_SERVER.len(), 6);
    for tool in OVERFLOW_TOOLS {
        let long_q = format!("{LONG_SERVER}__{tool}");
        let short_q = format!("{SHORT_SERVER}__{tool}");
        assert!(
            long_q.len() > PROVIDER_TOOL_NAME_MAX_CHARS,
            "{tool} long qualified len {}",
            long_q.len()
        );
        assert!(
            short_q.len() <= PROVIDER_TOOL_NAME_MAX_CHARS,
            "{tool} short qualified len {}",
            short_q.len()
        );
        assert!(validate_tool_name(&long_q).is_err(), "{long_q}");
        assert!(validate_tool_name(&short_q).is_ok(), "{short_q}");
    }
}

#[test]
fn long_and_short_prefix_admit_the_same_26_tool_catalog() {
    let short: Vec<String> = (0..19).map(|i| format!("short_tool_{i:02}")).collect();
    let all: Vec<&str> = short
        .iter()
        .map(String::as_str)
        .chain(OVERFLOW_TOOLS.iter().copied())
        .collect();
    assert_eq!(all.len(), 26);

    let long_ok = all
        .iter()
        .filter(|tool| qualify_mcp_tool_name(LONG_SERVER, tool).is_ok())
        .count();
    let short_ok = all
        .iter()
        .filter(|tool| qualify_mcp_tool_name(SHORT_SERVER, tool).is_ok())
        .count();
    assert_eq!(long_ok, 26);
    assert_eq!(short_ok, 26);

    let long_provider_ok = all
        .iter()
        .filter(|tool| validate_tool_name(&format!("{LONG_SERVER}__{tool}")).is_ok())
        .count();
    let short_provider_ok = all
        .iter()
        .filter(|tool| validate_tool_name(&format!("{SHORT_SERVER}__{tool}")).is_ok())
        .count();
    assert_eq!(long_provider_ok, 19);
    assert_eq!(short_provider_ok, 26);
}
