// Modified for Distill by Samuel Fajreldines, 2026.
use super::load::load_config_from_toml;
use super::mcp::{Config, user_config_path};
use anyhow::Result;
use distill_agent::prompt::skills::SkillsConfig;
use distill_config::fs_atomic::BoundDest;
use std::path::Path;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
/// Process-wide write lock for `~/.grok/config.toml`.
/// Serializes the read-modify-write in `save_config` so two rapid settings toggles can't interleave and clobber each other.
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Blank (first-run 0-byte file) is an empty table; other unparseable TOML is an error so a silent fallback cannot drop unmodeled sections.
pub(crate) fn parse_existing_config_toml(s: &str) -> Result<TomlValue, toml::de::Error> {
    if s.trim().is_empty() {
        return Ok(TomlValue::Table(TomlMap::new()));
    }
    toml::from_str(s)
}
/// Settings-save body. Caller must hold [`ConfigWriteGuard`].
/// Re-resolve `dest` before publish. A retarget must not merge A onto B.
async fn save_config_locked(
    guard: ConfigWriteGuard,
    slot: &Path,
    dest: BoundDest,
    config: &Config,
) -> Result<()> {
    let dest = require_same_user_config_dest(slot, &dest)?;
    let mut root: TomlValue = match tokio::fs::read_to_string(dest.as_path()).await {
        Ok(s) => match parse_existing_config_toml(&s) {
            Ok(v) => v,
            Err(parse_err) => {
                return Err(anyhow::anyhow!(
                    "refusing to overwrite unparseable {}: {}; save a backup \
                         and fix the syntax error before retrying",
                    slot.display(),
                    parse_err,
                ));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TomlValue::Table(TomlMap::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "refusing to overwrite unreadable {}: {e}",
                slot.display()
            ));
        }
    };
    if !matches!(root, TomlValue::Table(_)) {
        root = TomlValue::Table(TomlMap::new());
    }
    let table = root.as_table_mut().expect("root must be a table");
    merge_section(table, "cli", &config.cli);
    merge_section(table, "models", &config.models);
    merge_section(table, "ui", &config.ui);
    merge_section(table, "harness", &config.harness);
    merge_section(table, "session", &config.session);
    merge_ask_user_question_section(table, &config.ask_user_question);
    if config.privacy == super::mcp::PrivacyConfig::default() {
        table.remove("privacy");
    } else {
        merge_section(table, "privacy", &config.privacy);
    }
    if config.consent == super::consent::ConsentConfig::default() {
        table.remove("consent");
    } else {
        if let Some(TomlValue::Table(section)) = table.get_mut("consent") {
            section.remove("answers");
        }
        merge_section(table, "consent", &config.consent);
    }
    if config.skills == SkillsConfig::default() {
        table.remove("skills");
    } else {
        merge_section(table, "skills", &config.skills);
    }
    merge_section(table, "telemetry", &config.telemetry);
    merge_section(table, "features", &config.features);
    merge_section(table, "jev", &config.jev);
    let toml_str = toml::to_string_pretty(&root)?;
    let dest = require_same_user_config_dest(slot, &dest)?;
    guard
        .run_blocking(move || atomic_write_resolved_string(&dest, &toml_str))
        .await
        .map_err(|e| anyhow::anyhow!("config write task failed: {e}"))??;
    Ok(())
}
/// Guard for a user `config.toml` read-modify-write: [`SAVE_LOCK`] plus the config-init flock —
/// without the flock leg, a SAVE_LOCK writer and a flock writer silently drop each other's edits.
#[must_use]
pub(crate) struct ConfigWriteGuard {
    _save: tokio::sync::MutexGuard<'static, ()>,
    _flock: std::fs::File,
}
impl ConfigWriteGuard {
    /// Run `f` on the blocking pool with this guard living in that task.
    /// Cancelling the returned future must not drop the guard in the async frame.
    pub(crate) async fn run_blocking<T, F>(self, f: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let _guard = self;
            f()
        })
        .await
    }
}
/// Acquire the user `config.toml` write guard (SAVE_LOCK ⊃ init flock) on the blocking pool;
/// fails closed — callers must not fall back to an unguarded write.
/// `SAVE_LOCK` moves into that task so a cancelled await cannot release it while flock is still being acquired.
pub(crate) async fn lock_config_writes() -> std::io::Result<ConfigWriteGuard> {
    let save = SAVE_LOCK.lock().await;
    let distill_home = crate::util::distill_home::distill_home();
    tokio::task::spawn_blocking(move || {
        let flock = acquire_init_lock(&distill_home)?;
        Ok(ConfigWriteGuard {
            _save: save,
            _flock: flock,
        })
    })
    .await
    .map_err(|e| std::io::Error::other(format!("config lock task failed: {e}")))?
}
/// Exclusive advisory `flock` on `<distill_home>/.config-init.lock`, retried briefly, serializing
/// `config.toml` read-modify-writes; only `WouldBlock` retries, and the file is never removed.
pub fn acquire_init_lock(distill_home: &std::path::Path) -> std::io::Result<std::fs::File> {
    use fs2::FileExt;
    let _ = std::fs::create_dir_all(distill_home);
    let lock_path = distill_home.join(".config-init.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    for _ in 0..50 {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("timed out waiting for {} after 1s", lock_path.display()),
    ))
}
/// Read a file, treating only `NotFound` as empty.
/// Hard read errors (EACCES, EIO) propagate so callers don't clobber an unreadable file on the next write.
pub fn read_to_string_or_empty(path: &std::path::Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}
/// Bind the follow dest then read it. Pair with [`atomic_write_follow_bound`].
pub fn read_follow_bound(path: &std::path::Path) -> std::io::Result<(BoundDest, String)> {
    let dest = bind_user_config_dest(path)?;
    let content = read_to_string_or_empty(dest.as_path())?;
    Ok((dest, content))
}
fn bind_user_config_dest(path: &std::path::Path) -> std::io::Result<BoundDest> {
    bind_user_config_dest_with(path, true, distill_config::user_distill_home().is_some())
}
fn bind_user_config_dest_with(
    path: &std::path::Path,
    follow_leaf: bool,
    has_user_home: bool,
) -> std::io::Result<BoundDest> {
    if follow_leaf && has_user_home {
        distill_config::fs_atomic::bind_follow_destination(path)
    } else {
        distill_config::fs_atomic::bind_slot_destination(path)
    }
}
/// Re-resolve with the same follow/slot policy used to bind `dest`.
pub(crate) fn require_same_user_config_dest(
    slot: &std::path::Path,
    dest: &BoundDest,
) -> std::io::Result<BoundDest> {
    require_same_user_config_dest_with(slot, dest, distill_config::user_distill_home().is_some())
}
fn require_same_user_config_dest_with(
    slot: &std::path::Path,
    dest: &BoundDest,
    has_user_home: bool,
) -> std::io::Result<BoundDest> {
    let now = bind_user_config_dest_with(slot, true, has_user_home)?;
    if now != *dest {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "config destination for {} changed from {} to {}",
                slot.display(),
                dest.as_path().display(),
                now.as_path().display()
            ),
        ));
    }
    Ok(now)
}
/// Publish onto `dest` only if `slot` still resolves to it.
pub fn atomic_write_follow_bound(
    slot: &std::path::Path,
    dest: &BoundDest,
    content: &str,
) -> std::io::Result<()> {
    let dest = require_same_user_config_dest(slot, dest)?;
    atomic_write_resolved_string(&dest, content)
}
/// Atomic write via temp file then `rename`. Follows a leaf symlink.
/// Project `.grok/config.toml` must use [`atomic_replace_string`].
/// User-config RMW must bind dest before load ([`read_follow_bound`] + [`atomic_write_follow_bound`]).
pub fn atomic_write_string(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    atomic_write_string_inner(path, content, true)
}
/// Like [`atomic_write_string`], but `rename` replaces a leaf symlink inode.
pub fn atomic_replace_string(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    atomic_write_string_inner(path, content, false)
}
/// Follow-leaf bind + read. Caller already classified `path` as user `config.toml`.
pub fn read_follow_leaf(path: &std::path::Path) -> std::io::Result<(BoundDest, String)> {
    let dest = distill_config::fs_atomic::bind_follow_destination(path)?;
    let content = read_to_string_or_empty(dest.as_path())?;
    Ok((dest, content))
}
/// Follow-leaf publish if `slot` still resolves to `dest`.
pub fn atomic_write_follow_leaf(
    slot: &std::path::Path,
    dest: &BoundDest,
    content: &str,
) -> std::io::Result<()> {
    let dest = distill_config::fs_atomic::require_same_bound_destination(slot, dest)?;
    atomic_write_resolved_string(&dest, content)
}
fn atomic_write_string_inner(
    path: &std::path::Path,
    content: &str,
    follow_leaf: bool,
) -> std::io::Result<()> {
    let dest = if follow_leaf {
        let first = distill_config::fs_atomic::bind_follow_destination(path)?;
        distill_config::fs_atomic::require_same_bound_destination(path, &first)?
    } else {
        distill_config::fs_atomic::bind_slot_destination(path)?
    };
    atomic_write_resolved_string(&dest, content)
}
/// Publish onto an already-bound destination. Temp inherits the current dest mode.
pub(crate) fn atomic_write_resolved_string(dest: &BoundDest, content: &str) -> std::io::Result<()> {
    if let Some(parent) = dest.as_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    distill_config::fs_atomic::write_atomically_bound(dest, content, None)
}
/// Merge `[toolset.ask_user_question]` into the root table.
/// `[toolset]` is deliberately NOT merged wholesale, so only this settings-writable sub-table round-trips.
/// It carries runtime-only structs (`web_search` sampler etc.) whose serialized defaults must never land in the user file.
fn merge_ask_user_question_section(
    table: &mut TomlMap<String, TomlValue>,
    ask: &crate::tools::config::AskUserQuestionToolConfig,
) {
    if ask.timeout_enabled.is_none() && ask.timeout_secs.is_none() {
        return;
    }
    let toolset = table
        .entry("toolset".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    if !matches!(toolset, TomlValue::Table(_)) {
        *toolset = TomlValue::Table(TomlMap::new());
    }
    if let TomlValue::Table(toolset_table) = toolset {
        merge_section(toolset_table, "ask_user_question", ask);
    }
}
/// Merge serialized fields of `value` into `table[key]`, preserving any existing keys not present in the serialized output.
/// This prevents `save_config` round-trips from silently dropping unmodeled fields (e.g. pager-written `show_timestamps`, `auto_dark_theme`).
/// Deep-merge `incoming` into `existing`: nested tables recurse; scalars replace.
fn merge_toml_tables(
    existing: &mut TomlMap<String, TomlValue>,
    incoming: TomlMap<String, TomlValue>,
) {
    for (field_key, field_val) in incoming {
        match (existing.get_mut(&field_key), field_val) {
            (Some(TomlValue::Table(dst)), TomlValue::Table(src)) => {
                merge_toml_tables(dst, src);
            }
            (_, v) => {
                existing.insert(field_key, v);
            }
        }
    }
}
fn merge_section<T: serde::Serialize>(
    table: &mut TomlMap<String, TomlValue>,
    key: &str,
    value: &T,
) {
    match TomlValue::try_from(value) {
        Ok(TomlValue::Table(new_fields)) if !new_fields.is_empty() => {
            let section = table
                .entry(key.to_string())
                .or_insert_with(|| TomlValue::Table(TomlMap::new()));
            if let TomlValue::Table(existing) = section {
                merge_toml_tables(existing, new_fields);
            } else {
                *section = TomlValue::Table(new_fields);
            }
        }
        Ok(TomlValue::Table(_)) => {}
        Ok(_) | Err(_) => {
            table.remove(key);
        }
    }
}
/// Update settings with a read-modify-write, preserving unrelated fields.
pub async fn update_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    let guard = lock_config_writes().await?;
    let path = user_config_path();
    let dest = bind_user_config_dest(&path)?;
    let root: TomlValue = crate::config::load_config_file(dest.as_path())?;
    let mut cfg = load_config_from_toml(&root);
    f(&mut cfg);
    save_config_locked(guard, &path, dest, &cfg).await
}
#[cfg(test)]
#[path = "persist_tests.rs"]
mod tests;
