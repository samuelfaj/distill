//! Codebase index / code navigation methods (`workspace.code_*`).

use serde::{Deserialize, Serialize};

use super::{RpcActivityClass, WorkspaceRpc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeGotoDefinitionReq {
    #[serde(default)]
    pub root: Option<std::path::PathBuf>,
    pub file: String,
    pub line: usize,
    pub col: usize,
}

impl WorkspaceRpc for CodeGotoDefinitionReq {
    const METHOD: &'static str = "workspace.code_goto_definition";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = CodeNavResponse;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeGotoReferencesReq {
    #[serde(default)]
    pub root: Option<std::path::PathBuf>,
    pub file: String,
    pub line: usize,
    pub col: usize,
    #[serde(default)]
    pub include_definition: bool,
}

impl WorkspaceRpc for CodeGotoReferencesReq {
    const METHOD: &'static str = "workspace.code_goto_references";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = CodeNavResponse;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeFindDefinitionsReq {
    #[serde(default)]
    pub root: Option<std::path::PathBuf>,
    pub symbol: String,
    pub context_file: Option<String>,
}

impl WorkspaceRpc for CodeFindDefinitionsReq {
    const METHOD: &'static str = "workspace.code_find_definitions";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = CodeNavResponse;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeFindReferencesReq {
    #[serde(default)]
    pub root: Option<std::path::PathBuf>,
    pub symbol: String,
    pub context_file: Option<String>,
}

impl WorkspaceRpc for CodeFindReferencesReq {
    const METHOD: &'static str = "workspace.code_find_references";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = CodeNavResponse;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeIndexStatusReq {
    #[serde(default)]
    pub root: Option<std::path::PathBuf>,
}

impl WorkspaceRpc for CodeIndexStatusReq {
    const METHOD: &'static str = "workspace.code_index_status";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = CodeIndexStatusResponse;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeIndexStatusResponse {
    pub active: bool,
    pub file_count: Option<usize>,
    pub stats: Option<CodeIndexStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeIndexStats {
    pub files: usize,
    pub definitions: usize,
    pub references: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeNavLocation {
    pub path: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeNavResponse {
    pub locations: Vec<CodeNavLocation>,
}
