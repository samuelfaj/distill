// Modified for Distill by Samuel Fajreldines, 2026.
//! In dev mode, watches ~/.grok/pager.toml for changes and hot-reloads.
//! In prod mode, returns static defaults (no file operations).
use super::config::AppearanceConfig;
use std::io;
use std::path::PathBuf;
use tokio::sync::watch;
/// In dev mode: reads from ~/.grok/pager.toml, watches for changes.
/// In prod mode: returns static defaults, `.changed()` never fires.
pub struct ConfigWatcher {
    rx: watch::Receiver<AppearanceConfig>,
    #[allow(dead_code)]
    state: WatcherState,
}
enum WatcherState {
    /// No background task (prod mode or dev without notify)
    Static {
        /// Keep sender alive so channel doesn't close
        _tx: watch::Sender<AppearanceConfig>,
    },
}
impl ConfigWatcher {
    /// - In dev mode: reads/creates ~/.grok/pager.toml, watches for changes
    /// - In prod mode: returns default config, no file operations
    pub async fn start() -> io::Result<Self> {
        Self::start_static()
    }
    pub fn current(&self) -> watch::Ref<'_, AppearanceConfig> {
        self.rx.borrow()
    }
    /// Never completes in prod mode.
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.rx.changed().await
    }
    /// Path to `$GROK_HOME/pager.toml`.
    fn pager_config_path() -> PathBuf {
        crate::util::pager_toml_path()
    }
    /// Start with config loaded from disk (prod mode, no hot-reload).
    fn start_static() -> io::Result<Self> {
        let config = distill_config::user_distill_home()
            .and_then(|_| std::fs::read_to_string(Self::pager_config_path()).ok())
            .and_then(|content| {
                toml::from_str::<super::config::RawAppearanceConfig>(&content)
                    .ok()
                    .map(AppearanceConfig::from)
            })
            .unwrap_or_default();
        let (tx, rx) = watch::channel(config);
        Ok(Self {
            rx,
            state: WatcherState::Static { _tx: tx },
        })
    }
}
/// True when a notify `Remove` fired but the pager.toml slot is still present.
#[cfg(test)]
fn pager_slot_survived_remove(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
/// Slot present *and* the follow dest still names a file. A dangling slot is
/// referent deletion (defaults), not a tmp+rename replacement.
#[cfg(test)]
fn pager_referent_still_present(path: &std::path::Path) -> bool {
    if !pager_slot_survived_remove(path) {
        return false;
    }
    match distill_config::fs_atomic::resolve_atomic_destination(path) {
        Ok(dest) => std::fs::metadata(&dest)
            .map(|m| m.is_file())
            .unwrap_or(false),
        Err(_) => false,
    }
}
/// Accept the slot name or the bound dest name. Dest basename need not be `pager.toml`.
#[cfg(test)]
fn pager_event_is_watched_config(
    event_paths: &[std::path::PathBuf],
    slot: &std::path::Path,
    dest: Option<&std::path::Path>,
) -> bool {
    event_paths.iter().any(|path| {
        let Some(name) = path.file_name() else {
            return false;
        };
        slot.file_name() == Some(name) || dest.and_then(|d| d.file_name()) == Some(name)
    })
}
#[cfg(test)]
mod tests {
    use super::{
        pager_event_is_watched_config, pager_referent_still_present, pager_slot_survived_remove,
    };
    #[test]
    fn pager_reload_matches_differently_named_dest() {
        let slot = std::path::Path::new("/home/u/.grok/pager.toml");
        let dest = std::path::Path::new("/dotfiles/appearance.toml");
        assert!(pager_event_is_watched_config(
            &[dest.to_path_buf()],
            slot,
            Some(dest),
        ));
        assert!(pager_event_is_watched_config(
            &[slot.to_path_buf()],
            slot,
            Some(dest),
        ));
        assert!(!pager_event_is_watched_config(
            &[std::path::PathBuf::from("/dotfiles/other.toml")],
            slot,
            Some(dest),
        ));
    }
    #[test]
    fn remove_reloads_when_symlink_slot_still_exists() {
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join("dotfiles");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("pager.toml");
        std::fs::write(&dest, "x = 1").unwrap();
        let slot = dir.path().join("pager.toml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dest, &slot).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&dest, &slot).unwrap();
        assert!(pager_slot_survived_remove(&slot));
        let tmp = dest_dir.join(".tmp");
        std::fs::write(&tmp, "x = 2").unwrap();
        std::fs::rename(&tmp, &dest).unwrap();
        assert!(
            pager_slot_survived_remove(&slot),
            "slot must survive referent replace"
        );
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&slot)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&slot).unwrap();
        assert!(
            !pager_slot_survived_remove(&slot),
            "gone slot is a real delete"
        );
    }
    #[test]
    fn dangling_slot_is_referent_deletion_not_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join("dotfiles");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("pager.toml");
        std::fs::write(&dest, "x = 1").unwrap();
        let slot = dir.path().join("pager.toml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dest, &slot).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&dest, &slot).unwrap();
        assert!(pager_referent_still_present(&slot));
        std::fs::remove_file(&dest).unwrap();
        #[cfg(unix)]
        {
            assert!(
                pager_slot_survived_remove(&slot),
                "dangling symlink slot still exists"
            );
            assert!(
                !pager_referent_still_present(&slot),
                "deleted referent must not look like a replacement"
            );
        }
        #[cfg(not(unix))]
        assert!(!pager_slot_survived_remove(&slot));
    }
}
