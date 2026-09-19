// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::PathBuf;
use std::sync::OnceLock;
use distill_tool_runtime::{ToolError, ToolErrorKind};

#[cfg(bundle_rg)]
const RG_BYTES: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/bundle-rg/rg-",
    env!("GROK_TOOLS_RG_VER"),
    "-",
    env!("GROK_TOOLS_RG_TARGET"),
    ".bin.zst"
));

#[cfg(bundle_rg)]
fn resolve_bundled_rg() -> Result<Option<PathBuf>, crate::util::vendor::InstallError> {
    crate::util::vendor::resolve(
        concat!(
            "rg-",
            env!("GROK_TOOLS_RG_VER"),
            "-",
            env!("GROK_TOOLS_RG_TARGET")
        ),
        RG_BYTES,
        env!("GROK_TOOLS_RG_SHA256"),
    )
}

pub fn rg_path() -> Result<PathBuf, ToolError> {
    static RG_EXEC: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    RG_EXEC
        .get_or_init(|| {
            #[cfg(bundle_rg)]
            {
                resolve_bundled_rg()
                    .map(|found| found.unwrap_or_else(|| PathBuf::from("rg")))
                    .map_err(|e| e.to_string())
            }
            #[cfg(not(bundle_rg))]
            {
                Ok(rg_from_path_or_runfiles())
            }
        })
        .clone()
        .map_err(|msg| ToolError::new(ToolErrorKind::Execution, msg))
}

#[cfg(not(bundle_rg))]
fn rg_from_path_or_runfiles() -> PathBuf {
    if let Ok(p) = std::env::var("RG_BIN_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(rf) = std::env::var("RUNFILES_DIR")
        && let Ok(entries) = std::fs::read_dir(PathBuf::from(rf))
    {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .contains("ripgrep_hermetic")
            {
                for sub in ["amd64/rg", "arm64/rg", "rg"] {
                    let candidate = entry.path().join(sub);
                    if candidate.exists() {
                        return candidate;
                    }
                }
            }
        }
    }
    PathBuf::from("rg")
}
