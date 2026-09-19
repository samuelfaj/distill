use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Ask {
        hook_name: String,
        reason: Option<String>,
    },
    Defer {
        hook_name: String,
    },
    Deny {
        hook_name: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDecision {
    Allow,
    Block { reason: String, hook_name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopHookOutcome {
    pub block_reason: Option<String>,
    pub additional_context: Option<String>,
    pub force_stop: Option<StopOverride>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopOverride {
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementKind {
    Builtin,
    Mcp,
}

impl ReplacementKind {
    pub fn wire_field(self) -> &'static str {
        match self {
            Self::Builtin => "updatedToolOutput",
            Self::Mcp => "updatedMCPToolOutput",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputReplacement {
    pub kind: ReplacementKind,
    pub hook_name: String,
    pub value: serde_json::Value,
}

impl OutputReplacement {
    pub fn wire_field(&self) -> &'static str {
        self.kind.wire_field()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PostToolUseHookOutcome {
    pub block_reason: Option<String>,
    pub additional_context: Option<String>,
    pub output_replacement: Option<OutputReplacement>,
}

impl PostToolUseHookOutcome {
    pub fn is_empty(&self) -> bool {
        self.block_reason.is_none()
            && self.additional_context.is_none()
            && self.output_replacement.is_none()
    }
}

impl StopHookOutcome {
    pub fn is_empty(&self) -> bool {
        self.block_reason.is_none()
            && self.additional_context.is_none()
            && self.force_stop.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct HttpInfo {
    pub expanded_url: String,
    pub source_url: Option<String>,
    pub status: Option<u16>,
    pub response_preview: Option<String>,
}

#[derive(Debug)]
pub enum HookRunResult {
    Success {
        hook_name: String,
        elapsed: Duration,
        http_info: Option<HttpInfo>,
        system_message: Option<String>,
    },
    Skipped {
        hook_name: String,
    },
    Blocked {
        hook_name: String,
        detail: String,
        elapsed: Duration,
        http_info: Option<HttpInfo>,
        system_message: Option<String>,
    },
    Failed {
        hook_name: String,
        error: String,
        elapsed: Duration,
        http_info: Option<HttpInfo>,
        system_message: Option<String>,
    },
}
