//! File I/O methods: the service-level `workspace.put_files` / `workspace.get_files` pair and the `workspace.fs_*` extension ops.
//! The `workspace.fs_*` ops back the shell's `x.ai/fs/*` ACP methods.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{RpcActivityClass, WorkspaceRpc};

/// A single file entry to write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutFileEntry {
    /// Path relative to the client-fs base (the bound session's cwd when it extends the workspace root, else the root); escapes are rejected.
    pub path: String,
    /// UTF-8 file content (one chunk).
    pub content: String,
    /// If true, create parent directories as needed (default: true).
    #[serde(default = "default_true")]
    pub create_dirs: bool,
    /// If true, append instead of overwrite. Default false.
    /// Chunked writes: first chunk `append: false` (create/truncate), later chunks `append: true`, so a large file need not be held entirely in memory.
    #[serde(default)]
    pub append: bool,
}

/// Write one or more files. Service-level: not hunk-tracked and not visible to the model.
/// Non-transactional: a failure leaves earlier files written. Callers must inspect per-file results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutFilesReq {
    pub files: Vec<PutFileEntry>,
}

impl WorkspaceRpc for PutFilesReq {
    const METHOD: &'static str = "workspace.put_files";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Mutation;
    type Response = PutFilesRes;
}

/// Per-file result from a put_files operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutFileResult {
    /// The request path, echoed back.
    pub path: String,
    /// Whether this file was successfully written.
    pub ok: bool,
    /// Error message if `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// SHA-256 hex digest of the content that was written in this call (only set if ok).
    /// For `append: true`, this is the hash of the appended chunk, not the full file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutFilesRes {
    pub results: Vec<PutFileResult>,
}

/// Leaves room for base64 expansion and RPC metadata within the hub's default 8 MiB frame limit.
pub const MAX_SESSION_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_SESSION_IMAGE_BASE64_BYTES: usize = MAX_SESSION_IMAGE_BYTES.div_ceil(3) * 4;

/// Store a generated image in the bound session's own folder, never in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSessionImageReq {
    /// Standard base64-encoded image bytes.
    pub content_base64: String,
    /// Filename extension: jpg, png, webp, or gif.
    pub extension: String,
}

impl WorkspaceRpc for StoreSessionImageReq {
    const METHOD: &'static str = "workspace.store_session_image";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Mutation;
    type Response = StoreSessionImageRes;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSessionImageRes {
    /// Absolute host path, returned only after the complete image has been stored.
    pub file_path: String,
}

/// A single file to read, with optional cache validation and byte-range support.
/// Ranges are bytes but `content` is a UTF-8 `String`; a split codepoint errors rather than emitting invalid text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFileEntry {
    /// Path relative to the client-fs base (see [`PutFileEntry::path`]).
    pub path: String,
    /// If set, the server compares this hash against the full-file content hash.
    /// If they match, the content field in the response is `None` (cache hit).
    /// Works for both full-file and chunked reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_none_match: Option<String>,
    /// Byte offset to start reading from (default: 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Maximum number of bytes to read (default: entire file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

/// Request to read one or more files from the workspace filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFilesReq {
    pub files: Vec<GetFileEntry>,
}

impl WorkspaceRpc for GetFilesReq {
    const METHOD: &'static str = "workspace.get_files";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = GetFilesRes;
}

/// Per-file result from a get_files operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFileResult {
    /// The requested path (echoed back).
    pub path: String,
    pub exists: bool,
    /// File content (full file or requested byte range as UTF-8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// SHA-256 hex digest of the full-file content (streaming, no buffering).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// True if `if_none_match` matched the current hash.
    #[serde(default)]
    pub matched: bool,
    /// Total file size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Error message if the read failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFilesRes {
    pub results: Vec<GetFileResult>,
}

// Response types: serde shapes match the shell's `session::file_system` types byte-for-byte so the ACP wire contract is unchanged

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsListNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_symlink: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsListData {
    pub nodes: Vec<FsListNode>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsExistsData {
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileData {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_count: Option<u64>,
    #[serde(rename = "type")]
    pub content_type: String,
}

fn default_depth() -> usize {
    1
}
fn default_limit() -> usize {
    1000
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsListReq {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Pagination offset applied after the dirs-first / case-insensitive sort (default 0).
    /// When `offset > 0` (or the directory exceeds `limit`) the server collects the whole walk and sorts it.
    /// It then returns the stable slice `[offset, offset + limit)`.
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_true")]
    pub include_hidden: bool,
    #[serde(default = "default_true")]
    pub follow_symlinks: bool,
    #[serde(default = "default_true")]
    pub respect_git_ignore: bool,
    #[serde(default)]
    pub include_globs: Vec<String>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
}

impl WorkspaceRpc for FsListReq {
    const METHOD: &'static str = "workspace.fs_list";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = FsListData;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsExistsReq {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl WorkspaceRpc for FsExistsReq {
    const METHOD: &'static str = "workspace.fs_exists";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = FsExistsData;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadFileReq {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Byte offset to start reading from.
    /// When `offset` or `length` is set (or `encoding` is `base64`) the read is a binary-safe ranged read.
    /// When all are absent the whole file is read.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Bytes to read (absent means to EOF). Only for ranged reads, always capped at `max_bytes` and the server hard limit.
    /// Detect more data by comparing returned bytes from `offset` against `size`.
    #[serde(default)]
    pub length: Option<u64>,
    /// Per-chunk byte budget applied on top of `length` (default 1 MiB).
    /// Further clamped server-side so a single chunk fits the hub frame after base64.
    /// Only consulted for ranged reads.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Transfer encoding for ranged reads (default `utf8`; non-UTF-8 ranges fall back to base64 regardless of this setting).
    #[serde(default)]
    pub encoding: FsReadEncoding,
}

impl WorkspaceRpc for FsReadFileReq {
    const METHOD: &'static str = "workspace.fs_read_file";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = FsReadFileData;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsWriteFileReq {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub content: String,
    #[serde(default = "default_true")]
    pub create_dirs: bool,
}

impl WorkspaceRpc for FsWriteFileReq {
    const METHOD: &'static str = "workspace.fs_write_file";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Mutation;
    type Response = ();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsDeleteFileReq {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl WorkspaceRpc for FsDeleteFileReq {
    const METHOD: &'static str = "workspace.fs_delete_file";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Mutation;
    type Response = ();
}

// Client-facing `workspace.client_fs_*`: root-relative paths, `mtimeMs`, post-sort pagination, binary-safe base64. Not the shell `workspace.fs_*` schema.
// camelCase, fixed-width integers only. Server and grok.com backend share these structs; a field rename breaks both.

/// Wire method name for [`ClientFsListReq`].
pub const CLIENT_FS_LIST_METHOD: &str = "workspace.client_fs_list";
/// Wire method name for [`ClientFsStatReq`].
pub const CLIENT_FS_STAT_METHOD: &str = "workspace.client_fs_stat";
/// Wire method name for [`ClientFsReadFileReq`].
pub const CLIENT_FS_READ_FILE_METHOD: &str = "workspace.client_fs_read_file";

/// Filesystem node kind. Wire values match the shell's `x.ai/fs/list` node `type` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsNodeType {
    Directory,
    /// A regular file (or anything that is not a directory).
    File,
}

/// Requested content transfer encoding for [`ClientFsReadFileReq`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsReadEncoding {
    /// UTF-8 text in `content` (shell-compatible default).
    /// Falls back to base64 when the requested byte range is not valid UTF-8.
    #[default]
    Utf8,
    /// Base64 in `contentBase64` (binary-safe; chunked readers use this).
    Base64,
}

/// Whether the returned payload bytes were valid UTF-8 text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsContentType {
    Text,
    Binary,
}

fn default_client_depth() -> u32 {
    1
}
fn default_client_limit() -> u32 {
    1000
}
fn default_max_bytes() -> u64 {
    1_048_576
}

/// ACP-compatible list request (camelCase wire format, mirrors `x.ai/fs/list` plus `offset` pagination).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsListReq {
    /// Path relative to the client-fs base; escapes are rejected.
    /// `""` or `"."` mean the base itself: the bound session's cwd when it extends the workspace root, else the root.
    pub path: String,
    /// Walk depth below `path` (1 means immediate children).
    #[serde(default = "default_client_depth")]
    pub depth: u32,
    /// Include dotfiles.
    #[serde(default = "default_true")]
    pub include_hidden: bool,
    /// Maximum entries per page; the server caps this at 1000.
    #[serde(default = "default_client_limit")]
    pub limit: u32,
    /// Pagination offset, applied after the dirs-first case-insensitive sort (divergent from the shell, which has no offset).
    #[serde(default)]
    pub offset: u64,
    /// Follow symlinks while walking.
    #[serde(default = "default_true")]
    pub follow_symlinks: bool,
    /// Apply gitignore-style filters while walking.
    #[serde(default = "default_true")]
    pub respect_git_ignore: bool,
    /// Glob allowlist (empty means everything).
    #[serde(default)]
    pub include_globs: Vec<String>,
    /// Glob denylist.
    #[serde(default)]
    pub exclude_globs: Vec<String>,
}

impl WorkspaceRpc for ClientFsListReq {
    const METHOD: &'static str = CLIENT_FS_LIST_METHOD;
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = ClientFsListRes;
}

/// One listed node. Shell-aligned except `path` (client-fs-base-relative) and `mtimeMs` (epoch millis).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsListNode {
    /// File name (final path component).
    pub name: String,
    /// Path relative to the client-fs base (divergent: shell is absolute).
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: FsNodeType,
    /// `Some(true)` when the entry itself is a symlink; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_symlink: Option<bool>,
    /// File size in bytes (files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Modification time as epoch milliseconds (divergent: shell sends RFC 3339 `modifiedAt`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}

/// Response for [`ClientFsListReq`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsListRes {
    /// One page of nodes (post-sort slice `[offset, offset + limit)`).
    pub nodes: Vec<ClientFsListNode>,
    /// `true` when more entries exist beyond this page, or when the server's collection cap was hit before the walk finished.
    pub truncated: bool,
}

/// Stat request: existence, metadata, and a content hash for one path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsStatReq {
    /// Path relative to the client-fs base (see [`ClientFsListReq::path`]).
    pub path: String,
}

impl WorkspaceRpc for ClientFsStatReq {
    const METHOD: &'static str = CLIENT_FS_STAT_METHOD;
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = ClientFsStatRes;
}

/// Response for [`ClientFsStatReq`].
/// A missing path is `exists: false` with all other fields absent (not an RPC error).
/// That includes a path whose intermediate component is a file rather than a directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsStatRes {
    pub exists: bool,
    /// Node kind, when the path exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_type: Option<FsNodeType>,
    /// File size in bytes (files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Modification time as epoch milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    /// SHA-256 hex digest of the full content (files only).
    /// It keys the backend's content-addressed write-through cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// Binary-safe chunked read request.
/// Unlike `workspace.get_files`, byte ranges need no UTF-8 alignment: chunks transfer as base64.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsReadFileReq {
    /// Path relative to the client-fs base (see [`ClientFsListReq::path`]).
    pub path: String,
    /// Byte offset to start reading from (default 0).
    #[serde(default)]
    pub offset: Option<u64>,
    /// Bytes to read (absent means "to EOF"), always capped at `max_bytes` and the server's hard limit.
    /// An unset `length` still returns at most `max_bytes`.
    /// Detect "more data" by comparing the returned bytes (from `offset`) against `size`.
    #[serde(default)]
    pub length: Option<u64>,
    /// Per-chunk byte cap applied on top of `length` (default 1 MiB).
    /// The server additionally clamps the effective budget to 4 MiB so a single chunk always fits the hub's 8 MiB frame cap after base64.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Transfer encoding (default `utf8`, shell-compatible; chunked binary readers request `base64`).
    #[serde(default)]
    pub encoding: FsReadEncoding,
}

impl WorkspaceRpc for ClientFsReadFileReq {
    const METHOD: &'static str = CLIENT_FS_READ_FILE_METHOD;
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = ClientFsReadFileRes;
}

/// Response for [`ClientFsReadFileReq`].
/// Exactly one of `content` / `contentBase64` is populated, matching `type`.
/// `text` fills `content` (unless base64 was requested); `binary` fills `contentBase64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsReadFileRes {
    /// UTF-8 payload (only when `utf8` was requested and the range is valid UTF-8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Base64 payload (when `base64` was requested, or as the fallback for non-UTF-8 ranges).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
    /// Total file size in bytes (not the chunk length).
    pub size: u64,
    /// SHA-256 hex digest of the **full** file content.
    pub hash: String,
    /// Whether the returned payload bytes were valid UTF-8.
    #[serde(rename = "type")]
    pub content_type: FsContentType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_list_req_defaults_apply() {
        let req: FsListReq = serde_json::from_value(serde_json::json!({"path": "."})).unwrap();
        assert_eq!(req.depth, 1);
        assert_eq!(req.limit, 1000);
        assert_eq!(req.offset, 0);
        assert!(req.include_hidden);
        assert!(req.follow_symlinks);
        assert!(req.respect_git_ignore);
    }

    #[test]
    fn fs_read_file_req_defaults_are_legacy_full_read() {
        let req: FsReadFileReq =
            serde_json::from_value(serde_json::json!({ "path": "a.txt" })).unwrap();
        assert_eq!(req.offset, None);
        assert_eq!(req.length, None);
        assert_eq!(req.max_bytes, 1_048_576);
        assert_eq!(req.encoding, FsReadEncoding::Utf8);
        let req: FsReadFileReq = serde_json::from_value(serde_json::json!({
            "path": "a.bin", "offset": 4096, "length": 1024, "encoding": "base64"
        }))
        .unwrap();
        assert_eq!(req.offset, Some(4096));
        assert_eq!(req.length, Some(1024));
        assert_eq!(req.encoding, FsReadEncoding::Base64);
    }

    #[test]
    fn fs_list_node_renames_type_key() {
        let node = FsListNode {
            name: "a".into(),
            path: "/a".into(),
            node_type: "file".into(),
            is_symlink: None,
            size: Some(1),
            modified_at: None,
        };
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("file"));
        assert!(json.get("isSymlink").is_none());
        assert!(json.get("modifiedAt").is_none());
    }

    /// Pins the serialized JSON form of the client-facing `client_fs_*` types so field renames or serde-default changes fail loudly.
    /// The one real wire risk is cross-version skew between a backend and an older workspace image.
    #[test]
    fn client_fs_wire_stability_snapshot() {
        use serde_json::json;

        let list_req: ClientFsListReq = serde_json::from_value(json!({ "path": "docs" })).unwrap();
        assert_eq!(
            list_req,
            ClientFsListReq {
                path: "docs".into(),
                depth: 1,
                include_hidden: true,
                limit: 1000,
                offset: 0,
                follow_symlinks: true,
                respect_git_ignore: true,
                include_globs: vec![],
                exclude_globs: vec![],
            }
        );
        let read_req: ClientFsReadFileReq =
            serde_json::from_value(json!({ "path": "a.bin" })).unwrap();
        assert_eq!(
            read_req,
            ClientFsReadFileReq {
                path: "a.bin".into(),
                offset: None,
                length: None,
                max_bytes: 1_048_576,
                encoding: FsReadEncoding::Utf8,
            }
        );

        let list_req = ClientFsListReq {
            path: "docs".into(),
            depth: 2,
            include_hidden: false,
            limit: 100,
            offset: 200,
            follow_symlinks: false,
            respect_git_ignore: false,
            include_globs: vec!["*.md".into()],
            exclude_globs: vec![".git".into()],
        };
        assert_eq!(
            serde_json::to_value(&list_req).unwrap(),
            json!({
                "path": "docs",
                "depth": 2,
                "includeHidden": false,
                "limit": 100,
                "offset": 200,
                "followSymlinks": false,
                "respectGitIgnore": false,
                "includeGlobs": ["*.md"],
                "excludeGlobs": [".git"],
            })
        );
        assert_eq!(
            serde_json::to_value(ClientFsStatReq {
                path: "a.txt".into()
            })
            .unwrap(),
            json!({ "path": "a.txt" })
        );
        let read_req = ClientFsReadFileReq {
            path: "a.bin".into(),
            offset: Some(2_097_152),
            length: Some(2_097_152),
            max_bytes: 2_097_152,
            encoding: FsReadEncoding::Base64,
        };
        assert_eq!(
            serde_json::to_value(&read_req).unwrap(),
            json!({
                "path": "a.bin",
                "offset": 2_097_152,
                "length": 2_097_152,
                "maxBytes": 2_097_152,
                "encoding": "base64",
            })
        );

        let list_res = ClientFsListRes {
            nodes: vec![ClientFsListNode {
                name: "a.txt".into(),
                path: "docs/a.txt".into(),
                node_type: FsNodeType::File,
                is_symlink: Some(true),
                size: Some(11),
                mtime_ms: Some(1_700_000_000_000),
            }],
            truncated: true,
        };
        assert_eq!(
            serde_json::to_value(&list_res).unwrap(),
            json!({
                "nodes": [{
                    "name": "a.txt",
                    "path": "docs/a.txt",
                    "type": "file",
                    "isSymlink": true,
                    "size": 11,
                    "mtimeMs": 1_700_000_000_000_i64,
                }],
                "truncated": true,
            })
        );
        let missing = ClientFsStatRes {
            exists: false,
            node_type: None,
            size: None,
            mtime_ms: None,
            hash: None,
        };
        assert_eq!(
            serde_json::to_value(&missing).unwrap(),
            json!({ "exists": false })
        );
        let read_res = ClientFsReadFileRes {
            content: None,
            content_base64: Some("aGVsbG8=".into()),
            size: 5,
            hash: "abc123".into(),
            content_type: FsContentType::Binary,
        };
        assert_eq!(
            serde_json::to_value(&read_res).unwrap(),
            json!({
                "contentBase64": "aGVsbG8=",
                "size": 5,
                "hash": "abc123",
                "type": "binary",
            })
        );
    }
}
