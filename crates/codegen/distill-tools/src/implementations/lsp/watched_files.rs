//! Client-side `workspace/didChangeWatchedFiles`.
//!
//! Servers that do not see this capability create one OS `FileSystemWatcher`
//! per subdirectory of caches such as `~/.nuget/packages`. On Linux that is
//! one inotify watch per directory — tens or hundreds of thousands on a real
//! NuGet cache
//! ([dotnet/roslyn#82857](https://github.com/dotnet/roslyn/issues/82857)).
//!
//! We advertise the capability and accept `client/registerCapability` so the
//! server disables its own watchers. We do **not** arm OS watches. Out-of-
//! workspace globs (the NuGet cache, `dotnet/packs`) are accepted and then
//! ignored for delivery. Workspace mutations we already know about are
//! forwarded only when they match a live registration's glob and `WatchKind`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Edits that arrive before any in-workspace registration. Bootstrap replay
/// happens right after initialize, and `client/registerCapability` usually
/// follows; without this queue those first `.csproj` events disappear.
const MAX_PENDING_BEFORE_REGISTER: usize = 256;

use async_lsp::LanguageServer;
use async_lsp::lsp_types::{
    self, DidChangeWatchedFilesClientCapabilities, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, FileChangeType, FileEvent, FileSystemWatcher,
    GlobPattern, OneOf, Registration, Url, WatchKind,
};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::file_uri;

pub(crate) const METHOD: &str = "workspace/didChangeWatchedFiles";

/// What we tell the server during `initialize`.
pub(crate) fn client_capability() -> DidChangeWatchedFilesClientCapabilities {
    DidChangeWatchedFilesClientCapabilities {
        dynamic_registration: Some(true),
        relative_pattern_support: Some(true),
    }
}

struct Watcher {
    /// `None` means the glob is outside the workspace and must never match.
    matcher: Option<GlobSet>,
    /// `Some` = match `path` relative to this base (must be under it).
    /// `None` = match the absolute path against the glob as given.
    base: Option<PathBuf>,
    kind: WatchKind,
}

impl Watcher {
    fn matches(&self, path: &Path, typ: FileChangeType) -> bool {
        if !kind_includes(self.kind, typ) {
            return false;
        }
        let Some(matcher) = &self.matcher else {
            return false;
        };
        let candidate = match &self.base {
            Some(base) => {
                let Ok(rel) = path.strip_prefix(base) else {
                    return false;
                };
                rel.to_path_buf()
            }
            None => path.to_path_buf(),
        };
        let unix = candidate.to_string_lossy().replace('\\', "/");
        matcher.is_match(unix)
    }
}

struct PendingChange {
    path: PathBuf,
    typ: FileChangeType,
}

struct Inner {
    by_id: HashMap<String, Vec<Watcher>>,
    pending: VecDeque<PendingChange>,
}

/// Live `workspace/didChangeWatchedFiles` registrations for one server. Keyed by registration id so
/// unregister is exact. Delivery consults the stored globs and `WatchKind`; we never turn a
/// registration into an OS watch.
#[derive(Clone)]
pub(crate) struct WatchedFiles {
    workspace_root: Arc<PathBuf>,
    inner: Arc<parking_lot::Mutex<Inner>>,
}

impl std::fmt::Debug for WatchedFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchedFiles")
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

impl WatchedFiles {
    pub(crate) fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root: Arc::new(workspace_root),
            inner: Arc::new(parking_lot::Mutex::new(Inner {
                by_id: HashMap::new(),
                pending: VecDeque::new(),
            })),
        }
    }

    pub(crate) fn accept(&self, registrations: &[Registration]) -> Vec<(PathBuf, FileChangeType)> {
        let mut inner = self.inner.lock();
        for registration in registrations {
            if registration.method != METHOD {
                continue;
            }
            let watchers = self.parse_watchers(registration.register_options.as_ref());
            tracing::debug!(
                id = %registration.id,
                watchers = watchers.len(),
                "accepted workspace/didChangeWatchedFiles without arming an OS watcher"
            );
            inner.by_id.insert(registration.id.clone(), watchers);
        }
        drain_matching_pending(&mut inner)
    }

    pub(crate) fn forget(&self, ids: impl Iterator<Item = impl AsRef<str>>) {
        let mut inner = self.inner.lock();
        for id in ids {
            inner.by_id.remove(id.as_ref());
        }
    }

    pub(crate) fn watches(&self, path: &Path, typ: FileChangeType) -> bool {
        let inner = self.inner.lock();
        watchers_match(&inner, path, typ)
    }

    /// Hold the event until an in-workspace registration exists. Returns true
    /// when the caller must not send yet.
    fn hold_if_unregistered(&self, path: &Path, typ: FileChangeType) -> bool {
        let mut inner = self.inner.lock();
        if has_deliverable(&inner) {
            return false;
        }
        if inner.pending.len() >= MAX_PENDING_BEFORE_REGISTER {
            inner.pending.pop_front();
        }
        inner.pending.push_back(PendingChange {
            path: path.to_path_buf(),
            typ,
        });
        true
    }

    #[cfg(test)]
    pub(crate) fn accepted(&self) -> usize {
        self.inner.lock().by_id.len()
    }

    fn parse_watchers(&self, options: Option<&serde_json::Value>) -> Vec<Watcher> {
        let Some(options) = options else {
            return vec![self.workspace_catch_all()];
        };
        let Ok(parsed) =
            serde_json::from_value::<DidChangeWatchedFilesRegistrationOptions>(options.clone())
        else {
            // Unparseable options still have to be accepted or the server
            // falls back to its own FileSystemWatcher. Deliver every
            // workspace event rather than go silent.
            return vec![self.workspace_catch_all()];
        };
        if parsed.watchers.is_empty() {
            return vec![self.workspace_catch_all()];
        }
        parsed
            .watchers
            .into_iter()
            .map(|watcher| self.compile_watcher(watcher))
            .collect()
    }

    fn compile_watcher(&self, watcher: FileSystemWatcher) -> Watcher {
        let kind = watcher.kind.unwrap_or(WatchKind::all());
        let (base, pattern, in_workspace) = match watcher.glob_pattern {
            GlobPattern::String(pattern) => {
                if Path::new(&pattern).is_absolute() {
                    let in_workspace = is_under(&self.workspace_root, Path::new(&pattern));
                    // Match the absolute path against the glob as written.
                    // Stripping a "/" base would drop the leading slash and
                    // the glob would never hit.
                    (None, pattern, in_workspace)
                } else {
                    (
                        Some(self.workspace_root.as_path().to_path_buf()),
                        pattern,
                        true,
                    )
                }
            }
            GlobPattern::Relative(relative) => {
                let base = base_uri_path(&relative.base_uri)
                    .unwrap_or_else(|| self.workspace_root.as_path().to_path_buf());
                let in_workspace = is_under(&self.workspace_root, &base);
                (Some(base), relative.pattern, in_workspace)
            }
        };
        if !in_workspace {
            return Watcher {
                matcher: None,
                base,
                kind,
            };
        }
        Watcher {
            matcher: compile_globset(&pattern),
            base,
            kind,
        }
    }

    fn workspace_catch_all(&self) -> Watcher {
        Watcher {
            matcher: compile_globset("**/*"),
            base: Some(self.workspace_root.as_path().to_path_buf()),
            kind: WatchKind::all(),
        }
    }
}

/// Tell the server a workspace path changed, if a live registration matches.
pub(crate) fn notify_path_changed(
    socket: &mut async_lsp::ServerSocket,
    watched: &WatchedFiles,
    path: &Path,
    typ: FileChangeType,
) {
    if watched.hold_if_unregistered(path, typ) {
        return;
    }
    if !watched.watches(path, typ) {
        return;
    }
    send_watched(socket, path, typ);
}

fn send_watched(socket: &mut async_lsp::ServerSocket, path: &Path, typ: FileChangeType) {
    let Ok(uri) = file_uri(path) else {
        return;
    };
    if let Err(e) = socket.did_change_watched_files(DidChangeWatchedFilesParams {
        changes: vec![FileEvent { uri, typ }],
    }) {
        tracing::debug!(error = %e, "failed to send workspace/didChangeWatchedFiles");
    }
}

/// `client/registerCapability` as the router sees it. Only `workspace/didChangeWatchedFiles` is
/// implemented. Anything else is rejected so the server does not think an unimplemented capability
/// is live.
pub(crate) fn accept_register_capability(
    watched: &WatchedFiles,
    params: lsp_types::RegistrationParams,
) -> Result<Vec<(PathBuf, FileChangeType)>, async_lsp::ResponseError> {
    let unsupported: Vec<&str> = params
        .registrations
        .iter()
        .filter(|registration| registration.method != METHOD)
        .map(|registration| registration.method.as_str())
        .collect();
    if !unsupported.is_empty() {
        return Err(async_lsp::ResponseError::new(
            async_lsp::ErrorCode::METHOD_NOT_FOUND,
            format!(
                "unsupported client/registerCapability methods: {}",
                unsupported.join(", ")
            ),
        ));
    }
    Ok(watched.accept(&params.registrations))
}

fn has_deliverable(inner: &Inner) -> bool {
    inner
        .by_id
        .values()
        .flatten()
        .any(|watcher| watcher.matcher.is_some())
}

fn watchers_match(inner: &Inner, path: &Path, typ: FileChangeType) -> bool {
    inner
        .by_id
        .values()
        .flatten()
        .any(|watcher| watcher.matches(path, typ))
}

fn drain_matching_pending(inner: &mut Inner) -> Vec<(PathBuf, FileChangeType)> {
    let pending = std::mem::take(&mut inner.pending);
    let mut kept = VecDeque::new();
    let mut matched = Vec::new();
    for event in pending {
        if watchers_match(inner, &event.path, event.typ) {
            matched.push((event.path, event.typ));
        } else {
            // A first `**/*.cs` must not drop a held `.csproj`; later
            // registrations still get a chance, until the cap evicts.
            kept.push_back(event);
        }
    }
    inner.pending = kept;
    matched
}

pub(crate) fn accept_unregister_capability(
    watched: &WatchedFiles,
    params: lsp_types::UnregistrationParams,
) {
    watched.forget(params.unregisterations.iter().map(|u| u.id.as_str()));
}

fn kind_includes(kind: WatchKind, typ: FileChangeType) -> bool {
    if typ == FileChangeType::CREATED {
        kind.contains(WatchKind::Create)
    } else if typ == FileChangeType::CHANGED {
        kind.contains(WatchKind::Change)
    } else if typ == FileChangeType::DELETED {
        kind.contains(WatchKind::Delete)
    } else {
        true
    }
}

fn is_under(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

fn base_uri_path(base: &OneOf<lsp_types::WorkspaceFolder, Url>) -> Option<PathBuf> {
    let url = match base {
        OneOf::Left(folder) => &folder.uri,
        OneOf::Right(url) => url,
    };
    url.to_file_path().ok()
}

fn compile_globset(pattern: &str) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for expanded in expand_braces(pattern) {
        let glob = GlobBuilder::new(&expanded)
            .literal_separator(true)
            .build()
            .ok()?;
        builder.add(glob);
    }
    builder.build().ok()
}

/// One-level `{a,b}` expansion so `**/*.{cs,csproj}` compiles under globset.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(start) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(end_rel) = pattern.get(start + 1..).and_then(|rest| rest.find('}')) else {
        return vec![pattern.to_string()];
    };
    let end = start + 1 + end_rel;
    let Some(prefix) = pattern.get(..start) else {
        return vec![pattern.to_string()];
    };
    let Some(suffix) = pattern.get(end + 1..) else {
        return vec![pattern.to_string()];
    };
    let mut out = Vec::new();
    let Some(alts) = pattern.get(start + 1..end) else {
        return vec![pattern.to_string()];
    };
    for alt in alts.split(',') {
        out.extend(expand_braces(&format!("{prefix}{alt}{suffix}")));
    }
    if out.is_empty() {
        vec![pattern.to_string()]
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_lsp::lsp_types::{Registration, Unregistration, UnregistrationParams};

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn file_watch(id: &str, glob: serde_json::Value) -> Registration {
        Registration {
            id: id.into(),
            method: METHOD.into(),
            register_options: Some(serde_json::json!({
                "watchers": [{ "globPattern": glob }]
            })),
        }
    }

    #[test]
    fn advertise_dynamic_registration_and_relative_patterns() {
        let cap = client_capability();
        assert_eq!(cap.dynamic_registration, Some(true));
        assert_eq!(cap.relative_pattern_support, Some(true));
    }

    #[test]
    fn only_file_watch_registrations_are_kept_and_unregister_is_by_id() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        watched.accept(&[
            file_watch("cs", serde_json::json!("**/*.cs")),
            Registration {
                id: "diag".into(),
                method: "textDocument/diagnostic".into(),
                register_options: None,
            },
            file_watch("proj", serde_json::json!("**/*.csproj")),
        ]);
        assert_eq!(watched.accepted(), 2);

        watched.forget(std::iter::once("missing"));
        assert_eq!(watched.accepted(), 2);
        watched.forget(std::iter::once("cs"));
        assert_eq!(watched.accepted(), 1);
        assert!(watched.watches(&root.path().join("App.csproj"), FileChangeType::CHANGED));
        assert!(!watched.watches(&root.path().join("App.cs"), FileChangeType::CHANGED));
    }

    #[test]
    fn out_of_workspace_globs_never_match() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        watched.accept(&[file_watch(
            "nuget",
            serde_json::json!({
                "baseUri": "file:///tmp/fake-nuget/packages",
                "pattern": "**/*.dll"
            }),
        )]);
        assert_eq!(watched.accepted(), 1);
        assert!(!watched.watches(&root.path().join("App.dll"), FileChangeType::CHANGED));
        assert!(!watched.watches(
            Path::new("/tmp/fake-nuget/packages/foo/lib.dll"),
            FileChangeType::CHANGED
        ));
    }

    #[test]
    fn workspace_glob_and_kind_gate_delivery() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        watched.accept(&[Registration {
            id: "ts".into(),
            method: METHOD.into(),
            register_options: Some(serde_json::json!({
                "watchers": [{
                    "globPattern": "**/*.{ts,tsx}",
                    "kind": 3
                }]
            })),
        }]);
        let ts = root.path().join("src/app.ts");
        assert!(watched.watches(&ts, FileChangeType::CREATED));
        assert!(watched.watches(&ts, FileChangeType::CHANGED));
        assert!(!watched.watches(&ts, FileChangeType::DELETED));
        assert!(!watched.watches(&root.path().join("src/app.rs"), FileChangeType::CHANGED));
        assert!(
            !watched.watches(Path::new("/tmp/elsewhere/app.ts"), FileChangeType::CHANGED),
            "a relative glob must not match a path outside its base"
        );
    }

    #[test]
    fn absolute_workspace_glob_matches_the_full_path() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        let glob = format!("{}/**/*.cs", root.path().display());
        watched.accept(&[file_watch("abs", serde_json::json!(glob))]);
        assert!(watched.watches(&root.path().join("App.cs"), FileChangeType::CHANGED));
        assert!(!watched.watches(&root.path().join("App.rs"), FileChangeType::CHANGED));
    }

    #[test]
    fn unsupported_register_capability_is_rejected() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        let err = accept_register_capability(
            &watched,
            lsp_types::RegistrationParams {
                registrations: vec![Registration {
                    id: "diag".into(),
                    method: "textDocument/diagnostic".into(),
                    register_options: None,
                }],
            },
        )
        .expect_err("unsupported method");
        assert_eq!(err.code, async_lsp::ErrorCode::METHOD_NOT_FOUND);
        assert_eq!(watched.accepted(), 0);
    }

    #[test]
    fn unregister_uses_registration_id() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        accept_register_capability(
            &watched,
            lsp_types::RegistrationParams {
                registrations: vec![file_watch("keep", serde_json::json!("**/*.cs"))],
            },
        )
        .unwrap();
        accept_unregister_capability(
            &watched,
            UnregistrationParams {
                unregisterations: vec![Unregistration {
                    id: "missing".into(),
                    method: METHOD.into(),
                }],
            },
        );
        assert_eq!(watched.accepted(), 1);
        accept_unregister_capability(
            &watched,
            UnregistrationParams {
                unregisterations: vec![Unregistration {
                    id: "keep".into(),
                    method: METHOD.into(),
                }],
            },
        );
        assert_eq!(watched.accepted(), 0);
    }

    #[test]
    fn edits_before_any_workspace_registration_are_replayed() {
        let root = workspace();
        let watched = WatchedFiles::new(root.path().to_path_buf());
        let csproj = root.path().join("App.csproj");
        assert!(watched.hold_if_unregistered(&csproj, FileChangeType::CHANGED));
        let flushed = watched.accept(&[file_watch(
            "nuget",
            serde_json::json!({
                "baseUri": "file:///tmp/fake-nuget/packages",
                "pattern": "**/*.dll"
            }),
        )]);
        assert!(
            flushed.is_empty(),
            "out-of-workspace globs must not flush workspace pending"
        );
        let flushed = watched.accept(&[file_watch("cs", serde_json::json!("**/*.cs"))]);
        assert!(
            flushed.is_empty(),
            "a first **/*.cs must not drop a held .csproj"
        );
        let flushed = watched.accept(&[file_watch("proj", serde_json::json!("**/*.csproj"))]);
        assert_eq!(flushed.len(), 1);
        let Some((path, _)) = flushed.first() else {
            panic!("expected flushed csproj: {flushed:?}");
        };
        assert_eq!(path, &csproj);
    }

    #[test]
    fn relative_patterns_use_the_server_workspace_folder() {
        let session = workspace();
        let folder = workspace();
        let watched = WatchedFiles::new(folder.path().to_path_buf());
        let uri = Url::from_file_path(folder.path()).expect("folder uri");
        watched.accept(&[file_watch(
            "cs",
            serde_json::json!({
                "baseUri": uri.as_str(),
                "pattern": "**/*.cs"
            }),
        )]);
        assert!(watched.watches(&folder.path().join("App.cs"), FileChangeType::CHANGED));
        assert!(!watched.watches(&session.path().join("App.cs"), FileChangeType::CHANGED));
    }
}
