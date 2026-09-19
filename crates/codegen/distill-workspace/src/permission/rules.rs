use std::str::FromStr;

use crate::permission::types::{PatternMode, PermissionRule, PromptPolicy, RuleAction, ToolFilter};

/// Recognized `permissions.defaultMode` values.
/// Unknown strings fail `FromStr` and fall back to [`Self::Default`], but still claim their settings scope so a typo blocks a looser parent mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultPermissionMode {
    Default,
    AcceptEdits,
    Plan,
    /// Classifier-based auto mode: settings can select it, and it seeds the manager's auto flag with no separate `disableAutoMode` gate.
    Auto,
    DontAsk,
    BypassPermissions,
}

impl FromStr for DefaultPermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "acceptEdits" => Ok(Self::AcceptEdits),
            "plan" => Ok(Self::Plan),
            "auto" => Ok(Self::Auto),
            "dontAsk" => Ok(Self::DontAsk),
            "bypassPermissions" => Ok(Self::BypassPermissions),
            other => Err(other.to_string()),
        }
    }
}

impl DefaultPermissionMode {
    pub(crate) fn effects(self) -> DefaultModeEffects {
        match self {
            Self::AcceptEdits => DefaultModeEffects {
                accept_edits: true,
                ..Default::default()
            },
            Self::BypassPermissions => DefaultModeEffects {
                bypass_permissions: true,
                ..Default::default()
            },
            Self::Default | Self::Plan => DefaultModeEffects::default(),
            Self::DontAsk => DefaultModeEffects {
                prompt_policy: PromptPolicy::Deny,
                ..Default::default()
            },
            Self::Auto => DefaultModeEffects {
                prompt_policy: PromptPolicy::Auto,
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DefaultModeEffects {
    pub(crate) prompt_policy: PromptPolicy,
    pub(crate) accept_edits: bool,
    pub(crate) bypass_permissions: bool,
}

// ═════════════════════════════════════════════════════════════════════════════
// Error Type
// ═════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleParseError {
    /// Tool prefix is recognized but not supported (e.g., "EnterWorktree", "NotebookEdit", "NotebookRead").
    UnsupportedToolPrefix {
        prefix: String,
    },
    UnknownToolPrefix {
        prefix: String,
    },
    /// Rule string is malformed (e.g., missing closing paren).
    MalformedRule {
        detail: String,
    },
}

impl std::fmt::Display for RuleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuleParseError::UnsupportedToolPrefix { prefix } => {
                write!(f, "unsupported tool prefix: {}", prefix)
            }
            RuleParseError::UnknownToolPrefix { prefix } => {
                write!(f, "unknown tool prefix: {}", prefix)
            }
            RuleParseError::MalformedRule { detail } => {
                write!(f, "malformed rule: {}", detail)
            }
        }
    }
}

impl std::error::Error for RuleParseError {}

// ═════════════════════════════════════════════════════════════════════════════
// Rule Parser
// ═════════════════════════════════════════════════════════════════════════════

/// Parse a permission rule string into a native `PermissionRule`.
/// Unrecognized prefixes (`EnterWorktree`, `NotebookEdit`/`NotebookRead`, anything else) return `Err` and the rule is skipped; legacy `SendAgentMessage` still parses.
/// `WebFetch(domain:…)` matches the host, not a glob; bare tool names are wildcards; `.claude` `mcp__…` is rewritten onto Grok's unprefixed `<server>__<tool>` names.
pub fn parse_permission_rule(
    rule: &str,
    action: RuleAction,
) -> Result<PermissionRule, RuleParseError> {
    let rule = rule.trim();

    // Try to extract tool prefix: "ToolName(" ... ")"
    // Use escape-aware parsing to handle \( and \) in content.
    if let Some(open_paren) = find_first_unescaped(rule, b'(') {
        let Some(prefix) = rule.get(..open_paren) else {
            return Err(RuleParseError::MalformedRule {
                detail: "missing opening parenthesis".to_string(),
            });
        };
        let prefix_trimmed = prefix.trim();

        // Find last unescaped closing paren
        let Some(content_and_close) = rule.get(open_paren + 1..) else {
            return Err(RuleParseError::MalformedRule {
                detail: "missing closing parenthesis".to_string(),
            });
        };
        let close_paren = find_last_unescaped(content_and_close, b')').ok_or_else(|| {
            RuleParseError::MalformedRule {
                detail: "missing closing parenthesis".to_string(),
            }
        })?;

        let Some(raw_content) = content_and_close.get(..close_paren) else {
            return Err(RuleParseError::MalformedRule {
                detail: "missing closing parenthesis".to_string(),
            });
        };
        let raw_content = raw_content.trim();
        // Empty content or a standalone wildcard means a tool-wide rule
        let pattern = if raw_content.is_empty() || raw_content == "*" {
            String::new()
        } else {
            unescape_rule_content(raw_content)
        };

        let tool = match tool_name_to_filter(prefix_trimmed) {
            Some(f) => f,
            None if matches!(
                prefix_trimmed,
                "EnterWorktree" | "NotebookEdit" | "NotebookRead"
            ) =>
            {
                return Err(RuleParseError::UnsupportedToolPrefix {
                    prefix: prefix_trimmed.to_string(),
                });
            }
            None => {
                return Err(RuleParseError::UnknownToolPrefix {
                    prefix: prefix_trimmed.to_string(),
                });
            }
        };

        // `Bash(cmd:*)` means "commands starting with cmd"; as a glob it matches nothing.
        let pattern = if tool == ToolFilter::Bash {
            strip_bash_colon_wildcard(pattern)
        } else {
            pattern
        };

        let (pattern, pattern_mode) = strip_domain_prefix(pattern);

        let pattern_opt = if pattern.is_empty() {
            None
        } else {
            Some(pattern)
        };

        Ok(PermissionRule {
            action,
            tool,
            pattern: pattern_opt,
            pattern_mode,
        })
    } else {
        if matches!(rule, "EnterWorktree" | "NotebookEdit" | "NotebookRead") {
            return Err(RuleParseError::UnsupportedToolPrefix {
                prefix: rule.to_string(),
            });
        }

        if let Some(tool) = tool_name_to_filter(rule) {
            return Ok(PermissionRule {
                action,
                tool,
                pattern: None,
                pattern_mode: PatternMode::Glob,
            });
        }

        // `.claude` `mcp__<server>[__<tool>]` spelling: strip `mcp__` and rewrite onto Grok's unprefixed `<server>__<tool>` names
        // Otherwise the literal falls through to `ToolFilter::Any` and matches nothing; a bare `mcp__` still falls through
        if let Some(rest) = rule.strip_prefix("mcp__")
            && !rest.is_empty()
        {
            let pattern = if rest == "*" {
                // `*` covers every MCP tool, so the rule is tool-wide (no pattern)
                None
            } else if rest.contains("__") {
                // The rest is already `<server>__<tool>` (or `<server>__*`), the Grok qualified name, so use it verbatim
                // Server names may contain single underscores, but `__` only ever separates server from tool
                Some(rest.to_string())
            } else {
                // Server-only rule: cover every tool on that server.
                Some(format!("{rest}__*"))
            };
            return Ok(PermissionRule {
                action,
                tool: ToolFilter::Mcp,
                pattern,
                pattern_mode: PatternMode::Glob,
            });
        }

        let pattern_opt = if rule.is_empty() {
            None
        } else {
            Some(rule.to_string())
        };

        Ok(PermissionRule {
            action,
            tool: ToolFilter::Any,
            pattern: pattern_opt,
            pattern_mode: PatternMode::Glob,
        })
    }
}

pub(crate) fn tool_name_to_filter(name: &str) -> Option<ToolFilter> {
    match name {
        "Bash" => Some(ToolFilter::Bash),
        "Read" => Some(ToolFilter::Read),
        "Edit" | "Write" => Some(ToolFilter::Edit),
        "MCPTool" => Some(ToolFilter::Mcp),
        "Grep" | "Glob" => Some(ToolFilter::Grep),
        "WebFetch" => Some(ToolFilter::WebFetch),
        "WebSearch" => Some(ToolFilter::WebSearch),
        "AgentMessage" | "SendSubagentMessage" | "SendAgentMessage" => {
            Some(ToolFilter::AgentMessage)
        }
        _ => None,
    }
}

/// True if the byte at `pos` is NOT preceded by an odd number of backslashes.
pub(crate) fn is_unescaped(bytes: &[u8], pos: usize) -> bool {
    let mut backslashes = 0usize;
    let mut j = pos;
    while j > 0 && bytes.get(j - 1) == Some(&b'\\') {
        backslashes += 1;
        j -= 1;
    }
    backslashes.is_multiple_of(2)
}

pub(crate) fn find_first_unescaped(s: &str, target: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    bytes
        .iter()
        .enumerate()
        .find(|&(i, &b)| b == target && is_unescaped(bytes, i))
        .map(|(i, _)| i)
}

pub(crate) fn find_last_unescaped(s: &str, target: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    (0..bytes.len())
        .rev()
        .find(|&i| bytes.get(i) == Some(&target) && is_unescaped(bytes, i))
}

/// Unescape rule content: `\(` → `(`, `\)` → `)`, `\\` → `\`.
pub(crate) fn unescape_rule_content(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    // Order matters: unescape parens before backslashes (reverse of escaping).
    s.replace("\\(", "(")
        .replace("\\)", ")")
        .replace("\\\\", "\\")
}

pub(crate) fn strip_domain_prefix(pattern: String) -> (String, PatternMode) {
    match pattern.strip_prefix("domain:") {
        Some(domain) => (domain.to_string(), PatternMode::Domain),
        None => (pattern, PatternMode::Glob),
    }
}

/// A trailing `:*` turns the Bash pattern into the bare prefix before it; a `:*` anywhere else is literal.
/// The prefix matches raw, with no word-boundary check, the same way the evaluator prefix-matches every Bash literal.
pub(crate) fn strip_bash_colon_wildcard(pattern: String) -> String {
    match pattern.strip_suffix(":*") {
        Some(prefix) => prefix.to_string(),
        None => pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_message_filter_forms_including_legacy_alias() {
        for (rule_str, action) in [
            ("AgentMessage", RuleAction::Ask),
            ("SendSubagentMessage(*)", RuleAction::Ask),
            ("SendAgentMessage", RuleAction::Deny),
            ("SendAgentMessage", RuleAction::Ask),
            ("SendAgentMessage(*)", RuleAction::Deny),
            ("SendAgentMessage(*)", RuleAction::Ask),
        ] {
            let rule = parse_permission_rule(rule_str, action).unwrap();
            assert_eq!(rule.action, action, "{rule_str}");
            assert_eq!(rule.tool, ToolFilter::AgentMessage, "{rule_str}");
            assert!(rule.pattern.is_none(), "{rule_str}");
        }
    }

    #[test]
    fn parse_claude_mcp_rule_forms() {
        for (rule_str, expected_pattern) in [
            ("mcp__github", Some("github__*")),
            ("mcp__github__get_issue", Some("github__get_issue")),
            ("mcp__github__*", Some("github__*")),
            ("mcp__*", None),
        ] {
            let rule = parse_permission_rule(rule_str, RuleAction::Deny).unwrap();
            assert_eq!(rule.action, RuleAction::Deny, "{rule_str}");
            assert_eq!(rule.tool, ToolFilter::Mcp, "{rule_str}");
            assert_eq!(rule.pattern.as_deref(), expected_pattern, "{rule_str}");
            assert_eq!(rule.pattern_mode, PatternMode::Glob, "{rule_str}");
        }
    }
}
