// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

use std::path::PathBuf;

use super::mcp::{
    MANAGED_MCP_NAME_MAX_CHARS, MANAGED_MCP_PREFIX, mcp_name_matches, mcp_transport_known,
    normalize_managed_name,
};
use super::parse::{McpPolicyList, parse_mcp_entry_list};
use crate::capturing_warn_logs;
use crate::permission::rules::DefaultPermissionMode;
use crate::permission::types::RuleAction;

const FOREIGN: PolicySubjectOrigin = PolicySubjectOrigin::Foreign;
const NATIVE: PolicySubjectOrigin = PolicySubjectOrigin::GrokNative;
const CLAUDE_PATH: &str = "/test/managed-settings.json";
const SYS_REQ: &str = "/etc/grok/requirements.toml";
const USER_REQ: &str = "/home/u/.grok/requirements.toml";
const SYS_MANAGED: &str = "/etc/grok/managed_config.toml";
const USER_MANAGED: &str = "/home/u/.grok/managed_config.toml";
const MDM_REQ: &str = "ai.x.grok:requirements_toml_base64";
/// Every admin-owned TOML tier with its path label.
const ADMIN_TIERS: [(PolicyLayerTier, &str); 3] = [
    (PolicyLayerTier::Mdm, MDM_REQ),
    (PolicyLayerTier::SystemRequirements, SYS_REQ),
    (PolicyLayerTier::SystemManaged, SYS_MANAGED),
];
/// Every user-owned tier: both are the server-synced `$GROK_HOME` files.
const USER_TIERS: [(PolicyLayerTier, &str); 2] = [
    (PolicyLayerTier::UserRequirements, USER_REQ),
    (PolicyLayerTier::UserManaged, USER_MANAGED),
];

// ── shared builders ──────────────────────────────────────────────

/// HTTP server named `name` at `url`.
fn hs(name: &str, url: &str) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Http(
        agent_client_protocol::McpServerHttp::new(name, url).headers(vec![]),
    )
}

/// SSE server named `name` at `url`.
fn se(name: &str, url: &str) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Sse(
        agent_client_protocol::McpServerSse::new(name, url).headers(vec![]),
    )
}

/// Stdio server named `name` running `command` (no args).
fn ss(name: &str, command: &str) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio::new(
        name,
        std::path::PathBuf::from(command),
    ))
}

/// Stdio server with args.
fn sa(name: &str, command: &str, args: &[&str]) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Stdio(
        agent_client_protocol::McpServerStdio::new(name, std::path::PathBuf::from(command))
            .args(args.iter().map(|s| s.to_string()).collect()),
    )
}

/// Anonymous HTTP server for URL-only rows (the name never matters there).
fn h(url: &str) -> agent_client_protocol::McpServer {
    hs("t", url)
}

fn allowlist_from(json: serde_json::Value) -> McpServerPolicy {
    parse_managed_settings_json(&json, std::path::Path::new(CLAUDE_PATH)).mcp_allowlist
}

/// Some source of `policy` is a full lockdown (what `grok inspect` lists).
fn has_lockdown_source(policy: &McpServerPolicy) -> bool {
    policy.sources.iter().any(McpServerAllowlist::is_lockdown)
}

/// Policy with URL-pattern entries in `allowedMcpServers`.
fn allow_urls(patterns: &[&str]) -> McpServerPolicy {
    let entries: Vec<_> = patterns
        .iter()
        .map(|p| serde_json::json!({ "serverUrl": p }))
        .collect();
    allowlist_from(serde_json::json!({ "allowedMcpServers": entries }))
}

/// Policy with URL-pattern entries in `deniedMcpServers`.
fn deny_urls(patterns: &[&str]) -> McpServerPolicy {
    let entries: Vec<_> = patterns
        .iter()
        .map(|p| serde_json::json!({ "serverUrl": p }))
        .collect();
    allowlist_from(serde_json::json!({ "deniedMcpServers": entries }))
}

/// Assert `is_server_allowed` (Foreign) for each `(server, want, why)` row.
fn check_allowed(
    group: &str,
    al: &McpServerPolicy,
    rows: &[(agent_client_protocol::McpServer, bool, &str)],
) {
    for (server, want, label) in rows {
        assert_eq!(
            al.is_server_allowed(server, FOREIGN),
            *want,
            "{group} / {label}"
        );
    }
}

/// Assert `is_server_denied` (Foreign) for each `(server, want, why)` row.
fn check_denied(
    group: &str,
    al: &McpServerPolicy,
    rows: &[(agent_client_protocol::McpServer, bool, &str)],
) {
    for (server, want, label) in rows {
        assert_eq!(
            al.is_server_denied(server, FOREIGN),
            *want,
            "{group} / {label}"
        );
    }
}

/// Allow-table group: build an allow-URL policy and pin `is_server_allowed`
/// for each `(url, granted, why)` row.
fn allow_rows(group: &str, patterns: &[&str], rows: &[(&str, bool, &str)]) {
    let al = allow_urls(patterns);
    for (url, want, label) in rows {
        assert_eq!(
            al.is_server_allowed(&h(url), FOREIGN),
            *want,
            "{group} / {label}: {url}"
        );
    }
}

/// Deny-table group: build a deny-URL policy and pin `is_server_denied` for
/// each `(url, denied, why)` row, plus the allow side (for a deny-only
/// policy, allowed is exactly the complement of denied).
fn deny_rows(group: &str, patterns: &[&str], rows: &[(&str, bool, &str)]) {
    let al = deny_urls(patterns);
    for (url, denied, label) in rows {
        assert_eq!(
            al.is_server_denied(&h(url), FOREIGN),
            *denied,
            "{group} / {label}: {url}"
        );
        assert_eq!(
            al.is_server_allowed(&h(url), FOREIGN),
            !*denied,
            "{group} / {label} (allow side): {url}"
        );
    }
}

// ── managed-settings.json end-to-end tests ───────────────────────

#[test]
fn parse_managed_settings_json_end_to_end() {
    let json = serde_json::json!({
        "env": {
            "DISABLE_TELEMETRY": 1,
            "DISABLE_FEEDBACK_COMMAND": 1
        },
        "permissions": {
            "disableBypassPermissionsMode": "disable",
            "deny": ["Read(**/.env*)"]
        },
        "allowedMcpServers": [
            { "serverUrl": "https://*.example.com/*" },
            { "command": "npx" }
        ],
        "strictKnownMarketplaces": [
            { "source": "git", "url": "git@github.enterprise.example:ACME/repo.git" }
        ]
    });
    let path = std::path::Path::new(CLAUDE_PATH);
    let ms = parse_managed_settings_json(&json, path);

    assert_eq!(ms.features.disable_telemetry, Some(true));
    assert_eq!(ms.features.disable_feedback, Some(true));
    assert_eq!(ms.features.disable_yolo, Some(true));

    assert!(ms.mcp_allowlist.is_restricted());
    check_allowed(
        "end-to-end MCP allowlist",
        &ms.mcp_allowlist,
        &[
            (h("https://api.example.com/mcp"), true, "allowlisted URL"),
            (h("https://evil.com/mcp"), false, "unlisted URL"),
            (
                h("https://evil.com/?x=https://fake.example.com/y"),
                false,
                "embedded URL in a query string is no bypass",
            ),
            (ss("t", "npx"), true, "allowlisted command"),
            (ss("t", "node"), false, "unlisted command"),
        ],
    );

    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(
        ms.marketplace_allowlist
            .is_url_allowed("git@github.enterprise.example:ACME/repo.git", FOREIGN)
    );
    assert!(
        !ms.marketplace_allowlist
            .is_url_allowed("git@evil.com:org/repo.git", FOREIGN)
    );

    assert_eq!(ms.permissions.len(), 1);
    assert_eq!(
        ms.permissions
            .first()
            .unwrap_or_else(|| panic!("expected permission"))
            .value
            .action,
        RuleAction::Deny
    );
}

#[test]
fn mcp_denylist_classifies_denied_servers() {
    let al = allowlist_from(serde_json::json!({
        "allowedMcpServers": [ { "serverUrl": "https://ok.example.com/*" } ],
        "deniedMcpServers": [ { "serverUrl": "https://blocked.example.com/*" } ]
    }));
    let denied = || hs("blocked", "https://blocked.example.com/mcp");
    let unlisted = || hs("other", "https://other.com/mcp");
    check_allowed(
        "deny vs missing-allowlist classification",
        &al,
        &[
            (denied(), false, "denied server is blocked"),
            (unlisted(), false, "unlisted server is blocked"),
        ],
    );
    check_denied(
        "deny vs missing-allowlist classification",
        &al,
        &[
            (denied(), true, "denied server classifies as denied"),
            (unlisted(), false, "blocks as missing-allowlist, not a deny"),
        ],
    );
}

// ── URL matcher semantics (tables) ───────────────────────────────

/// Pins the ALLOW URL table: scheme, host, port, and path match separately,
/// canonicalized like the WHATWG parser; anything unparseable grants nothing.
#[test]
#[rustfmt::skip]
fn allow_url_matcher_semantics() {
    allow_rows("host wildcard *.corp.com", &["https://*.corp.com/*"], &[
        ("https://mcp.corp.com/sse", true, "subdomain host is granted"),
        ("https://a.corp.com/x/y", true, "nested path under a subdomain"),
        ("https://sub.sub.corp.com/x", true, "any subdomain depth matches"),
        ("https://corp.com/x", false, "`*.` needs a subdomain label: no apex grant"),
        ("https://evil.example/a.corp.com/x", false, "`*` must not span `evil.example/a`"),
        ("https://a.corp.com@evil.example/x", false, "userinfo decoy: connect host is evil"),
        ("https://evil.example\\@a.corp.com/x", false, "`\\` ends the authority like connect"),
        ("http://mcp.corp.com/sse", false, "scheme stays literal"),
        ("https://mcp.corp.com:8080/sse", false, "port stays literal"),
    ]);

    allow_rows("path glob /* and scheme case", &["https://mcp.corp.com/*"], &[
        ("https://mcp.corp.com/", true, "trailing-slash spelling"),
        ("https://mcp.corp.com", true, "path-less spelling — same request as `/`"),
        ("https://mcp.corp.com/a/b", true, "nested path"),
        ("HTTPS://mcp.corp.com/mcp", true, "uppercase HTTPS:// parses to the same scheme"),
        ("mcp.corp.com/mcp", false, "relative (unparseable) URL earns no grant"),
    ]);

    // Claude: "A pattern with no path matches any path". `https://host/` is
    // WHATWG-equal to the path-less spelling, so it means the same thing.
    allow_rows("no pattern path = any path (Claude parity)", &["https://mcp.corp.com"], &[
        ("https://mcp.corp.com", true, "root, path-less spelling"),
        ("https://mcp.corp.com/", true, "root, trailing-slash spelling"),
        ("https://mcp.corp.com/sse", true, "one segment"),
        ("https://mcp.corp.com/a/b/c", true, "nested path"),
        ("https://mcp.corp.com:8080/sse", false, "port still literal"),
    ]);
    allow_rows("explicit trailing slash = any path too", &["https://mcp.corp.com/"], &[
        ("https://mcp.corp.com/", true, "root"),
        ("https://mcp.corp.com/mcp/tool", true, "nested path"),
    ]);
    // Every spelling whose CANONICAL path is `/` grants every path.
    for pattern in [
        "https://mcp.corp.com/.",
        "https://mcp.corp.com/mcp/..",
        "https://mcp.corp.com/./",
    ] {
        allow_rows(pattern, &[pattern], &[
            ("https://mcp.corp.com/admin/x", true, "canonical-root pattern grants any path"),
        ]);
    }
    allow_rows(
        "path-less pattern with every other component shape",
        &[
            "*://any.corp.com",
            "https://port.corp.com:8443",
            "https://*.glob.corp.com",
            "https://[2001:db8::1]",
        ],
        &[
            ("http://any.corp.com/a/b", true, "`*://` scheme"),
            ("https://port.corp.com:8443/a/b", true, "explicit port"),
            ("https://sub.glob.corp.com/a/b", true, "host glob"),
            ("https://[2001:db8:0:0:0:0:0:1]/a/b", true, "bracketed IPv6"),
        ],
    );
    check_allowed(
        "SSE transport routes through the same URL matcher",
        &allow_urls(&["https://mcp.corp.com"]),
        &[(se("t", "https://mcp.corp.com/sse"), true, "path-less pattern grants an SSE server")],
    );
    allow_rows("a pattern WITH a path stays path-scoped", &["https://mcp.corp.com/mcp"], &[
        ("https://mcp.corp.com/mcp", true, "exact path"),
        ("https://mcp.corp.com/mcp?q=1#frag", true, "query and fragment are not part of the path"),
        ("https://mcp.corp.com/", false, "root is outside"),
        ("https://mcp.corp.com/mcp/tool", false, "nested path is outside"),
        ("https://mcp.corp.com/sse", false, "sibling path is outside"),
    ]);

    // Only a bare `*` is the wildcard; the rest stays literal. Bounded to schemes the transport can connect to, so a future transport does not inherit the grant.
    allow_rows("`*://` matches the supported remote schemes", &["*://mcp.corp.com/*"], &[
        ("https://mcp.corp.com/sse", true, "https"),
        ("http://mcp.corp.com/sse", true, "http"),
        ("ftp://mcp.corp.com/x", false, "ftp parses with a host but is not a supported scheme"),
        ("file://mcp.corp.com/x", false, "file parses with a host but is not a supported scheme"),
        ("https://evil.example/mcp.corp.com/x", false, "host still bounded under `*://`"),
    ]);
    allow_rows("`*://` with an explicit default port", &["*://mcp.corp.com:443/*"], &[
        ("https://mcp.corp.com/sse", true, "https elides :443"),
        ("http://mcp.corp.com/sse", false, "http's default port is 80, not 443"),
    ]);
    // Same shapes the parse lint reports as unmatchable.
    allow_rows(
        "partial-glob or empty scheme never matches",
        &["http*://mcp.corp.com/*", "://mcp.corp.com/*"],
        &[
            ("https://mcp.corp.com/sse", false, "not https"),
            ("http://mcp.corp.com/sse", false, "not http either"),
        ],
    );

    allow_rows(
        "dot segments resolve; allow paths are case-sensitive",
        &["https://corp.com/mcp/*"],
        &[
            ("https://corp.com/mcp/../admin", false, "`..` connects to /admin — outside"),
            ("https://corp.com/mcp/%2e%2e/admin", false, "`%2e%2e` resolves like connect time"),
            ("https://corp.com/mcp/.%2e/admin", false, "mixed literal+escaped dot resolves too"),
            ("https://corp.com/mcp/./tool", true, "benign `/./` lands inside the grant"),
            ("https://corp.com/mcp/x", true, "matching-case path is granted"),
            ("https://corp.com/MCP/x", false, "a different-cased path is a different resource"),
            ("https://CORP.com/mcp/x", true, "host case stays irrelevant"),
        ],
    );

    allow_rows("explicit scheme-default port", &["https://mcp.corp.com:443/*"], &[
        ("https://mcp.corp.com/mcp", true, "port-less spelling names the same target"),
        ("https://mcp.corp.com:443/mcp", true, "explicit :443 spelling"),
        ("https://mcp.corp.com:8080/mcp", false, "non-default port stays literal"),
    ]);
    allow_rows("trailing host wildcard, no pattern port", &["https://mcp.corp.*/*"], &[
        ("https://mcp.corp.com/mcp", true, "host glob matches"),
        ("https://mcp.corp.com:8080/mcp", false, "host wildcard cannot absorb a port"),
    ]);
    allow_rows(
        "trailing host wildcard with explicit default port",
        &["https://mcp.corp.*:443/*"],
        &[
            ("https://mcp.corp.com/mcp", true, "port-less spelling"),
            ("https://mcp.corp.com:443/mcp", true, "explicit default port"),
            ("https://mcp.corp.com:8080/mcp", false, "non-default port"),
        ],
    );

    allow_rows("Unicode pattern host and path", &["https://bücher.example/café/*"], &[
        ("https://bücher.example/café/tool", true, "Unicode runtime spelling"),
        ("https://xn--bcher-kva.example/caf%C3%A9/tool", true, "punycode connect spelling"),
        ("https://xn--bcher-kva.example/other/tool", false, "different path stays out"),
    ]);
    // `u` + combining diaeresis — the NFD spelling of `ü`; IDNA maps both
    // spellings to the same punycode label.
    allow_rows("NFD-encoded Unicode pattern host", &["https://bu\u{0308}cher.example/*"], &[
        ("https://b\u{00fc}cher.example/x", true, "NFC runtime spelling of the same host"),
        ("https://xn--bcher-kva.example/x", true, "punycode runtime spelling"),
    ]);

    allow_rows("bracketed IPv6 compares by parsed address", &["https://[2001:db8::1]/*"], &[
        ("https://[2001:db8::1]/mcp", true, "literal spelling"),
        ("https://[2001:db8:0:0:0:0:0:1]/mcp", true, "expanded spelling of the address"),
        ("https://[2001:db8::2]/mcp", false, "different address"),
        ("https://[2001:db8::1]:8080/mcp", false, "non-default port stays literal"),
    ]);
    allow_rows("leading-zero IPv6 PATTERN spelling", &["https://[2001:0db8::1]/*"], &[
        ("https://[2001:db8::1]/mcp", true, "still compares by parsed address"),
    ]);
    allow_rows("IPv6 address ending in a default-port hextet", &["https://[2001:db8::443]/*"], &[
        ("https://[2001:db8::443]/mcp", true, "no port to strip: address stays intact"),
        ("https://[2001:db8::443]:443/mcp", true, "explicit default port on the address"),
    ]);
    allow_rows("zero-padded default port on IPv6", &["https://[::1]:0443/*"], &[
        ("https://[::1]/mcp", true, "strips numerically like the parser"),
    ]);
    allow_rows("zero-padded non-default port on IPv6", &["https://[::1]:08080/*"], &[
        ("https://[::1]:8080/mcp", true, "both-explicit ports compare numerically"),
        ("https://[::1]/mcp", false, "port-less spelling is a different target"),
    ]);
    allow_rows("host wildcard grants IP runtimes too", &["https://*/*"], &[
        ("https://[::1]/mcp", true, "IPv6 runtime"),
        ("https://10.0.0.1/mcp", true, "IPv4 runtime"),
    ]);

    allow_rows("IPv6 zone-id URL fails closed", &["https://[fe80::1]/*"], &[
        ("https://[fe80::1%25eth0]/mcp", false, "zone-id URL is unparseable — no grant"),
    ]);
    allow_rows("IPv6 zone-id PATTERN can never match", &["https://[fe80::1%eth0]/*"], &[
        ("https://[fe80::1]/mcp", false, "not the plain address"),
        ("https://[fe80::1%25eth0]/mcp", false, "not even the zone-id URL (unparseable)"),
    ]);

    allow_rows(
        "leading non-address [..] is a glob character class",
        &["https://[ab]host.corp.com/*"],
        &[
            ("https://ahost.corp.com/mcp", true, "class member a"),
            ("https://bhost.corp.com/mcp", true, "class member b"),
            ("https://chost.corp.com/mcp", false, "non-member c"),
        ],
    );

    allow_rows("glob in the pattern port", &["https://mcp.corp.com:4*/*"], &[
        ("https://mcp.corp.com:443/mcp", false, "ports are literal: a glob grants nothing"),
        ("https://mcp.corp.com:4000/mcp", false, "not even other 4xxx ports"),
    ]);
    allow_rows("zero-padded pattern port on a domain host", &["https://mcp.corp.com:0443/*"], &[
        ("https://mcp.corp.com:443/mcp", true, "leading zeros compare numerically"),
    ]);

    allow_rows(
        "pattern userinfo drops like the connect-time parser",
        &["https://token@mcp.corp.com/*", "https://token@[::1]/*"],
        &[
            ("https://mcp.corp.com/x", true, "copied token@domain still grants its host"),
            ("https://[::1]/x", true, "token@[IPv6] still reads as an address"),
        ],
    );

    allow_rows(
        "escape hex case never splits one connect target",
        &["https://h.example/caf%c3%a9/*"],
        &[
            ("https://h.example/caf%C3%A9/x", true, "uppercase-hex runtime spelling"),
            ("https://h.example/caf%c3%a9/x", true, "lowercase-hex runtime spelling"),
            ("https://h.example/café/x", true, "raw Unicode runtime spelling"),
        ],
    );

    allow_rows("broken allow HOST glob grants nothing", &["https://host[x.corp.com/*"], &[
        ("https://hostx.corp.com/mcp", false, "unclosed character class fails closed"),
        ("https://host.corp.com/mcp", false, "no other corp host either"),
    ]);

    allow_rows(
        "double-trailing-dot host spelling",
        &["https://evil.com/*"],
        &[(
            "https://evil.com../x",
            true,
            "parses; trailing dots trim to the same connect host — granted, not a bypass",
        )],
    );
}

/// Pins deny URL matching: host-normalized and scheme/port-agnostic, asymmetric with allow because a deny must never fail open.
/// Alternate IP spellings, rejected URLs, and broken path globs still deny. Allow is the exact complement.
#[test]
#[rustfmt::skip]
fn deny_url_matcher_semantics() {
    // Regression for a managed-gateway deny pattern: the four bypass
    // spellings previously fell through the literal glob (fail-open).
    deny_rows("host-normalized, scheme/port-agnostic", &["https://mcp-gateway.example.net/*"], &[
        ("https://mcp-gateway.example.net:443/mcp", true, "explicit default port"),
        ("http://mcp-gateway.example.net/mcp", true, "scheme swap"),
        ("https://mcp-gateway.example.net", true, "path-less host"),
        ("https://mcp-gateway.example.net./mcp", true, "trailing-dot FQDN"),
        ("https://mcp-gateway.example.net/mcp", true, "baseline spelling"),
        ("https://mcp-gateway.example.net/mcp?x=y", true, "query strips — no bypass"),
        ("https://MCP-GATEWAY.example.net/mcp", true, "host case folds"),
        ("https://mcp-gateway.staging.example.net/mcp", false, "deny is host-scoped"),
        ("https://other.example.com/mcp", false, "unrelated host stays"),
    ]);

    // Deny drops the pattern's scheme and port before matching, so the shapes
    // the ALLOW lint rejects (no scheme, glob port) still block the host.
    deny_rows("scheme-less and glob-port deny patterns block on any scheme and port", &[
        "blocked.example.com/*",
        "http://blocked.example.net:*/*",
    ], &[
        ("https://blocked.example.com:8443/mcp", true, "scheme-less pattern, https + port"),
        ("http://blocked.example.com/mcp", true, "scheme-less pattern, http"),
        ("https://blocked.example.net/mcp", true, "glob-port pattern, https default port"),
        ("http://blocked.example.net:8080/mcp", true, "glob-port pattern, explicit port"),
        ("https://other.example.com/mcp", false, "unrelated host stays"),
    ]);

    deny_rows("path-scoped deny survives spelling dodges", &["https://corp.com/admin/*"], &[
        ("https://corp.com/mcp/../admin/x", true, "`..` connects under /admin"),
        ("https://corp.com/mcp/%2e%2e/admin/x", true, "`%2e%2e` resolves the same way"),
        ("https://corp.com/admin//../x", true, "`//../x` pops only the empty segment"),
        ("https://corp.com/%61dmin/x", true, "unreserved escape decodes (`%61` = `a`)"),
        ("https://corp.com/a%2Fdmin/x", false, "%2F is not a separator; stays encoded"),
        // Servers routinely normalize `//admin` to `/admin`, so empty
        // segments collapse before deny matching (runtime and pattern side).
        ("https://corp.com//admin/x", true, "double-slash prefix can't dodge the path glob"),
        ("https://corp.com/admin///x", true, "interior empty segments collapse too"),
    ]);
    deny_rows("empty segments collapse on the pattern side", &["https://corp.com//admin/*"], &[
        ("https://corp.com/admin/x", true, "pattern `//admin` scopes to `/admin`"),
        ("https://corp.com//admin/x", true, "and still catches the double-slash spelling"),
    ]);

    deny_rows("backslash authority terminator", &["https://evil.example/*"], &[
        ("https://evil.example\\@a.corp.com/x", true, "connect host is evil.example"),
    ]);

    deny_rows("IPv4 deny matches every connect-equal spelling", &["http://169.254.169.254/*"], &[
        ("http://0xa9fea9fe/latest/meta-data", true, "hex spelling"),
        ("http://[::ffff:169.254.169.254]/latest/meta-data", true, "IPv4-mapped IPv6"),
        ("http://[::ffff:a9fe:a9fe]/latest/meta-data", true, "hex-group IPv4-mapped"),
        ("http://[2001:db8::1]/mcp", false, "a genuine IPv6 address is not an alias"),
    ]);
    deny_rows("localhost IPv4 deny", &["http://127.0.0.1/*"], &[
        ("http://127.1/mcp", true, "shortened spelling"),
        ("http://2130706433/mcp", true, "decimal spelling"),
        ("http://10.0.0.1/mcp", false, "different address untouched"),
    ]);
    deny_rows("IPv4-mapped deny entry (mirror direction)", &["http://[::ffff:127.0.0.1]/*"], &[
        ("http://127.0.0.1/mcp", true, "blocks the plain IPv4 spelling"),
    ]);

    deny_rows("Unicode deny host", &["https://bücher.example/*"], &[
        ("https://bücher.example/mcp", true, "Unicode runtime spelling"),
        ("https://xn--bcher-kva.example/mcp", true, "punycode runtime spelling"),
        ("https://XN--BCHER-KVA.example/mcp", true, "mixed-case punycode folds"),
        ("https://other.example/mcp", false, "unrelated host untouched"),
    ]);
    deny_rows("wildcard label around Unicode labels", &["https://*.bücher.example/*"], &[
        ("https://mcp.xn--bcher-kva.example/x", true, "glob label stays a glob"),
    ]);
    deny_rows(
        "host wildcard *.corp.com (deny side)",
        &["https://*.corp.com/*"],
        &[(
            "https://corp.com/mcp",
            false,
            "as-is: `*.` needs a subdomain label, so the apex is NOT denied — an apex deny \
             must be written without the `*.`",
        )],
    );

    deny_rows("unparseable runtime URLs are denied outright", &["https://blocked.example.com/*"], &[
        ("mcp.corp.com/mcp", true, "relative URL fails closed"),
    ]);
    deny_rows("double-trailing-dot host spelling (deny side)", &["https://evil.com/*"], &[
        ("https://evil.com../x", true, "trims to the denied host — no dodge"),
    ]);
    deny_rows("IPv6 zone-id URL (deny side)", &["https://[fe80::1]/*"], &[
        ("https://[fe80::1%25eth0]/mcp", true, "unparseable — denied outright"),
    ]);
    // A zone-id PATTERN is neither a valid address nor a compiling glob: it could
    // never match a parseable URL, so the deny key fails closed (lockdown, no deny match).
    let zone_id = deny_urls(&["https://[fe80::1%eth0]/*"]);
    assert!(has_lockdown_source(&zone_id));
    for url in ["https://[fe80::1]/mcp", "https://[fe80::1%25eth0]/mcp"] {
        assert!(!zone_id.is_server_denied(&h(url), FOREIGN), "{url}");
        assert!(!zone_id.is_server_allowed(&h(url), FOREIGN), "{url}");
    }

    deny_rows("IPv6 deny matches every spelling of the address", &["https://[2001:db8::1]/*"], &[
        ("https://[2001:db8::1]/mcp", true, "literal spelling"),
        ("http://[2001:db8::1]:8080/mcp", true, "any scheme/port variant still denied"),
        ("https://[2001:0db8::1]/mcp", true, "leading-zero spelling"),
        ("https://[2001:db8:0:0:0:0:0:1]/mcp", true, "expanded spelling"),
        ("https://[2001:db8::2]/mcp", false, "different address untouched"),
    ]);

    deny_rows("trailing-slash deny blocks the whole host", &["https://blocked.example.com/"], &[
        ("https://blocked.example.com/mcp", true, "deep path"),
        ("https://blocked.example.com/", true, "root path"),
        ("https://ok.example.com/mcp", false, "other host untouched"),
    ]);
    // Every spelling whose CANONICAL path is `/` denies the whole host.
    for pattern in [
        "https://blocked.example.com/.",
        "https://blocked.example.com/mcp/..",
        "https://blocked.example.com/./",
    ] {
        deny_rows(
            pattern,
            &[pattern],
            &[(
                "https://blocked.example.com/mcp",
                true,
                "canonical-root pattern denies host",
            )],
        );
    }

    deny_rows("leading non-address [..] globs the host", &["https://[ab]evil.example/*"], &[
        ("https://aevil.example/x", true, "class member a"),
        ("https://bevil.example/x", true, "class member b"),
        ("https://cevil.example/x", false, "non-member c"),
    ]);

    deny_rows("percent-encoded pattern host decodes", &["https://%61dmin.example/*"], &[
        ("https://admin.example/x", true, "blocks its real host"),
        ("https://badmin.example/x", false, "not a substring host"),
    ]);

    deny_rows("pattern path dot segments resolve", &["https://h.example/x/../admin/*"], &[
        ("https://h.example/admin/secret", true, "scopes to /admin/*"),
        ("https://h.example/x/admin/secret", false, "not the unresolved spelling"),
    ]);
    deny_rows(
        "percent-encoded unreserved bytes in the PATTERN path",
        &["https://h.example/%61dmin/*"],
        &[(
            "https://h.example/admin/x",
            true,
            "matches the plain runtime path",
        )],
    );

    deny_rows("invalid deny PATH glob fails closed", &["https://h.example/admin[x/*"], &[
        ("https://h.example/admin[x/y", true, "host matched + broken glob: deny the host"),
        ("https://other.example/admin[x/y", false, "different host: entry does not apply"),
        ("https://host[x/y", true, "unparseable URL denied on the unparseable branch"),
    ]);
    // A broken deny HOST glob matches no parseable host: the key fails closed
    // (lockdown) instead of an entry that only "denies" unparseable URLs.
    let broken_host = deny_urls(&["https://host[x/*"]);
    assert!(has_lockdown_source(&broken_host));
    for url in ["https://host[x/y", "https://other.example/admin"] {
        assert!(!broken_host.is_server_denied(&h(url), FOREIGN), "{url}");
        assert!(!broken_host.is_server_allowed(&h(url), FOREIGN), "{url}");
    }

    deny_rows("unbracketed IPv6 deny pattern", &["https://2001:db8::1/*"], &[
        ("https://[2001:db8::1]/mcp", true, "denies the bracketed connect spelling"),
        ("https://0.0.7.209/mcp", false, "not misread as IPv4 by a first-`:` split"),
    ]);
    deny_rows("unbracketed IPv6 with a trailing decimal group", &["https://2001:db8::1:443/*"], &[
        ("https://[2001:db8::1]/mcp", true, ":443 is a PORT — denies host 2001:db8::1"),
        ("https://[2001:db8::1]:8080/mcp", true, "deny is port-agnostic"),
        ("https://[2001:db8::1:443]/mcp", false, "the different address is untouched"),
    ]);
    deny_rows(
        "unbracketed IPv6 with a non-decimal final group",
        &["https://2001:db8::1:ffff/*"],
        &[(
            "https://[2001:db8::1:ffff]/mcp",
            true,
            ":ffff is a hextet, not a port — the whole string is the address",
        )],
    );
}

// ── serverName / serverCommand / lockdown policy (tables) ────────

/// Pins allow-dimension union at `is_server_allowed`: a command allowlist never covers HTTP and vice versa; deny beats allow.
/// `serverCommand` is exact argv, never a prefix; `allowManagedMcpServersOnly` requires a positive grant.
#[test]
#[rustfmt::skip]
fn name_argv_and_lockdown_semantics() {
    check_allowed(
        "command-only allowlist",
        &allowlist_from(serde_json::json!({
            "allowedMcpServers": [ { "command": "npx" } ]
        })),
        &[
            (h("https://any.example/mcp"), true, "never restricts HTTP"),
            (ss("ok", "npx"), true, "listed command runs"),
            (ss("no", "other"), false, "unlisted command blocked"),
    ]);
    check_allowed("URL-only allowlist", &allow_urls(&["https://ok.example/*"]), &[
        (ss("s", "anything"), true, "never restricts stdio"),
        (hs("ok", "https://ok.example/mcp"), true, "listed URL runs"),
        (h("https://other.example/x"), false, "unlisted URL blocked"),
    ]);

    // Enterprise MDM-shaped managed policy: pure blocklist, no allowlist.
    let deny_only = allowlist_from(serde_json::json!({
        "deniedMcpServers": [
            { "serverUrl": "https://mcp-gateway.example.net/*" },
            { "command": "npx" }
        ]
    }));
    assert!(
        deny_only.is_restricted(),
        "deny-only must still count as restricted so enforcement engages"
    );
    check_allowed("deny-only policy", &deny_only, &[
        (h("https://mcp-gateway.example.net/mcp"), false, "denied URL blocked"),
        (h("https://other.com/mcp"), true, "empty allowlist still allows the un-denied rest"),
        (ss("n", "npx"), false, "denied command blocked"),
        (ss("n", "node"), true, "other commands run"),
        (ss("n", "/usr/local/bin/npx"), true, "command deny is exact-string, never a suffix"),
    ]);
    check_allowed("URL-only denylist", &deny_urls(&["https://blocked.com/*"]), &[
        (ss("s", "anything"), true, "never restricts stdio"),
        (h("https://blocked.com/mcp"), false, "while still denying its URL"),
    ]);

    let deny_beats_allow = allowlist_from(serde_json::json!({
        "allowedMcpServers": [
            { "serverUrl": "https://*.example.com/*" },
            { "command": "npx" }
        ],
        "deniedMcpServers": [
            { "serverUrl": "https://blocked.example.com/*" },
            { "command": "npx" }
        ]
    }));
    check_allowed("deny beats allow (URL and command)", &deny_beats_allow, &[
        (h("https://ok.example.com/mcp"), true, "allowlisted URL runs"),
        (h("https://blocked.example.com/mcp"), false, "deny wins over the URL allow"),
        (ss("n", "npx"), false, "deny wins over the command allow"),
    ]);
    check_denied("deny beats allow (URL and command)", &deny_beats_allow, &[
        (h("https://blocked.example.com/mcp"), true, "URL deny classifies as denied"),
        (ss("n", "npx"), true, "command deny classifies as denied"),
    ]);

    let deny_name = allowlist_from(serde_json::json!({
        "deniedMcpServers": [ { "serverName": "foo" } ]
    }));
    assert!(deny_name.is_restricted(), "a name denylist restricts");
    check_denied("deny serverName foo", &deny_name, &[
        (hs("foo", "https://x.example/mcp"), true, "bare runtime name"),
        (hs("grok_com_foo", "https://x.example/mcp"), true, "managed-prefixed runtime name"),
        (ss("grok_com_foo", "npx"), true, "name match is transport-agnostic"),
        (hs("foobar", "https://x.example/mcp"), false, "exact after strip, never substring"),
        (hs("grok_com_foobar", "https://x.example/mcp"), false, "prefixed near-miss"),
        (hs("barfoo", "https://x.example/mcp"), false, "suffix near-miss"),
        (hs("bar", "https://x.example/mcp"), false, "unrelated name"),
    ]);
    // Allow side: deny blocks, everything not denied (near-misses pinned on
    // the deny side above) stays allowed.
    check_allowed("deny serverName foo", &deny_name, &[
        (hs("foo", "https://x.example/mcp"), false, "denied bare name"),
        (hs("grok_com_foo", "https://x.example/mcp"), false, "denied managed name"),
        (ss("grok_com_foo", "npx"), false, "denied on any transport"),
        (hs("foobar", "https://x.example/mcp"), true, "unrelated names remain allowed"),
    ]);

    let allow_name = allowlist_from(serde_json::json!({
        "allowedMcpServers": [ { "serverName": "foo" } ]
    }));
    assert!(allow_name.is_restricted(), "a name allowlist restricts");
    check_allowed("allow serverName foo", &allow_name, &[
        (hs("foo", "https://anything.example/x"), true, "named server allowed on any URL"),
        (hs("grok_com_foo", "https://evil.example/x"), true, "managed spelling, any URL"),
        (ss("grok_com_foo", "/usr/bin/whatever"), true, "allowed on any transport"),
        (hs("bar", "https://anything.example/x"), false, "unlisted name blocked"),
        (ss("bar", "npx"), false, "unlisted stdio blocked too"),
    ]);
    check_denied(
        "allow serverName foo",
        &allow_name,
        &[(
            hs("bar", "https://anything.example/x"),
            false,
            "blocked as missing, not denied",
        )],
    );

    let name_both = allowlist_from(serde_json::json!({
        "allowedMcpServers": [ { "serverName": "foo" } ],
        "deniedMcpServers":  [ { "serverName": "foo" } ]
    }));
    check_denied("serverName deny beats allow", &name_both, &[
        (hs("foo", "https://foo.example/x"), true, "bare spelling"),
        (hs("grok_com_foo", "https://foo.example/x"), true, "managed spelling"),
    ]);
    check_allowed("serverName deny beats allow", &name_both, &[
        (hs("foo", "https://foo.example/x"), false, "deny wins for the same name"),
        (hs("grok_com_foo", "https://foo.example/x"), false, "deny wins, managed spelling"),
    ]);

    check_denied(
        "prefixed policy entry vs bare runtime (vice versa)",
        &allowlist_from(serde_json::json!({
            "deniedMcpServers": [ { "serverName": "grok_com_foo" } ]
        })),
        &[
            (hs("foo", "https://x.example/mcp"), true, "bare runtime matches after strip"),
            (hs("grok_com_foo", "https://x.example/mcp"), true, "prefixed runtime matches"),
            (hs("foobar", "https://x.example/mcp"), false, "near-miss unrelated"),
            (hs("grok_com_foobar", "https://x.example/mcp"), false, "prefixed near-miss"),
    ]);

    check_allowed(
        "allow: URL ∪ name — either dimension grants",
        &allowlist_from(serde_json::json!({
            "allowedMcpServers": [
                { "serverUrl": "https://ok.example.com/*" },
                { "serverName": "foo" }
            ]
        })),
        &[
            (hs("bar", "https://ok.example.com/mcp"), true, "URL dimension grants"),
            (hs("foo", "https://evil.example.com/mcp"), true, "name dimension grants"),
            (hs("bar", "https://evil.example.com/mcp"), false, "neither dimension: blocked"),
    ]);
    let deny_dims = allowlist_from(serde_json::json!({
        "deniedMcpServers": [ { "command": "npx" }, { "serverName": "foo" } ]
    }));
    check_denied("deny: command and name deny independently", &deny_dims, &[
        (ss("unrelated", "npx"), true, "command deny hits regardless of name"),
        (ss("foo", "node"), true, "name deny hits regardless of command"),
        (ss("unrelated", "node"), false, "neither deny dimension"),
    ]);
    check_allowed(
        "deny: command and name deny independently",
        &deny_dims,
        &[(
            ss("unrelated", "node"),
            true,
            "undenied server stays allowed",
        )],
    );

    let argv_policy = allowlist_from(serde_json::json!({
        "allowedMcpServers": [
            { "serverCommand": ["npx", "@example-corp/ui-kit-mcp", "enterprise-webc"] }
        ],
        "deniedMcpServers": [
            { "serverCommand": ["npx", "evil-mcp"] }
        ]
    }));
    check_allowed("serverCommand arrays: exact argv", &argv_policy, &[
        (sa("k", "npx", &["@example-corp/ui-kit-mcp", "enterprise-webc"]), true, "exact argv"),
        (sa("k", "npx", &["@example-corp/ui-kit-mcp", "client-react"]), false, "other variant"),
        (sa("k", "npx", &["@example-corp/ui-kit-mcp", "enterprise-webc", "--evil"]), false,
            "prefix + extra trailing arg is not granted"),
        (sa("evil", "npx", &["evil-mcp"]), false, "denied argv is blocked"),
    ]);
    check_denied("serverCommand arrays: exact argv", &argv_policy, &[
        (sa("evil", "npx", &["evil-mcp"]), true, "deny argv classifies as denied"),
        (sa("bare", "npx", &[]), false, "a shorter argv is not denied"),
    ]);

    let lockdown = allowlist_from(serde_json::json!({
        "allowManagedMcpServersOnly": true,
        "allowedMcpServers": [
            { "serverUrl": "https://mcp.figma.com/*" },
            { "serverCommand": ["uvx", "mcp-grafana"] }
        ]
    }));
    assert!(lockdown.managed_only(FOREIGN));
    assert!(lockdown.is_restricted());
    check_allowed("managed-only lockdown requires a positive grant", &lockdown, &[
        (hs("figma", "https://mcp.figma.com/mcp"), true, "allowlisted URL runs"),
        (sa("grafana", "uvx", &["mcp-grafana"]), true, "allowlisted argv runs"),
        (ss("rogue", "python3"), false, "stdio can't ride a URL-only allowlist"),
        (h("https://evil.example.com/mcp"), false, "unlisted URL fails closed"),
    ]);

    let empty_lockdown = allowlist_from(serde_json::json!({ "allowManagedMcpServersOnly": true }));
    assert!(empty_lockdown.is_restricted());
    check_allowed("managed-only with an empty allowlist blocks everything", &empty_lockdown, &[
        (h("https://any.example.com/mcp"), false, "any HTTP blocked"),
        (ss("any", "npx"), false, "any stdio blocked"),
    ]);
}

/// ACP 0.10 cannot construct a fourth `McpServer` variant; `known = false`
/// is the fail-closed branch `is_server_denied` takes for one.
#[test]
fn deny_only_fails_closed_on_unrecognized_transport() {
    let unlisted = h("https://other.com/mcp");
    let deny_only = allowlist_from(serde_json::json!({
        "deniedMcpServers": [{ "command": "npx" }]
    }));
    let source = deny_only.sources.first().expect("deny-only parsed");
    assert!(mcp_transport_known(&unlisted));
    assert!(!source.is_server_denied(&unlisted));
    assert!(source.is_server_denied_known(&unlisted, false));

    let unrestricted = McpServerAllowlist::new(vec![], vec![], None);
    assert!(!unrestricted.is_server_denied_known(&unlisted, false));
}

/// Pins `serverName` identity: strip `grok_com_`, normalize, exact equality (empty never matches).
/// Legacy truncation applies only to a managed name at exactly the cap, so a long entry is not a prefix grant.
#[test]
#[rustfmt::skip]
fn mcp_name_matching_semantics() {
    let max_bare = MANAGED_MCP_NAME_MAX_CHARS - MANAGED_MCP_PREFIX.len();
    let long = "a".repeat(MANAGED_MCP_NAME_MAX_CHARS * 2);
    let long_runtime = format!(
        "{MANAGED_MCP_PREFIX}{}",
        long.get(..max_bare).unwrap_or(long.as_str())
    );
    let long_bare = "a".repeat(max_bare + 8);
    let entry = format!("{MANAGED_MCP_PREFIX}{long_bare}");
    let plain_decoy = format!(
        "{}-decoy",
        long_bare.get(..max_bare).unwrap_or(long_bare.as_str())
    );
    let at_cap = format!(
        "{MANAGED_MCP_PREFIX}{}",
        long_bare.get(..max_bare).unwrap_or(long_bare.as_str())
    );
    let over_cap_decoy = format!(
        "{MANAGED_MCP_PREFIX}{}-decoy",
        long_bare.get(..max_bare).unwrap_or(long_bare.as_str())
    );
    let under_cap_decoy = format!(
        "{MANAGED_MCP_PREFIX}{}",
        long_bare
            .get(..max_bare - 4)
            .unwrap_or(long_bare.as_str())
    );
    let corp = "corporate-approved-server-alpha-prod";
    let rows: Vec<(&str, &str, bool, &str)> = vec![
        ("foo", "foo", true, "exact bare match"),
        ("foo", "grok_com_foo", true, "bare entry vs managed runtime"),
        ("grok_com_foo", "foo", true, "managed entry vs bare runtime"),
        ("grok_com_foo", "grok_com_foo", true, "both managed"),
        ("foo", "foobar", false, "never substring"),
        ("foo", "grok_com_foobar", false, "never substring, managed"),
        ("foo", "barfoo", false, "never suffix"),
        ("foo", "bar", false, "different name"),
        ("", "foo", false, "empty entry never matches"),
        ("Slack", "grok_com_slack", true, "display case folds"),
        ("My Server", "grok_com_my_server", true, "spaces normalize to underscores"),
        ("grok_com_my_server", "My Server", true, "managed entry vs display runtime"),
        ("My Server", "my_server", true, "display entry vs local runtime"),
        ("SLACK", "slack", true, "all-caps entry"),
        ("My Server", "my_server_2", false, "normalized near-miss"),
        ("", "", false, "both empty never match"),
        ("grok_com_", "grok_com_anything", false, "bare prefix key is empty"),
        (&long, &long_runtime, true, "too-long entry matches its truncated runtime name"),
        (corp, "corporate-approved-server-alpha-anything", false, "long plain names don't collide"),
        (corp, corp, true, "exact long plain names still match"),
        (&entry, &plain_decoy, false, "long managed entry is no prefix grant over a plain decoy"),
        (&entry, &at_cap, true, "matches its own truncated managed runtime name"),
        (&entry, &over_cap_decoy, false, "over-cap managed decoy does not prefix-match"),
        (&entry, &under_cap_decoy, false, "under-cap managed decoy does not prefix-match"),
    ];
    for (pattern, name, want, label) in rows {
        assert_eq!(mcp_name_matches(pattern, name), want, "{label}");
    }

    // normalize_managed_name: the shared bare-name → runtime spelling.
    for (input, want, label) in [
        ("Slack", "slack", "lowercases"),
        ("My Server", "my_server", "spaces become underscores"),
        ("My  Server", "my__server", "double space keeps both underscores"),
        ("", "", "empty is identity"),
    ] {
        assert_eq!(normalize_managed_name(input), want, "{label}");
    }
}

// ── verdict API (matrix) ─────────────────────────────────────────

/// Assert one `mcp_verdict` outcome; `want` is the pinned `Display` string
/// of the block reason (`None` = allowed).
fn check_verdict(
    ms: &ManagedSettings,
    server: &agent_client_protocol::McpServer,
    subject: McpSubject,
    want: Option<&str>,
    label: &str,
) {
    match (ms.mcp_verdict(server, subject), want) {
        (McpVerdict::Allowed, None) => {}
        (McpVerdict::Blocked(reason), Some(want)) => {
            assert_eq!(reason.to_string(), want, "{label}");
        }
        (McpVerdict::Allowed, Some(want)) => panic!("{label}: allowed, wanted block {want:?}"),
        (McpVerdict::Blocked(reason), None) => {
            panic!("{label}: blocked with {reason}, wanted allowed")
        }
    }
}

/// Pins `mcp_verdict` precedence: deny, then lockdown missing-grant, then project pin; an allow grant exempts the pin.
/// Each reason's `Display` is the wire/UX payload.
#[test]
fn mcp_verdict_matrix() {
    let foreign = McpSubject {
        origin: FOREIGN,
        project_scoped: false,
    };
    let project = McpSubject {
        origin: FOREIGN,
        project_scoped: true,
    };

    // Deny + project pin, no allow list.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "deniedMcpServers": [ { "serverName": "blocked" } ],
            "enableAllProjectMcpServers": false
        }),
        std::path::Path::new(CLAUDE_PATH),
    );
    let deny_msg = format!("matches deniedMcpServers ({CLAUDE_PATH})");
    let pin_msg =
        format!("project MCP disabled by enableAllProjectMcpServers = false ({CLAUDE_PATH})");
    let url = "https://mcp.corp.com/mcp";
    check_verdict(&ms, &hs("blocked", url), foreign, Some(&deny_msg), "deny");
    check_verdict(
        &ms,
        &hs("blocked", url),
        project,
        Some(&deny_msg),
        "deny attributed before the project pin",
    );
    check_verdict(
        &ms,
        &hs("ok", url),
        foreign,
        None,
        "non-denied non-project runs",
    );
    check_verdict(
        &ms,
        &hs("ok", url),
        project,
        Some(&pin_msg),
        "project pin blocks an ungranted project server",
    );

    // Lockdown: deny still wins, then the missing grant.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "allowedMcpServers": [ { "serverUrl": "https://mcp.corp.com/*" } ],
            "deniedMcpServers": [ { "serverName": "blocked" } ],
            "allowManagedMcpServersOnly": true
        }),
        std::path::Path::new(CLAUDE_PATH),
    );
    let not_granted_msg = format!("not in allowedMcpServers ({CLAUDE_PATH})");
    check_verdict(
        &ms,
        &h(url),
        foreign,
        None,
        "granted URL runs under lockdown",
    );
    check_verdict(
        &ms,
        &h("https://other.example/mcp"),
        foreign,
        Some(&not_granted_msg),
        "ungranted URL is not-granted under lockdown",
    );
    check_verdict(
        &ms,
        &hs("blocked", "https://other.example/mcp"),
        foreign,
        Some(&deny_msg),
        "deny wins over not-granted",
    );

    // A full lockdown is its own reason: "not in allowedMcpServers" would send
    // the admin hunting for a key the file may not even contain.
    let lockdown_msg = format!("locked down by policy ({CLAUDE_PATH})");
    for (label, json) in [
        (
            "empty allowlist",
            serde_json::json!({ "allowedMcpServers": [] }),
        ),
        (
            "unenforceable deny list",
            serde_json::json!({ "deniedMcpServers": [ { "serverTypo": "x" } ] }),
        ),
    ] {
        let ms = parse_managed_settings_json(&json, std::path::Path::new(CLAUDE_PATH));
        check_verdict(&ms, &h(url), foreign, Some(&lockdown_msg), label);
    }

    // `mcp_project_pin_block` in isolation: only an ungranted project
    // subject under an active pin blocks; a grant exempts.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "allowedMcpServers": [ { "serverUrl": "https://mcp.corp.com/*" } ],
            "enableAllProjectMcpServers": false
        }),
        std::path::Path::new(CLAUDE_PATH),
    );
    let ungranted = ss("proj", "python3");
    assert_eq!(
        ms.mcp_project_pin_block(&ungranted, project)
            .map(|r| r.to_string()),
        Some(pin_msg),
        "pin blocks an ungranted project server"
    );
    assert_eq!(
        ms.mcp_project_pin_block(&h(url), project)
            .map(|r| r.to_string()),
        None,
        "an allow-entry grant exempts the pin"
    );
    assert_eq!(
        ms.mcp_project_pin_block(&ungranted, foreign)
            .map(|r| r.to_string()),
        None,
        "non-project subjects are never pinned"
    );
    assert_eq!(
        ManagedSettings::default()
            .mcp_project_pin_block(&ungranted, project)
            .map(|r| r.to_string()),
        None,
        "no active pin, no block"
    );
}

/// `McpBlockReason::Display` strings are wire/UX payloads (pager rows,
/// doctor details, enable errors) — pin them and `source()` directly.
#[test]
fn mcp_block_reason_display_and_source_are_pinned() {
    let src = PathBuf::from("/etc/grok/requirements.toml");
    let cases = [
        (
            McpBlockReason::Deny {
                source: src.clone(),
            },
            "matches deniedMcpServers (/etc/grok/requirements.toml)",
        ),
        (
            McpBlockReason::NotGranted {
                source: src.clone(),
            },
            "not in allowedMcpServers (/etc/grok/requirements.toml)",
        ),
        (
            McpBlockReason::Lockdown {
                source: src.clone(),
            },
            "locked down by policy (/etc/grok/requirements.toml)",
        ),
        (
            McpBlockReason::ProjectPin {
                source: src.clone(),
            },
            "project MCP disabled by enableAllProjectMcpServers = false (/etc/grok/requirements.toml)",
        ),
    ];
    for (reason, want) in cases {
        assert_eq!(reason.to_string(), want);
        assert_eq!(reason.source(), src.as_path());
    }

    // A blocking source without a path attributes to the empty fallback —
    // the string a consumer renders must stay stable, not panic or lie.
    let ms = ManagedSettings {
        mcp_allowlist: McpServerPolicy::single(
            McpServerAllowlist::new(vec![], vec![], None).with_managed_only(),
        ),
        ..Default::default()
    };
    let subject = McpSubject {
        origin: FOREIGN,
        project_scoped: false,
    };
    match ms.mcp_verdict(&h("https://any.example/mcp"), subject) {
        McpVerdict::Blocked(reason @ McpBlockReason::NotGranted { .. }) => {
            assert_eq!(reason.to_string(), "not in allowedMcpServers ()");
            assert_eq!(reason.source(), std::path::Path::new(""));
        }
        other => panic!("wanted the empty-source NotGranted fallback, got {other:?}"),
    }
}

// ── parse fail-directions (table) ────────────────────────────────

/// Parse `key` while capturing WARN-level logs on this thread. A `Malformed` key
/// yields no entries here; pin it with `is_malformed()` where that matters.
fn parse_mcp_entries_capturing_logs(
    json: &serde_json::Value,
    list: McpPolicyList,
) -> (Vec<AllowedMcpServer>, String) {
    capturing_warn_logs(|| parse_mcp_entry_list(json, list).entries())
}

/// What this table pins: parse-time fail directions. An unenforceable DENY entry
/// fails the key closed; unusable ALLOW entries warn and drop (an all-unsupported allowlist is a lockdown).
#[test]
fn mcp_entry_parse_fail_directions() {
    use McpPolicyList::{Allow, Deny};
    struct ParseCase {
        label: &'static str,
        list: McpPolicyList,
        entries: serde_json::Value,
        /// Entries surviving the parse.
        want_len: usize,
        /// Exact occurrence counts of a log line.
        counts: &'static [(&'static str, usize)],
        /// Log lines that must appear at least once.
        contains: &'static [&'static str],
    }
    impl Default for ParseCase {
        fn default() -> Self {
            Self {
                label: "",
                list: Deny,
                entries: serde_json::Value::Null,
                want_len: 0,
                counts: &[],
                contains: &[],
            }
        }
    }
    let cases = vec![
        ParseCase {
            label: "allow: partial-glob, empty, and missing schemes fail closed but warn",
            list: Allow,
            entries: serde_json::json!([
                { "serverUrl": "http*://mcp.corp.com/*" },
                { "serverUrl": "://mcp.corp.com/*" },
                { "serverUrl": "*.corp.com/*" }
            ]),
            want_len: 3,
            counts: &[("can never match", 3)],
            ..Default::default()
        },
        ParseCase {
            label: "allow: a bare `*://` is the any-scheme wildcard — silent like a written-out scheme",
            list: Allow,
            entries: serde_json::json!([
                { "serverUrl": "*://mcp.corp.com/*" },
                { "serverUrl": "https://mcp.corp.com/*" }
            ]),
            want_len: 2,
            counts: &[("WARN", 0)],
            ..Default::default()
        },
        ParseCase {
            label: "allow: dead IP/label/port shapes warn; working shapes stay silent",
            list: Allow,
            entries: serde_json::json!([
                { "serverUrl": "http://127.1/*" },
                { "serverUrl": "https://2001:db8::1/*" },
                { "serverUrl": "https://bü*.example/*" },
                // Working shapes the warn must NOT fire on: bracketed and
                // canonical IPs, canonical-address-plus-port, trailing dot.
                { "serverUrl": "https://[2001:0db8::1]/*" },
                { "serverUrl": "https://127.0.0.1/*" },
                { "serverUrl": "https://2001:db8::1:443/*" },
                { "serverUrl": "https://127.0.0.1./*" },
                { "serverUrl": "https://2001:db8*:443/*" },
                // Dead shapes: no host, glob port — bracketed IPv6 included.
                { "serverUrl": "https:///admin/*" },
                { "serverUrl": "https://mcp.corp.com:*/mcp/*" },
                { "serverUrl": "https://[::1]:*/*" },
                { "serverUrl": "https://[::1]:http/*" },
                // Working shape: bracketed address with a numeric port.
                { "serverUrl": "https://[::1]:8080/*" }
            ]),
            want_len: 13,
            counts: &[("can never match", 7)],
            contains: &["has no host", "port is not a number"],
        },
        ParseCase {
            label: "deny: every unmatchable serverUrl shape is reported, then the key fails closed",
            list: Deny,
            entries: serde_json::json!([
                { "serverUrl": "/admin/*" },
                { "serverUrl": "https://host[x/*" },
                { "serverUrl": "https://bü*.example/*" },
                { "serverUrl": "https://blocked.example.com/*" }
            ]),
            want_len: 0,
            counts: &[("unenforceable deniedMcpServers entry", 3)],
            contains: &[
                "has no host",
                "host glob does not compile",
                "mixes Unicode and glob characters",
            ],
        },
        ParseCase {
            // Nothing warns before this line, so it must name the causes itself.
            label: "deny: unsupported entry fails the key closed with a self-contained warning",
            list: Deny,
            entries: serde_json::json!([
                { "serverTypo": "internal-only" },
                { "serverUrl": "https://blocked.com/*" }
            ]),
            want_len: 0,
            counts: &[("WARN", 1)],
            contains: &[
                "unenforceable deniedMcpServers entry; failing closed (no recognized field",
            ],
        },
        ParseCase {
            label: "allow: unsupported entry warns and grants nothing",
            list: Allow,
            entries: serde_json::json!([ { "serverTypo": "internal-only" } ]),
            want_len: 0,
            contains: &["ignoring unusable allowedMcpServers entry"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: serverName is first-class — parsed, no warn",
            list: Deny,
            entries: serde_json::json!([ { "serverName": "internal-only" } ]),
            want_len: 1,
            counts: &[("WARN", 0)],
            ..Default::default()
        },
        ParseCase {
            label: "deny: serverCommand array is enforced, not warn-dropped",
            list: Deny,
            entries: serde_json::json!([ { "serverCommand": ["npx", "evil-mcp"] } ]),
            want_len: 1,
            counts: &[("WARN", 0)],
            ..Default::default()
        },
        ParseCase {
            label: "deny: malformed serverCommand argv fails the key closed (a partial \
                    argv would match the wrong command)",
            list: Deny,
            entries: serde_json::json!([ { "serverCommand": ["npx", 42] } ]),
            want_len: 0,
            contains: &["unenforceable deniedMcpServers entry; failing closed"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: empty serverCommand argv fails the key closed (matches no command)",
            list: Deny,
            entries: serde_json::json!([ { "serverCommand": [] } ]),
            want_len: 0,
            contains: &["unenforceable deniedMcpServers entry; failing closed"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: string serverCommand fails the key closed (an argv array is required)",
            list: Deny,
            entries: serde_json::json!([ { "serverCommand": "npx evil-mcp" } ]),
            want_len: 0,
            contains: &["unenforceable deniedMcpServers entry; failing closed"],
            ..Default::default()
        },
    ];
    for case in cases {
        let key = match case.list {
            Allow => "allowedMcpServers",
            Deny => "deniedMcpServers",
        };
        let label = case.label;
        let json = serde_json::json!({ key: case.entries });
        let (entries, logs) = parse_mcp_entries_capturing_logs(&json, case.list);
        assert_eq!(entries.len(), case.want_len, "{label}: surviving entries");
        for (needle, count) in case.counts {
            assert_eq!(
                logs.matches(needle).count(),
                *count,
                "{label}: count of {needle:?}, got: {logs:?}"
            );
        }
        for needle in case.contains {
            assert!(
                logs.contains(needle),
                "{label}: missing {needle:?} in {logs:?}"
            );
        }
    }

    // The serverName row must parse into a Name entry, not just survive.
    let (entries, _) = parse_mcp_entries_capturing_logs(
        &serde_json::json!({ "deniedMcpServers": [ { "serverName": "internal-only" } ] }),
        Deny,
    );
    assert!(
        matches!(&entries.first().unwrap_or_else(|| panic!("expected entry")), AllowedMcpServer::Name { name } if name == "internal-only"),
        "expected a Name entry, got {entries:?}"
    );
}

// ── git-URL identity + marketplace matching (table) ──────────────

/// Pins git-URL identity: scheme and host fold case, paths stay case-sensitive, exactly one `.git` strips, scp and https never alias.
/// Local paths pass through. Marketplace allowlist matching is built on this.
#[test]
#[rustfmt::skip]
fn git_url_identity_and_marketplace_matching() {
    let rows: [(&str, &str, &str); 4] = [
        (
            "HTTPS://Git.Corp.com/Team/Tools.git",
            "https://git.corp.com/Team/Tools",
            "scheme+host fold case; one .git strips",
        ),
        (
            "https://h.example/repo.git.git",
            "https://h.example/repo.git",
            "`repo.git.git` is a repo named `repo.git`",
        ),
        (
            "Git@GitHub.com:Org/Repo.git",
            "git@github.com:Org/Repo",
            "scp-style: user@host folds, path does not",
        ),
        ("/tmp/Marketplace", "/tmp/Marketplace", "local paths pass through untouched"),
    ];
    for (input, want, label) in rows {
        assert_eq!(normalize_git_url(input), want, "{label}");
    }
    assert_ne!(
        normalize_git_url("https://git.corp.com/team/tools"),
        normalize_git_url("https://git.corp.com/Team/Tools"),
        "path case is identity: a different-cased path is a different repo"
    );

    // (allowlist entry, candidate URL, allowed?, why)
    let ent = "git@github.enterprise.example:ACME/repo.git";
    let match_rows: [(&str, &str, bool, &str); 7] = [
        (ent, "git@github.enterprise.example:ACME/repo.git", true, "exact spelling"),
        (ent, "git@github.enterprise.example:ACME/repo", true, ".git suffix is optional"),
        (ent, "git@GITHUB.ENTERPRISE.EXAMPLE:ACME/repo.git", true, "host case folds"),
        (ent, "git@github.enterprise.example:acme/repo.git", false, "path case earns no grant"),
        (ent, "git@evil.com:ACME/repo.git", false, "different host"),
        (
            "git@github.com:Org/Repo.git",
            "https://github.com/Org/Repo.git",
            false,
            "scp-form entry does not alias the https form of the same repo",
        ),
        (
            "https://github.com/Org/Repo.git",
            "git@github.com:Org/Repo.git",
            false,
            "https-form entry does not alias the scp form",
        ),
    ];
    for (entry, url, want, label) in match_rows {
        let al = MarketplaceAllowlist {
            allowed_urls: vec![entry.into()],
            source_path: None,
            authority: PolicySourceAuthority::Native,
        };
        assert_eq!(al.is_url_allowed(url), want, "{label}");
    }
}

// ── layer resolution (scenario table) ────────────────────────────

/// [`resolve_managed_settings`] over inline sources: an optional Claude JSON
/// plus `(tier, path, toml)` layers.
fn layered(
    claude: Option<serde_json::Value>,
    layers: &[(PolicyLayerTier, &str, &str)],
) -> ManagedSettings {
    resolve_managed_settings(
        claude.map(|json| (json, PathBuf::from(CLAUDE_PATH))),
        layers
            .iter()
            .map(|(tier, path, toml_str)| PolicyLayer {
                tier: *tier,
                path: PathBuf::from(path),
                value: toml::from_str(toml_str).unwrap(),
            })
            .collect(),
    )
}

/// One layer-resolution expectation; each variant names the surface it pins.
enum Expect {
    /// `is_server_allowed` for an HTTP server (name, url, origin, want).
    Allowed(&'static str, &'static str, PolicySubjectOrigin, bool),
    /// `is_server_denied` for an HTTP server (name, url, origin, want).
    Denied(&'static str, &'static str, PolicySubjectOrigin, bool),
    /// `is_server_denied` for a stdio server with exactly this argv.
    ArgvDenied(&'static [&'static str], bool),
    ManagedOnly(PolicySubjectOrigin, bool),
    /// Project-MCP pin state: `Some(path)` = disabled, attributed there.
    ProjectMcpPin(Option<&'static str>),
    /// `mcp_project_pin_block` for a project-scoped foreign HTTP server (url, blocked?).
    ProjectPinBlocks(&'static str, bool),
    /// Plugin auto-update pin state: `Some(path)` = disabled, attributed there.
    AutoUpdatePin(Option<&'static str>),
    MarketRestricted(bool),
    MarketUrl(&'static str, PolicySubjectOrigin, bool),
    /// `add_block_reason(url).is_some()`.
    MarketAddBlocked(&'static str, bool),
    ExtrasCount(usize),
    /// `extra_marketplaces[idx]` is a Git marketplace (name, url, ref).
    ExtraGit(usize, &'static str, &'static str, Option<&'static str>),
}

fn assert_expects(label: &str, ms: &ManagedSettings, expects: Vec<Expect>) {
    for e in expects {
        match e {
            Expect::Allowed(name, url, origin, want) => assert_eq!(
                ms.mcp_allowlist.is_server_allowed(&hs(name, url), origin),
                want,
                "{label}: allowed({url}, {origin:?})"
            ),
            Expect::Denied(name, url, origin, want) => assert_eq!(
                ms.mcp_allowlist.is_server_denied(&hs(name, url), origin),
                want,
                "{label}: denied({url}, {origin:?})"
            ),
            Expect::ArgvDenied(argv, want) => assert_eq!(
                ms.mcp_allowlist.is_server_denied(
                    &sa(
                        "t",
                        argv.first().unwrap_or_else(|| panic!("expected argv0")),
                        argv.get(1..).unwrap_or(&[])
                    ),
                    FOREIGN
                ),
                want,
                "{label}: argv denied({argv:?})"
            ),
            Expect::ManagedOnly(origin, want) => assert_eq!(
                ms.mcp_allowlist.managed_only(origin),
                want,
                "{label}: managed_only({origin:?})"
            ),
            Expect::ProjectMcpPin(source) => assert_eq!(
                ms.project_mcp.source(),
                source.map(Path::new),
                "{label}: project_mcp pin"
            ),
            Expect::ProjectPinBlocks(url, want) => {
                let subject = McpSubject {
                    origin: FOREIGN,
                    project_scoped: true,
                };
                assert_eq!(
                    ms.mcp_project_pin_block(&hs("p", url), subject).is_some(),
                    want,
                    "{label}: project pin blocks({url})"
                )
            }
            Expect::AutoUpdatePin(source) => assert_eq!(
                ms.plugin_auto_update.source(),
                source.map(Path::new),
                "{label}: auto-update pin"
            ),
            Expect::MarketRestricted(want) => assert_eq!(
                ms.marketplace_allowlist.is_restricted(),
                want,
                "{label}: marketplace restricted"
            ),
            Expect::MarketUrl(url, origin, want) => assert_eq!(
                ms.marketplace_allowlist.is_url_allowed(url, origin),
                want,
                "{label}: marketplace url({url}, {origin:?})"
            ),
            Expect::MarketAddBlocked(url, want) => assert_eq!(
                ms.marketplace_allowlist.add_block_reason(url).is_some(),
                want,
                "{label}: add gate({url})"
            ),
            Expect::ExtrasCount(n) => {
                assert_eq!(ms.extra_marketplaces.len(), n, "{label}: extras count")
            }
            Expect::ExtraGit(idx, name, url, git_ref) => {
                let extra = &ms
                    .extra_marketplaces
                    .get(idx)
                    .unwrap_or_else(|| panic!("expected extra marketplace {idx}"));
                assert_eq!(extra.name, name, "{label}: extra[{idx}] name");
                assert_eq!(
                    extra.kind,
                    ManagedMarketplaceKind::Git {
                        url: url.into(),
                        git_ref: git_ref.map(String::from),
                    },
                    "{label}: extra[{idx}] kind"
                );
            }
        }
    }
}

/// Pins strictest-wins layer resolution: any deny wins, restricted sources intersect, pins only tighten.
/// Grok TOML binds native subjects; vendor Claude is advisory. Malformed values degrade per-key and never drop healthy pins.
#[test]
fn layer_resolution_semantics() {
    assert_expects(
        "requirements TOML pins combine strictest-wins with the Claude source",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [
                    { "serverUrl": "https://ok.example.com/*" },
                    { "serverUrl": "https://user-extra.example.com/*" }
                ]
            })),
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
allow_managed_mcp_servers_only = true
enable_all_project_mcp_servers = false
plugin_auto_update = false

[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"

[[denied_mcp_servers]]
server_command = ["npx", "evil-mcp"]

[[strict_known_marketplaces]]
source = "git"
url = "https://github.com/example-corp/approved-plugins.git"

[[strict_known_marketplaces]]
source = "github"
repo = "acme/approved-plugins"
ref = "stable"

[extra_known_marketplaces.approved-plugins]
source = { source = "git", url = "https://github.com/example-corp/approved-plugins.git", ref = "main" }
"#,
            )],
        ),
        vec![
            Expect::Allowed("ok", "https://ok.example.com/mcp", FOREIGN, true),
            // Allowed by the Claude source alone — blocked by the pin.
            Expect::Allowed(
                "extra",
                "https://user-extra.example.com/mcp",
                FOREIGN,
                false,
            ),
            Expect::ArgvDenied(&["npx", "evil-mcp"], true),
            Expect::ManagedOnly(FOREIGN, true),
            Expect::ProjectMcpPin(Some(SYS_REQ)),
            Expect::AutoUpdatePin(Some(SYS_REQ)),
            Expect::MarketRestricted(true),
            Expect::MarketUrl(
                "https://github.com/example-corp/approved-plugins.git",
                FOREIGN,
                true,
            ),
            // github+repo strict entry participates (ref tolerated).
            Expect::MarketUrl(
                "https://github.com/acme/approved-plugins.git",
                FOREIGN,
                true,
            ),
            Expect::MarketUrl("https://github.com/evil/repo.git", FOREIGN, false),
            Expect::ExtrasCount(1),
            Expect::ExtraGit(
                0,
                "approved-plugins",
                "https://github.com/example-corp/approved-plugins.git",
                Some("main"),
            ),
        ],
    );

    // User layer deliberately listed FIRST: the tier sort must reorder, so a
    // user-writable layer or the advisory Claude file never claims a
    // marketplace name or a pin's attribution ahead of an admin layer.
    assert_expects(
        "first-wins resolution is trust-descending, not load-order",
        &layered(
            Some(serde_json::json!({
                "pluginAutoUpdate": false,
                "extraKnownMarketplaces": {
                    "corp": { "source": { "source": "git", "url": "https://github.com/claude/evil.git" } }
                }
            })),
            &[
                (
                    PolicyLayerTier::UserRequirements,
                    USER_REQ,
                    r#"
plugin_auto_update = false

[extra_known_marketplaces.corp]
source = { source = "git", url = "https://github.com/user/evil.git" }
"#,
                ),
                (
                    PolicyLayerTier::SystemRequirements,
                    SYS_REQ,
                    r#"
plugin_auto_update = false

[extra_known_marketplaces.corp]
source = { source = "git", url = "https://github.com/corp/approved.git", ref = "stable" }
"#,
                ),
            ],
        ),
        vec![
            Expect::ExtrasCount(1),
            // The admin layer's URL claims the name; squats are skipped.
            Expect::ExtraGit(
                0,
                "corp",
                "https://github.com/corp/approved.git",
                Some("stable"),
            ),
            // Pin attribution names the admin layer, not the user file.
            Expect::AutoUpdatePin(Some(SYS_REQ)),
        ],
    );

    // The vendor file applies last, yet its admin-owned pin re-attributes a
    // pin a user layer set first (grok inspect / doctor show the vendor path).
    assert_expects(
        "an admin-owned vendor pin upgrades a user-owned auto-update pin",
        &layered(
            Some(serde_json::json!({ "pluginAutoUpdate": false })),
            &[(
                PolicyLayerTier::UserManaged,
                USER_MANAGED,
                "plugin_auto_update = false\n",
            )],
        ),
        vec![Expect::AutoUpdatePin(Some(CLAUDE_PATH))],
    );

    // An exotic TOML value (`inf` -> JSON null) OUTSIDE the policy keys must
    // not affect the layer's policy pins.
    assert_expects(
        "non-finite float elsewhere in a layer keeps its policy pins",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
unrelated_tuning = inf
plugin_auto_update = false

[[denied_mcp_servers]]
server_name = "blocked"
"#,
            )],
        ),
        vec![
            Expect::AutoUpdatePin(Some(SYS_REQ)),
            Expect::Denied("blocked", "https://x.example.com/mcp", FOREIGN, true),
        ],
    );

    // A wrong-typed policy list (table or string instead of array-of-tables) locks
    // its surface down (classified missing-allow); well-typed siblings still apply.
    assert_expects(
        "wrong-typed policy lists lock down without dropping sibling keys",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
plugin_auto_update = false
denied_mcp_servers = { server_url = "https://evil.example.com/*" }
strict_known_marketplaces = "https://github.com/corp/x.git"
"#,
            )],
        ),
        vec![
            Expect::Denied("evil", "https://evil.example.com/mcp", FOREIGN, false),
            Expect::Allowed("evil", "https://evil.example.com/mcp", FOREIGN, false),
            Expect::MarketRestricted(true),
            Expect::MarketUrl("https://github.com/corp/x.git", FOREIGN, false),
            Expect::AutoUpdatePin(Some(SYS_REQ)),
        ],
    );

    // TOML `strict_known_marketplaces = []` reaches the engine as a zero-URL
    // source: add/install fail closed with no URL listed to compare against.
    assert_expects(
        "an empty strict marketplace list locks add and install down",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                "strict_known_marketplaces = []\n",
            )],
        ),
        vec![
            Expect::MarketRestricted(true),
            Expect::MarketUrl("https://github.com/any/repo.git", NATIVE, false),
            Expect::MarketAddBlocked("https://github.com/any/repo.git", true),
            Expect::MarketAddBlocked("/opt/local-marketplace", true),
        ],
    );

    // A non-string value INSIDE a deny entry (TOML datetime → object, `inf` → null)
    // is an unenforceable deny: the key fails closed and each layer's sibling pin survives.
    assert_expects(
        "unstringifiable values inside deny entries fail the key closed",
        &layered(
            None,
            &[
                (
                    PolicyLayerTier::SystemRequirements,
                    SYS_REQ,
                    r#"
plugin_auto_update = false

[[denied_mcp_servers]]
server_url = 1979-05-27T07:32:00Z
"#,
                ),
                (
                    PolicyLayerTier::UserRequirements,
                    USER_REQ,
                    r#"
enable_all_project_mcp_servers = false

[[denied_mcp_servers]]
server_url = inf
"#,
                ),
            ],
        ),
        vec![
            Expect::AutoUpdatePin(Some(SYS_REQ)),
            Expect::ProjectMcpPin(Some(USER_REQ)),
            Expect::Denied("t", "https://x.example.com/mcp", FOREIGN, false),
            Expect::Allowed("t", "https://x.example.com/mcp", FOREIGN, false),
        ],
    );

    assert_expects(
        "the vendor Claude JSON is advisory: binds foreign subjects only",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [ { "serverUrl": "https://ok.example.com/*" } ],
                "deniedMcpServers": [ { "serverUrl": "https://denied.example.com/*" } ],
                "allowManagedMcpServersOnly": true,
                "strictKnownMarketplaces": [
                    { "source": "git", "url": "https://github.com/example-corp/approved-plugins.git" }
                ]
            })),
            &[],
        ),
        vec![
            // Foreign subjects (project files, imported editor configs,
            // client injection): the vendor file binds.
            Expect::Allowed("denied", "https://denied.example.com/mcp", FOREIGN, false),
            Expect::Allowed(
                "unlisted",
                "https://unlisted.example.com/mcp",
                FOREIGN,
                false,
            ),
            // grok-native subjects (user/system config.toml, plugins):
            // advisory — the same servers still run.
            Expect::Allowed("denied", "https://denied.example.com/mcp", NATIVE, true),
            Expect::Allowed("unlisted", "https://unlisted.example.com/mcp", NATIVE, true),
            Expect::MarketUrl("https://github.com/other/repo.git", FOREIGN, false),
            Expect::MarketUrl("https://github.com/other/repo.git", NATIVE, true),
            // The add/install gate acquires NEW sources — not grok-native
            // yet, so even an advisory strict list fail-closes it.
            Expect::MarketAddBlocked("https://github.com/other/repo.git", true),
        ],
    );

    assert_expects(
        "grok's own signed TOML layers bind native subjects too",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
[[denied_mcp_servers]]
server_url = "https://denied.example.com/*"

[[strict_known_marketplaces]]
source = "git"
url = "https://github.com/example-corp/approved-plugins.git"
"#,
            )],
        ),
        vec![
            Expect::Allowed("denied", "https://denied.example.com/mcp", NATIVE, false),
            Expect::MarketUrl("https://github.com/other/repo.git", NATIVE, false),
        ],
    );

    // The user-pinned URL fails the admin source; the admin URL fails the
    // user source. Intersection blocks both — never a union.
    assert_expects(
        "a user layer's extra allow URL cannot re-admit what admin excludes",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [ { "serverUrl": "https://admin.example.com/*" } ]
            })),
            &[(
                PolicyLayerTier::UserRequirements,
                USER_REQ,
                r#"
[[allowed_mcp_servers]]
server_url = "https://user.example.com/*"
"#,
            )],
        ),
        vec![
            Expect::Allowed("u", "https://user.example.com/mcp", FOREIGN, false),
            Expect::Allowed("a", "https://admin.example.com/mcp", FOREIGN, false),
        ],
    );

    assert_expects(
        "Claude JSON carries the pins; github+repo strict entries canonicalize",
        &layered(
            Some(serde_json::json!({
                "allowManagedMcpServersOnly": true,
                "enableAllProjectMcpServers": false,
                "strictKnownMarketplaces": [
                    { "source": "github", "repo": "acme/approved-plugins" }
                ],
                "extraKnownMarketplaces": {
                    "acme": {
                        "source": { "source": "github", "repo": "acme/approved-plugins", "ref": "v2" },
                        "autoUpdate": false
                    }
                }
            })),
            &[],
        ),
        vec![
            Expect::ManagedOnly(FOREIGN, true),
            Expect::ProjectMcpPin(Some(CLAUDE_PATH)),
            // github+repo strict entry participates in the allowlist
            // (fail-closed fix: previously the allowlist came out empty).
            Expect::MarketRestricted(true),
            Expect::MarketUrl(
                "https://github.com/acme/approved-plugins.git",
                FOREIGN,
                true,
            ),
            Expect::MarketUrl("https://github.com/evil/repo.git", FOREIGN, false),
            // github+repo extra with ref canonicalizes and keeps the ref.
            Expect::ExtraGit(
                0,
                "acme",
                "https://github.com/acme/approved-plugins.git",
                Some("v2"),
            ),
        ],
    );

    // `extraKnownMarketplaces.<name>.autoUpdate = false` pins the GLOBAL
    // session-start auto-update off (no per-marketplace granularity;
    // silently ignoring the opt-out would fail open).
    assert_expects(
        "extras-level autoUpdate:false pins global auto-update",
        &layered(
            Some(serde_json::json!({
                "extraKnownMarketplaces": {
                    "corp": {
                        "source": { "source": "git", "url": "https://github.com/corp/approved.git" },
                        "autoUpdate": false
                    }
                }
            })),
            &[],
        ),
        vec![Expect::AutoUpdatePin(Some(CLAUDE_PATH))],
    );
    assert_expects(
        "autoUpdate absent does not pin",
        &layered(
            Some(serde_json::json!({
                "extraKnownMarketplaces": {
                    "corp": { "source": { "source": "git", "url": "https://github.com/corp/approved.git" } }
                }
            })),
            &[],
        ),
        vec![Expect::AutoUpdatePin(None)],
    );
}

/// An admin-owned lockdown (any admin tier) accepts only admin-owned entries,
/// wherever they are shipped; a user-owned lockdown accepts any layer's entry.
#[test]
fn user_layer_cannot_satisfy_admin_managed_only_grant() {
    const LOCKDOWN: &str = "allow_managed_mcp_servers_only = true\n";
    const GRANT: &str = r#"
[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"
"#;
    let allowed = |want| Expect::Allowed("ok", "https://ok.example.com/mcp", FOREIGN, want);

    for (admin, admin_path) in ADMIN_TIERS {
        for (user, user_path) in USER_TIERS {
            assert_expects(
                &format!("a {user:?} grant must not satisfy a {admin:?} lockdown"),
                &layered(
                    None,
                    &[(admin, admin_path, LOCKDOWN), (user, user_path, GRANT)],
                ),
                vec![allowed(false)],
            );
        }
    }
    assert_expects(
        "admin entries satisfy an admin lockdown shipped in another admin tier",
        &layered(
            None,
            &[
                (PolicyLayerTier::Mdm, MDM_REQ, LOCKDOWN),
                (PolicyLayerTier::SystemManaged, SYS_MANAGED, GRANT),
            ],
        ),
        vec![allowed(true)],
    );
    assert_expects(
        "a user-pinned lockdown accepts an admin grant",
        &layered(
            None,
            &[
                (PolicyLayerTier::UserManaged, USER_MANAGED, LOCKDOWN),
                (PolicyLayerTier::SystemRequirements, SYS_REQ, GRANT),
            ],
        ),
        vec![allowed(true)],
    );
    assert_expects(
        "a user-pinned lockdown accepts the same file's grant",
        &layered(
            None,
            &[(
                PolicyLayerTier::UserManaged,
                USER_MANAGED,
                &format!("{LOCKDOWN}{GRANT}"),
            )],
        ),
        vec![allowed(true)],
    );
    assert_expects(
        "vendor entries are admin-owned and satisfy an admin TOML lockdown",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [ { "serverUrl": "https://ok.example.com/*" } ]
            })),
            &[(PolicyLayerTier::SystemManaged, SYS_MANAGED, LOCKDOWN)],
        ),
        vec![allowed(true)],
    );
}

/// The project-MCP pin's exception grant is ownership-aware; a user-owned pin
/// upgrades (with re-attribution) when the admin-owned vendor file also pins.
#[test]
fn admin_project_pin_ignores_user_layer_grants() {
    const PIN: &str = "enable_all_project_mcp_servers = false\n";
    const GRANT: &str = r#"
[[allowed_mcp_servers]]
server_url = "https://proj.example.com/*"
"#;
    let blocked = |want| Expect::ProjectPinBlocks("https://proj.example.com/mcp", want);

    for (user, user_path) in USER_TIERS {
        assert_expects(
            &format!("an admin pin ignores a {user:?} grant"),
            &layered(
                None,
                &[
                    (PolicyLayerTier::SystemManaged, SYS_MANAGED, PIN),
                    (user, user_path, GRANT),
                ],
            ),
            vec![blocked(true)],
        );
    }
    assert_expects(
        "an admin grant carves the exception out of an admin pin",
        &layered(
            None,
            &[
                (PolicyLayerTier::SystemManaged, SYS_MANAGED, PIN),
                (PolicyLayerTier::SystemRequirements, SYS_REQ, GRANT),
            ],
        ),
        vec![blocked(false)],
    );
    assert_expects(
        "a user pin accepts an admin grant",
        &layered(
            None,
            &[
                (PolicyLayerTier::UserManaged, USER_MANAGED, PIN),
                (PolicyLayerTier::SystemRequirements, SYS_REQ, GRANT),
            ],
        ),
        vec![blocked(false)],
    );
    assert_expects(
        "a user pin accepts the same file's grant",
        &layered(
            None,
            &[(
                PolicyLayerTier::UserManaged,
                USER_MANAGED,
                &format!("{PIN}{GRANT}"),
            )],
        ),
        vec![blocked(false)],
    );
    assert_expects(
        "the vendor file's pin upgrades a user pin: the user grant stops counting",
        &layered(
            Some(serde_json::json!({ "enableAllProjectMcpServers": false })),
            &[(
                PolicyLayerTier::UserManaged,
                USER_MANAGED,
                &format!("{PIN}{GRANT}"),
            )],
        ),
        vec![blocked(true), Expect::ProjectMcpPin(Some(CLAUDE_PATH))],
    );
    assert_expects(
        "an admin pin set first stays admin-attributed when a user layer also pins",
        &layered(
            None,
            &[
                (PolicyLayerTier::SystemManaged, SYS_MANAGED, PIN),
                (
                    PolicyLayerTier::UserManaged,
                    USER_MANAGED,
                    &format!("{PIN}{GRANT}"),
                ),
            ],
        ),
        vec![blocked(true), Expect::ProjectMcpPin(Some(SYS_MANAGED))],
    );
}

/// Every auto-update pin that is not a literal `pluginAutoUpdate = false` warns with
/// the source path and the consequence: fail-closed bools and extras entries alike.
/// Rows are (label, source, needle, WARN lines): one defect is reported once.
#[test]
fn auto_update_pin_warnings_name_the_source_and_consequence() {
    let path = std::path::Path::new(CLAUDE_PATH);
    let git = serde_json::json!({ "source": "git", "url": "https://github.com/corp/approved.git" });
    let cases = [
        (
            // Two defects: the wrong-typed value, then the opt-out it fails closed to.
            "wrong-typed extras autoUpdate (block on error)",
            serde_json::json!({
                "extraKnownMarketplaces": { "corp": { "source": git.clone(), "autoUpdate": "false" } }
            }),
            "policy key must be a boolean",
            2,
        ),
        (
            "conflicting spellings of a bool pin",
            serde_json::json!({ "pluginAutoUpdate": true, "plugin_auto_update": false }),
            "applying the fail-closed value",
            1,
        ),
        (
            "extras entry opting out (widened to the global pin)",
            serde_json::json!({
                "extraKnownMarketplaces": { "corp": { "source": git.clone(), "autoUpdate": false } }
            }),
            "pinned off",
            1,
        ),
        (
            "string-shorthand extras entry (opt-out unknowable)",
            serde_json::json!({
                "extraKnownMarketplaces": { "corp": "https://github.com/corp/approved.git" }
            }),
            "pinned off",
            1,
        ),
        (
            "conflicting spellings of the extras table (opt-outs unknowable)",
            serde_json::json!({
                "extraKnownMarketplaces": { "corp": { "source": git.clone() } },
                "extra_known_marketplaces": {}
            }),
            "pinned off",
            1,
        ),
    ];
    for (label, json, needle, warns) in cases {
        let (ms, logs) = capturing_warn_logs(|| parse_managed_settings_json(&json, path));
        assert!(ms.plugin_auto_update.is_disabled(), "{label}: must pin off");
        for needle in [needle, CLAUDE_PATH] {
            assert!(
                logs.contains(needle),
                "{label}: missing {needle:?} in: {logs:?}"
            );
        }
        assert_eq!(
            logs.matches("WARN").count(),
            warns,
            "{label}: WARN lines in: {logs:?}"
        );
    }
}

// ── unique-setup tests (fixtures, disk layers, hand-built policies) ──

/// The full GA fixture: all 43 allowedMcpServers entries parse — zero dropped.
#[test]
fn enterprise_ga_fixture_parses_all_entries() {
    let raw = include_str!("../../../tests/fixtures/enterprise-managed-settings-ga.json");
    let json: serde_json::Value = serde_json::from_str(raw).unwrap();
    let expected = json
        .get("allowedMcpServers")
        .unwrap_or(&serde_json::Value::Null)
        .as_array()
        .unwrap()
        .len();

    let path =
        std::path::Path::new("/Library/Application Support/ClaudeCode/managed-settings.json");
    let ms = parse_managed_settings_json(&json, path);

    assert_eq!(ms.mcp_allowlist.sources.len(), 1);
    let source = &ms
        .mcp_allowlist
        .sources
        .first()
        .unwrap_or_else(|| panic!("expected allowlist source"));
    assert_eq!(
        source.entries().count(),
        expected,
        "every allowedMcpServers entry must parse into policy"
    );
    let argv_entries = source
        .entries()
        .filter(|e| matches!(e, AllowedMcpServer::StdioArgv { .. }))
        .count();
    let url_entries = source
        .entries()
        .filter(|e| matches!(e, AllowedMcpServer::Http { .. }))
        .count();
    let fixture_entries_with = |key: &str| {
        json.get("allowedMcpServers")
            .unwrap_or(&serde_json::Value::Null)
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e.get(key).is_some())
            .count()
    };
    assert_eq!(
        url_entries,
        fixture_entries_with("serverUrl"),
        "every serverUrl entry must parse to an Http entry"
    );
    assert_eq!(
        argv_entries,
        fixture_entries_with("serverCommand"),
        "every serverCommand entry must parse to a StdioArgv entry"
    );

    // The lockdown + project pins from the fixture, plus the extras-level
    // autoUpdate:false opt-out mapping to the global pin.
    assert!(ms.mcp_allowlist.managed_only(FOREIGN));
    assert!(ms.project_mcp.is_disabled());
    assert!(ms.plugin_auto_update.is_disabled());

    // Spot-check enforcement: a listed argv runs, an unlisted one doesn't;
    // a path-bearing URL entry grants its path, a path-less one grants any path.
    assert!(ms.mcp_allowlist.is_server_allowed(
        &sa(
            "ui-kit",
            "npx",
            &["@example-corp/ui-kit-mcp", "enterprise-webc"]
        ),
        FOREIGN
    ));
    assert!(ms.mcp_allowlist.is_server_allowed(
        &hs("design", "https://mcp.design-tool.example/mcp"),
        FOREIGN
    ));
    assert!(ms.mcp_allowlist.is_server_allowed(
        &hs("kb", "https://knowledge-mcp.cloud.example/mcp"),
        FOREIGN
    ));
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&ss("rogue", "python3"), FOREIGN)
    );

    // Marketplace: git+url allowlisted; the extra registered.
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(ms.marketplace_allowlist.is_url_allowed(
        "https://github.com/example-corp/approved-plugins.git",
        FOREIGN
    ));
    assert_eq!(ms.extra_marketplaces.len(), 1);
    assert_eq!(
        ms.extra_marketplaces
            .first()
            .unwrap_or_else(|| panic!("expected extra marketplace"))
            .name,
        "approved-plugins"
    );
}

/// A managed-only block is attributed to a lockdown source that is actually
/// unsatisfied — never to a source whose own allowlist contains the server.
#[test]
fn managed_only_block_names_an_unsatisfied_lockdown_source() {
    // User layer: managed-only plus a grant for the server (self-satisfied).
    // Vendor file: admin-owned managed-only with no entry — the real blocker.
    let ms = layered(
        Some(serde_json::json!({ "allowManagedMcpServersOnly": true })),
        &[(
            PolicyLayerTier::UserManaged,
            USER_MANAGED,
            r#"
allow_managed_mcp_servers_only = true

[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"
"#,
        )],
    );
    let server = hs("ok", "https://ok.example.com/mcp");
    let subject = McpSubject {
        origin: FOREIGN,
        project_scoped: false,
    };
    let McpVerdict::Blocked(reason) = ms.mcp_verdict(&server, subject) else {
        panic!("the admin lockdown must block the user-granted server");
    };
    assert!(
        matches!(&reason, McpBlockReason::NotGranted { source } if source == Path::new(CLAUDE_PATH)),
        "block must name the unsatisfied vendor lockdown, not the user file \
         whose allowlist contains the server; got {reason:?}"
    );
}

/// Extras name dedupe is ownership-aware: a user layer cannot claim a name ahead
/// of the vendor file's admin-owned entry (e.g. one carrying a Local pin).
#[test]
fn vendor_admin_extra_wins_name_over_user_layer_squat() {
    let ms = layered(
        Some(serde_json::json!({
            "extraKnownMarketplaces": {
                "corp": { "source": { "source": "local", "path": "/opt/mp" } }
            }
        })),
        &[(
            PolicyLayerTier::UserManaged,
            USER_MANAGED,
            r#"
[extra_known_marketplaces.corp]
source = { source = "git", url = "https://github.com/user/squat.git" }
"#,
        )],
    );
    assert_eq!(ms.extra_marketplaces.len(), 1);
    assert_eq!(
        ms.extra_marketplaces
            .first()
            .unwrap_or_else(|| panic!("expected extra marketplace"))
            .ownership,
        PolicyLayerOwnership::Admin,
        "the admin-owned vendor pin must win the name over the user squat"
    );
    assert_eq!(
        ms.extra_marketplaces
            .first()
            .unwrap_or_else(|| panic!("expected extra marketplace"))
            .kind,
        ManagedMarketplaceKind::Local {
            path: "/opt/mp".into()
        }
    );
}

/// Real-file fixture through `managed_config_layers_at` →
/// `managed_toml_policy_layers` → `resolve_managed_settings`: if layer
/// discovery regresses to empty, the policy controls vanish and this fails.
#[test]
fn managed_config_layers_from_disk_reach_the_policy_engine() {
    let system = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    std::fs::write(
        system.path().join("managed_config.toml"),
        r#"
allow_managed_mcp_servers_only = true
plugin_auto_update = false

[[denied_mcp_servers]]
server_url = "https://evil.example.com/*"

[[strict_known_marketplaces]]
source = "git"
url = "https://github.com/corp/approved.git"

[extra_known_marketplaces.corp-local]
source = { source = "local", path = "/opt/mp" }
"#,
    )
    .unwrap();
    std::fs::write(
        user.path().join("managed_config.toml"),
        r#"
[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"

[extra_known_marketplaces.user-local]
source = { source = "local", path = "/tmp/mp" }
"#,
    )
    .unwrap();

    // The synced user requirements.toml grants the same server (UserRequirements).
    let user_requirements = distill_config::RequirementsLayer {
        value: toml::from_str(
            r#"
[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"
"#,
        )
        .unwrap(),
        source: distill_config::RequirementsSource::File(user.path().join("requirements.toml")),
        is_system: false,
    };
    let layers = distill_config::managed_config_layers_at(Some(system.path()), Some(user.path()));
    let ms = resolve_managed_settings(
        None,
        managed_toml_policy_layers(layers, vec![user_requirements]),
    );

    // System layer: deny + lockdown + auto-update pin + strict marketplaces,
    // attributed to the on-disk system path.
    assert!(
        ms.mcp_allowlist
            .is_server_denied(&hs("evil", "https://evil.example.com/mcp"), NATIVE)
    );
    assert!(ms.mcp_allowlist.managed_only(NATIVE));
    assert!(ms.plugin_auto_update.is_disabled());
    assert_eq!(
        ms.plugin_auto_update.source(),
        Some(system.path().join("managed_config.toml").as_path())
    );
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(
        !ms.marketplace_allowlist
            .is_url_allowed("https://github.com/evil/repo.git", NATIVE)
    );
    // Neither user layer's grant satisfies the system (admin-owned) lockdown.
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&hs("ok", "https://ok.example.com/mcp"), NATIVE)
    );
    // The blocked grant above would also hold if the requirements layer were
    // dropped, so its source path pins that it reached the engine.
    assert!(
        ms.mcp_allowlist
            .source_paths()
            .contains(&user.path().join("requirements.toml").as_path())
    );
    // Pin ownership follows the layer that provisioned it: only Admin pins
    // may carve a Local source out of a strict lockdown.
    let ownership: std::collections::HashMap<&str, PolicyLayerOwnership> = ms
        .extra_marketplaces
        .iter()
        .map(|m| (m.name.as_str(), m.ownership))
        .collect();
    assert_eq!(
        ownership.get("corp-local"),
        Some(&PolicyLayerOwnership::Admin)
    );
    assert_eq!(
        ownership.get("user-local"),
        Some(&PolicyLayerOwnership::User)
    );
}

/// Add gate: fail-closed for non-allowlisted git URLs and local paths.
#[test]
fn marketplace_add_gate_fails_closed() {
    let restricted = MarketplacePolicy::single(MarketplaceAllowlist {
        allowed_urls: vec!["https://github.com/example-corp/approved-plugins.git".into()],
        source_path: Some(PathBuf::from(SYS_REQ)),
        authority: PolicySourceAuthority::Native,
    });
    assert!(
        restricted
            .add_block_reason("https://github.com/example-corp/approved-plugins.git")
            .is_none()
    );
    let blocked = restricted
        .add_block_reason("https://github.com/evil/repo.git")
        .expect("non-allowlisted URL blocked");
    assert!(blocked.contains("strictKnownMarketplaces"));
    // Local paths never match the git allowlist — blocked (fail-closed).
    assert!(
        restricted
            .add_block_reason("/tmp/local-marketplace")
            .is_some()
    );

    let unrestricted = MarketplacePolicy::default();
    assert!(
        unrestricted
            .add_block_reason("https://github.com/any/repo.git")
            .is_none()
    );
    assert!(
        unrestricted
            .add_block_reason("/tmp/local-marketplace")
            .is_none()
    );
}

/// `block_reason` names the source that actually rejects the URL, not the
/// first restricted source in load order.
#[test]
fn marketplace_block_reason_names_the_actual_blocker() {
    let permissive = MarketplaceAllowlist {
        allowed_urls: vec![
            "https://github.com/ok/repo.git".into(),
            "https://github.com/extra/repo.git".into(),
        ],
        source_path: Some(PathBuf::from(CLAUDE_PATH)),
        authority: PolicySourceAuthority::Advisory,
    };
    let strict = MarketplaceAllowlist {
        allowed_urls: vec!["https://github.com/ok/repo.git".into()],
        source_path: Some(PathBuf::from(SYS_REQ)),
        authority: PolicySourceAuthority::Native,
    };
    let policy = MarketplacePolicy {
        sources: vec![permissive, strict],
    };
    // Allowed by the first source, rejected by the second: the reason must
    // name the second (by file name — the add refusal is user-facing).
    let reason = policy
        .add_block_reason("https://github.com/extra/repo.git")
        .expect("blocked by the strict source");
    assert!(
        reason.contains("requirements.toml") && !reason.contains("managed-settings.json"),
        "reason must name the blocking source, got: {reason}"
    );
    assert!(
        !reason.contains("/etc/grok/"),
        "user-facing refusal must not leak the policy directory, got: {reason}"
    );
    // The full-path form for logs still names the complete source.
    assert!(
        policy
            .block_reason(
                "https://github.com/extra/repo.git",
                PolicySubjectOrigin::Foreign
            )
            .contains(SYS_REQ)
    );
}

/// User-facing MCP refusals name the policy file only; `Display` (doctor
/// details, `mcp list --json`, tracing logs) keeps the full path.
#[test]
fn mcp_block_reason_user_facing_form_names_the_file_only() {
    let reason = McpBlockReason::Deny {
        source: PathBuf::from("/etc/grok/managed_config.toml"),
    };
    assert_eq!(
        reason.to_string(),
        "matches deniedMcpServers (/etc/grok/managed_config.toml)"
    );
    assert_eq!(
        reason.user_facing_reason(),
        "matches deniedMcpServers (managed_config.toml)"
    );
    // No file name (unattributed source) — fall back to the full display.
    let unattributed = McpBlockReason::NotGranted {
        source: PathBuf::new(),
    };
    assert_eq!(
        unattributed.user_facing_reason(),
        "not in allowedMcpServers ()"
    );
}

#[test]
fn parse_managed_settings_reads_nested_default_mode() {
    let json = serde_json::json!({
        "permissions": {
            "defaultMode": "dontAsk",
            "allow": ["Bash(git status)"]
        }
    });
    let path = std::path::Path::new(CLAUDE_PATH);
    let ms = parse_managed_settings_json(&json, path);
    assert_eq!(ms.default_mode, Some(DefaultPermissionMode::DontAsk));
    assert_eq!(ms.permissions.len(), 1);

    let auto_json = serde_json::json!({
        "permissions": { "defaultMode": "auto" }
    });
    let ms_auto = parse_managed_settings_json(&auto_json, path);
    assert_eq!(ms_auto.default_mode, Some(DefaultPermissionMode::Auto));
}

// ── lockdown semantics: empty lists lock down, malformed policy fails closed ──

/// `allowedMcpServers` PRESENT but empty is a lockdown (vendor managed-settings semantics), unlike an absent key.
#[test]
fn present_empty_allowlist_is_lockdown() {
    let any = || hs("any", "https://any.example.com/mcp");
    let empty = allowlist_from(serde_json::json!({ "allowedMcpServers": [] }));
    assert!(has_lockdown_source(&empty));
    assert!(!empty.is_server_allowed(&any(), FOREIGN));
    assert!(!empty.is_server_allowed(&ss("any", "npx"), FOREIGN));
    // The vendor file is advisory: its lockdown must not bind grok-native subjects.
    assert!(empty.is_server_allowed(&hs("native", "https://any.example.com/mcp"), NATIVE));

    // Every entry unsupported = zero parsed entries = the same lockdown.
    let unsupported = allowlist_from(serde_json::json!({
        "allowedMcpServers": [ { "serverTypo": "internal-only" } ]
    }));
    assert!(has_lockdown_source(&unsupported));
    assert!(!unsupported.is_server_allowed(&any(), FOREIGN));

    let absent = allowlist_from(serde_json::json!({}));
    assert!(!absent.is_restricted());
    assert!(absent.is_server_allowed(&any(), FOREIGN));
}

/// An explicit empty DENY list is a no-op: present-but-empty is a lockdown only
/// for allow lists (a deny scaffold must stay harmless).
#[test]
fn present_empty_denylist_is_harmless() {
    let any = || hs("any", "https://any.example.com/mcp");
    let empty = allowlist_from(serde_json::json!({ "deniedMcpServers": [] }));
    assert!(!empty.is_restricted());
    assert!(empty.is_server_allowed(&any(), FOREIGN));

    // The documented TOML spelling through the layered path.
    let ms = layered(
        None,
        &[(
            PolicyLayerTier::SystemRequirements,
            SYS_REQ,
            "denied_mcp_servers = []\n",
        )],
    );
    assert!(!ms.mcp_allowlist.is_restricted());
    assert!(ms.mcp_allowlist.is_server_allowed(&any(), NATIVE));
}

/// `strictKnownMarketplaces: []` is a complete lockdown, as is a strict list
/// whose every entry is unsupported (local paths).
#[test]
fn strict_marketplaces_present_empty_is_lockdown() {
    let path = std::path::Path::new(CLAUDE_PATH);
    let repo = "https://github.com/any/repo.git";
    let ms =
        parse_managed_settings_json(&serde_json::json!({ "strictKnownMarketplaces": [] }), path);
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(!ms.marketplace_allowlist.is_url_allowed(repo, FOREIGN));
    // The vendor file is advisory: its lockdown must not bind grok-native marketplaces.
    assert!(ms.marketplace_allowlist.is_url_allowed(repo, NATIVE));

    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "strictKnownMarketplaces": [ { "source": "local", "path": "/opt/mp" } ]
        }),
        path,
    );
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(!ms.marketplace_allowlist.is_url_allowed(repo, FOREIGN));
}

/// Present-but-wrong-typed policy keys fail closed, never open.
#[test]
fn malformed_policy_keys_fail_closed() {
    let path = std::path::Path::new(CLAUDE_PATH);
    let any = hs("any", "https://any.example.com/mcp");
    // allowedMcpServers with a non-array value = lockdown, not "no policy".
    let ms = parse_managed_settings_json(&serde_json::json!({ "allowedMcpServers": "oops" }), path);
    assert!(has_lockdown_source(&ms.mcp_allowlist));
    assert!(!ms.mcp_allowlist.is_server_allowed(&any, FOREIGN));

    // Non-array deniedMcpServers: intended denials unknowable, so the source
    // locks down — classified as missing-allow, not as a deny match.
    let ms = parse_managed_settings_json(&serde_json::json!({ "deniedMcpServers": 7 }), path);
    assert!(has_lockdown_source(&ms.mcp_allowlist));
    assert!(!ms.mcp_allowlist.is_server_allowed(&any, FOREIGN));
    assert!(!ms.mcp_allowlist.is_server_denied(&any, FOREIGN));

    // Valid allow entries beside a malformed deny list: the lockdown is the
    // source's, so even the listed server is blocked.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "allowedMcpServers": [ { "serverUrl": "https://ok.example.com/*" } ],
            "deniedMcpServers": 7
        }),
        path,
    );
    assert!(has_lockdown_source(&ms.mcp_allowlist));
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&hs("ok", "https://ok.example.com/mcp"), FOREIGN)
    );

    // Non-bool lockdown flag = enabled (block on error).
    let ms = parse_managed_settings_json(
        &serde_json::json!({ "allowManagedMcpServersOnly": "yes" }),
        path,
    );
    assert!(ms.mcp_allowlist.managed_only(FOREIGN));

    // Non-array strict marketplaces = lockdown.
    let ms =
        parse_managed_settings_json(&serde_json::json!({ "strictKnownMarketplaces": 42 }), path);
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(
        !ms.marketplace_allowlist
            .is_url_allowed("https://github.com/any/repo.git", FOREIGN)
    );

    // Non-bool pins = pinned off (fail-closed).
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "enableAllProjectMcpServers": "nope",
            "pluginAutoUpdate": 1
        }),
        path,
    );
    assert!(ms.project_mcp.is_disabled());
    assert!(ms.plugin_auto_update.is_disabled());
}

/// Both spellings of a policy key in ONE source with different values must fail
/// closed like the cross-source combination, not fail open via camel shadowing.
#[test]
fn conflicting_key_spellings_in_one_source_fail_closed() {
    let ms = layered(
        None,
        &[(
            PolicyLayerTier::SystemRequirements,
            SYS_REQ,
            r#"
allowManagedMcpServersOnly = false
allow_managed_mcp_servers_only = true

deniedMcpServers = []

[[denied_mcp_servers]]
server_url = "https://evil.example.com/*"
"#,
        )],
    );
    // Conflicting bool: the key's fail-closed value (lockdown ON) applies.
    assert!(ms.mcp_allowlist.managed_only(FOREIGN));
    // Conflicting deny lists: the intended denials are unknowable — the
    // source locks down, classified missing-allow (not a deny match).
    let evil = hs("evil", "https://evil.example.com/mcp");
    assert!(has_lockdown_source(&ms.mcp_allowlist));
    assert!(!ms.mcp_allowlist.is_server_allowed(&evil, FOREIGN));
    assert!(!ms.mcp_allowlist.is_server_denied(&evil, FOREIGN));

    // Equal values in both spellings are not a conflict.
    let ms = layered(
        None,
        &[(
            PolicyLayerTier::SystemRequirements,
            SYS_REQ,
            "allowManagedMcpServersOnly = true\nallow_managed_mcp_servers_only = true\n",
        )],
    );
    assert!(ms.mcp_allowlist.managed_only(FOREIGN));
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&hs("any", "https://any.example.com/mcp"), FOREIGN)
    );
}

/// One entry carrying both spellings of a field with different values is ambiguous:
/// a deny entry fails the key closed (lockdown), an allow entry grants nothing.
#[test]
fn conflicting_entry_field_spellings_fail_closed() {
    let conflict = serde_json::json!(
        { "serverUrl": "https://evil.example/*", "server_url": "https://other.example/*" }
    );
    let deny = serde_json::json!({ "deniedMcpServers": [conflict.clone()] });
    assert!(parse_mcp_entry_list(&deny, McpPolicyList::Deny).is_malformed());
    let locked = allowlist_from(deny);
    assert!(has_lockdown_source(&locked));
    assert!(!locked.is_server_allowed(&hs("other", "https://other.example/mcp"), FOREIGN));

    let allow = serde_json::json!({ "allowedMcpServers": [conflict] });
    let (entries, logs) = parse_mcp_entries_capturing_logs(&allow, McpPolicyList::Allow);
    assert!(
        entries.is_empty(),
        "an ambiguous allow entry grants nothing: {entries:?}"
    );
    assert!(!parse_mcp_entry_list(&allow, McpPolicyList::Allow).is_malformed());
    assert!(
        logs.contains("ambiguously spelled"),
        "the ambiguous entry must warn, got: {logs:?}"
    );

    // Equal values in both spellings are not a conflict.
    let json = serde_json::json!({
        "deniedMcpServers": [
            { "serverUrl": "https://evil.example/*", "server_url": "https://evil.example/*" }
        ]
    });
    let (entries, logs) = parse_mcp_entries_capturing_logs(&json, McpPolicyList::Deny);
    assert_eq!(entries.len(), 1);
    assert_eq!(logs.matches("WARN").count(), 0, "{logs:?}");
}

/// A layer whose policy keys cannot be converted fails CLOSED — as if every key
/// were present-but-malformed — never silently unwinding its denies and pins.
#[test]
fn unreadable_policy_layer_locks_down() {
    let mut ms = ManagedSettings::default();
    apply_unreadable_policy_source(
        &mut ms,
        std::path::Path::new(SYS_REQ),
        PolicyLayerTier::SystemRequirements,
    );
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&hs("any", "https://any.example.com/mcp"), NATIVE)
    );
    assert!(
        !ms.marketplace_allowlist
            .is_url_allowed("https://github.com/any/repo.git", NATIVE)
    );
    assert!(ms.project_mcp.is_disabled());
    assert!(ms.plugin_auto_update.is_disabled());
}

/// An unreadable `extraKnownMarketplaces` key fails closed on the auto-update
/// pin: a lost `autoUpdate: false` opt-out must not fail open.
#[test]
fn unreadable_extras_fail_closed_on_auto_update_pin() {
    let path = std::path::Path::new(CLAUDE_PATH);
    // Wrong-typed container (array instead of a name-keyed table — the
    // natural typo, since the sibling policy keys ARE arrays of tables).
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "extraKnownMarketplaces": [
                {
                    "source": { "source": "git", "url": "https://github.com/corp/approved.git" },
                    "autoUpdate": false
                }
            ]
        }),
        path,
    );
    assert!(ms.extra_marketplaces.is_empty());
    assert!(ms.plugin_auto_update.is_disabled());

    // A skipped entry (unsupported source) still honors its opt-out.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "extraKnownMarketplaces": {
                "corp": { "source": { "source": "gti" }, "autoUpdate": false }
            }
        }),
        path,
    );
    assert!(ms.extra_marketplaces.is_empty());
    assert!(ms.plugin_auto_update.is_disabled());

    // A non-object entry (the array-vs-table typo one level down) cannot be
    // read for an opt-out at all: pin off, like the wrong-typed container.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "extraKnownMarketplaces": {
                "corp": [ {
                    "source": { "source": "git", "url": "https://github.com/corp/approved.git" },
                    "autoUpdate": false
                } ]
            }
        }),
        path,
    );
    assert!(ms.extra_marketplaces.is_empty());
    assert!(ms.plugin_auto_update.is_disabled());

    // A skipped entry WITHOUT an opt-out does not pin.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "extraKnownMarketplaces": {
                "corp": { "source": { "source": "gti" } }
            }
        }),
        path,
    );
    assert!(!ms.plugin_auto_update.is_disabled());

    // Both spellings of the extras key with different values: the intended
    // extras (and any opt-out they carried) are unknowable.
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "extraKnownMarketplaces": {
                "corp": {
                    "source": { "source": "git", "url": "https://github.com/corp/a.git" },
                    "autoUpdate": false
                }
            },
            "extra_known_marketplaces": {
                "corp": {
                    "source": { "source": "git", "url": "https://github.com/corp/b.git" }
                }
            }
        }),
        path,
    );
    assert!(ms.extra_marketplaces.is_empty());
    assert!(ms.plugin_auto_update.is_disabled());
}

/// The TOML spelling of the present-empty lockdown (`allowed_mcp_servers = []`)
/// locks down through the layered path; no sibling allow entry punches through.
#[test]
fn toml_present_empty_allowlist_locks_down_despite_sibling_grant() {
    let ms = layered(
        None,
        &[
            (
                PolicyLayerTier::SystemManaged,
                SYS_MANAGED,
                "allowed_mcp_servers = []\n",
            ),
            (
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                "[[allowed_mcp_servers]]\nserver_url = \"https://ok.example.com/*\"\n",
            ),
        ],
    );
    assert!(ms.mcp_allowlist.is_restricted());
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&hs("ok", "https://ok.example.com/mcp"), NATIVE),
        "an empty-allowlist lockdown is absolute; even an admin sibling's \
         matching allow entry must not satisfy it"
    );
}

/// Dropping an unenforceable deny entry would block nothing, so the key is
/// malformed and the source is a lockdown (the warning is pinned in the parse table).
#[test]
fn unsupported_deny_entry_fails_the_key_closed() {
    let json = serde_json::json!({
        "deniedMcpServers": [
            { "serverTypo": "internal-only" },
            { "serverUrl": "https://blocked.com/*" }
        ]
    });
    assert!(parse_mcp_entry_list(&json, McpPolicyList::Deny).is_malformed());

    let locked = allowlist_from(json);
    assert!(has_lockdown_source(&locked));
    assert!(!locked.is_server_allowed(&hs("any", "https://any.example.com/mcp"), FOREIGN));
}

/// Every lockdown warns with the source path and the consequence, whatever the cause:
/// wrong-typed or conflicting key, empty or all-unsupported MCP allowlist, all-local strict list.
#[test]
fn lockdown_warnings_name_the_source_and_consequence() {
    let path = std::path::Path::new(CLAUDE_PATH);
    let wrong_typed = serde_json::json!({ "allowedMcpServers": "nope" });
    let conflicting = serde_json::json!({
        "deniedMcpServers": [],
        "denied_mcp_servers": [ { "serverUrl": "https://evil.example/*" } ]
    });
    let zero_entries = serde_json::json!({
        "allowedMcpServers": [],
        "strictKnownMarketplaces": [ { "source": "local", "path": "/opt/mp" } ]
    });

    // The key-level line says what is wrong, the lockdown line names the file; one report each.
    for (label, json, needle) in [
        (
            "wrong-typed key",
            &wrong_typed,
            "policy key must be an array of entries; failing closed",
        ),
        (
            "conflicting spellings",
            &conflicting,
            "policy key is spelled both ways with different values; failing closed",
        ),
    ] {
        let (_, logs) = capturing_warn_logs(|| parse_managed_settings_json(json, path));
        for needle in [needle, "MCP lockdown", CLAUDE_PATH] {
            assert!(
                logs.contains(needle),
                "{label}: missing {needle:?} in: {logs:?}"
            );
        }
        assert_eq!(logs.matches("WARN").count(), 2, "{label}: {logs:?}");
    }

    let (_, logs) = capturing_warn_logs(|| parse_managed_settings_json(&zero_entries, path));
    for needle in ["MCP lockdown", "marketplace lockdown", CLAUDE_PATH] {
        assert!(logs.contains(needle), "missing {needle:?} in: {logs:?}");
    }

    // Absent keys and populated lists stay quiet.
    let (_, logs) = capturing_warn_logs(|| {
        parse_managed_settings_json(
            &serde_json::json!({ "allowedMcpServers": [ { "serverUrl": "https://ok.example/*" } ] }),
            path,
        )
    });
    assert!(!logs.contains("lockdown"), "unexpected warning: {logs:?}");
}
