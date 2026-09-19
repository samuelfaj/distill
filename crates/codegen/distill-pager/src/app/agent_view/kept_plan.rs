//! Waiting CreatePlan keep. Set by `PlanKept`, cleared by `PlanCleared`.

use std::path::{Path, PathBuf};

use crate::app::agent_view::capped_kept_plan_body;

#[derive(Debug, Clone, Default)]
pub(crate) struct KeptPlan {
    body: Option<String>,
    path: Option<PathBuf>,
}

impl KeptPlan {
    pub(crate) fn kept(body: Option<String>, path: Option<PathBuf>) -> Self {
        KeptPlan { body, path }
    }

    pub(crate) fn from_signal(plan_uri: &str, content: String) -> Self {
        let path = url::Url::parse(plan_uri)
            .ok()
            .and_then(|uri| uri.to_file_path().ok());
        let body = capped_kept_plan_body(content);
        KeptPlan { body, path }
    }

    pub(crate) fn is_kept(&self) -> bool {
        usable(&self.body, &self.path)
    }

    /// Chip / open-preview gate. Does not read file bytes.
    pub(crate) fn preview_available(&self) -> bool {
        self.body
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || self.path.as_deref().is_some_and(Path::is_file)
    }

    #[cfg(test)]
    pub(crate) fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn file_uri(&self) -> Option<String> {
        self.path()
            .and_then(|path| url::Url::from_file_path(path).ok())
            .map(|url| url.to_string())
    }

    pub(crate) fn clear(&mut self) {
        *self = KeptPlan::default();
    }

    pub(crate) fn clear_body(&mut self) {
        self.body = None;
    }

    /// Drop a file-backed stash body so the next read uses the on-disk keep.
    pub(crate) fn drop_body_if_pathed(&mut self) {
        if self.path.is_some() {
            self.body = None;
        }
    }

    /// Prefer the on-disk keep (YAML frontmatter) when a path exists.
    pub(crate) fn review_content(
        &self,
        read_file: impl FnOnce(&Path) -> Option<String>,
    ) -> Option<String> {
        if !self.is_kept() {
            return None;
        }
        if let Some(path) = self.path.as_deref()
            && let Some(content) = read_file(path)
        {
            return Some(content);
        }
        self.body
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
    }
}

fn usable(body: &Option<String>, path: &Option<PathBuf>) -> bool {
    body.as_deref().is_some_and(|text| !text.trim().is_empty()) || path.is_some()
}
