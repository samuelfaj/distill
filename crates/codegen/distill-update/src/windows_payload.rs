// Modified for Distill by Samuel Fajreldines, 2026.
//! The Windows-only payload a grok release ships beside `grok.exe`: the grove
//! hook exes (`grove.exe`, `grove-fsmonitor.exe`, `grove-credential.exe`,
//! assumes) and the bundled MinGit under `%LOCALAPPDATA%\grok\git\<mingit-version>\`
//! that `distill_tty_utils::bundled_git()` locates.
//!
//! Fetched after the grok binary is smoke-tested, activated after the managed
//! bin swap, always best-effort: a release without the payload (anything
//! before it shipped), a signer that has not published the hook exes yet, or a
//! failed fetch leaves grok updated and the payload as it was. The pure parts
//! (names, sidecar parsing, the prune rule, archive extraction) build and test
//! everywhere; the network and install steps are Windows-only.

// Off Windows the pure parts have only test callers.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

/// Hook exes in install order; all three are installed or none is.
pub(super) const GROVE_EXES: [&str; 3] = ["grove", "grove-fsmonitor", "grove-credential"];

/// `git\<version>` payloads that survive a prune: the one just installed and
/// the previous one, which running processes may still have on their `PATH`.
pub(super) const KEEP_MINGIT_VERSIONS: usize = 2;

/// Prefix of the directory an archive is extracted into before the rename
/// that publishes it as `git\<mingit-version>`.
const STAGING_PREFIX: &str = ".staging-";

/// Cap on the extracted archive (MinGit is about 95 MiB) so a corrupt or
/// crafted zip cannot fill the disk.
const MAX_MINGIT_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;

/// Cap on a `.version` / `.sha256` sidecar body; each is one short line.
const MAX_SIDECAR_BYTES: usize = 4096;

/// What the download phase fetched for this release. Empty when the release
/// carries no payload or a fetch failed.
#[derive(Debug, Default)]
pub(super) struct Payload {
    /// Downloaded hook exes in [`GROVE_EXES`] order; `None` unless all three came down.
    pub(super) grove: Option<[PathBuf; 3]>,
    /// A sha256-verified MinGit archive not yet installed, if this release
    /// ships one that is not already on disk.
    pub(super) mingit: Option<MinGitDownload>,
}

#[derive(Debug)]
pub(super) struct MinGitDownload {
    pub(super) zip: PathBuf,
    /// The `.version` sidecar: the directory name under `git\`.
    pub(super) version: String,
}

/// `grove-<ver>-<platform>`; the CLI fetch helper appends `.exe` on Windows.
pub(super) fn grove_object_name(exe: &str, version: &str, platform: &str) -> String {
    format!("{exe}-{version}-{platform}")
}

/// `grok-<ver>-<platform>-mingit`; `.zip`, `.zip.sha256` and `.version` hang off it.
pub(super) fn mingit_object_base(version: &str, platform: &str) -> String {
    format!("grok-{version}-{platform}-mingit")
}

/// The digest from a `sha256sum` sidecar (`<hex>  <name>`), lowercased.
pub(super) fn parse_sha256_sidecar(text: &str) -> Option<String> {
    let digest = text.split_whitespace().next()?;
    (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
}

/// A MinGit version string names a directory verbatim, so only a plain
/// dotted name (`2.55.0.windows.5`) is accepted.
pub(super) fn valid_mingit_version(version: &str) -> bool {
    let mut chars = version.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// `.staging-<grok-version>`: per grok release, so a crashed extraction of an
/// older release never collides with this one and is swept by the prune.
pub(super) fn staging_dir_name(grok_version: &str) -> String {
    format!("{STAGING_PREFIX}{grok_version}")
}

/// The tree under `version_dir` is one the locator will pick as usable
/// (`cmd\git.exe` plus a platform tree with the helpers and their DLL `bin`):
/// the test for skipping a version already installed and for publishing an
/// extracted one, so a launcher-only leftover is replaced rather than kept.
pub(super) fn payload_usable(version_dir: &Path) -> bool {
    distill_tty_utils::bundled_git_at(version_dir).is_some_and(|git| git.is_usable())
}

/// Entries of the `git` root to delete: every stale staging directory (not
/// `current_staging`) and every version past the newest `keep` (in the
/// locator's `version_key` order), except the one `in_use` (what this
/// process's locator resolved to, which other processes may share). Hidden
/// entries other than staging are left alone.
pub(super) fn prune_plan(
    entries: &[String],
    keep: usize,
    in_use: Option<&str>,
    current_staging: &str,
) -> Vec<String> {
    use distill_tty_utils::version_key;

    let mut doomed: Vec<String> = entries
        .iter()
        .filter(|name| name.starts_with(STAGING_PREFIX) && name.as_str() != current_staging)
        .cloned()
        .collect();
    let mut versions: Vec<&String> = entries
        .iter()
        .filter(|name| !name.starts_with('.'))
        .collect();
    versions.sort_by(|a, b| version_key(b).cmp(&version_key(a)).then_with(|| b.cmp(a)));
    doomed.extend(
        versions
            .into_iter()
            .skip(keep)
            .filter(|name| Some(name.as_str()) != in_use)
            .cloned(),
    );
    doomed
}

/// Lowercase hex SHA-256 of a file. Blocking.
pub(super) fn sha256_hex_of_file(path: &Path) -> std::io::Result<String> {
    use sha2::Digest as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Extract a zip into `dest`, refusing entries that escape it (any path
/// component other than a plain name, or a symlink), and archives that
/// expand past [`MAX_MINGIT_EXTRACTED_BYTES`]. Blocking.
pub(super) fn extract_zip(zip_path: &Path, dest: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Read as _;
    use std::path::Component;

    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("open archive {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("read zip directory")?;
    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).context("read zip entry")?;
        let Some(relative) = entry.enclosed_name() else {
            anyhow::bail!("archive entry escapes the destination: {}", entry.name());
        };
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            anyhow::bail!("archive entry escapes the destination: {}", entry.name());
        }
        if entry.is_symlink() {
            anyhow::bail!("archive entry is a symlink: {}", entry.name());
        }
        let out = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out_file =
            std::fs::File::create(&out).with_context(|| format!("create {}", out.display()))?;
        let budget = MAX_MINGIT_EXTRACTED_BYTES - total + 1;
        let written = std::io::copy(&mut entry.by_ref().take(budget), &mut out_file)
            .with_context(|| format!("extract {}", entry.name()))?;
        total += written;
        if total > MAX_MINGIT_EXTRACTED_BYTES {
            anyhow::bail!("archive expands past the {MAX_MINGIT_EXTRACTED_BYTES}-byte cap");
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) use windows::{activate, download};

#[cfg(windows)]
mod windows {
    use super::{
        GROVE_EXES, KEEP_MINGIT_VERSIONS, MAX_SIDECAR_BYTES, MinGitDownload, Payload, extract_zip,
        grove_object_name, mingit_object_base, parse_sha256_sidecar, payload_usable, prune_plan,
        sha256_hex_of_file, staging_dir_name, valid_mingit_version,
    };
    use crate::auto_update::{
        download_cli_artifact_from_gcs, download_client, download_silent, replace_managed_bins,
    };
    use anyhow::{Context, Result};
    use futures::future::join_all;
    use std::path::{Path, PathBuf};

    /// `%LOCALAPPDATA%\grok\git`, the root `distill_tty_utils::bundled_git` scans.
    fn mingit_root() -> Option<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(local).join("grok").join("git"))
    }

    /// `Ok(None)` on 404 (the release predates the object), the body
    /// otherwise; a body past [`MAX_SIDECAR_BYTES`] is an error.
    async fn fetch_text(url: &str) -> Result<Option<String>> {
        let mut resp = download_client()?.get(url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            anyhow::bail!("HTTP {} for {url}", resp.status());
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if body.len() + chunk.len() > MAX_SIDECAR_BYTES {
                anyhow::bail!("body over {MAX_SIDECAR_BYTES} bytes for {url}");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(
            String::from_utf8(body).with_context(|| format!("body of {url} is not UTF-8"))?,
        ))
    }

    /// Whether `url` is published: `Ok(false)` on 404. HEAD first; anything
    /// but 2xx/404 from it (a CDN that serves GET only) is re-probed with a
    /// one-byte GET, so only the GET verdict can fail the probe.
    async fn object_published(url: &str) -> Result<bool> {
        let client = download_client()?;
        match client.head(url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(true),
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => return Ok(false),
            Ok(resp) => tracing::debug!("HEAD {url}: HTTP {}; probing with GET", resp.status()),
            Err(e) => tracing::debug!("HEAD {url} failed: {e}; probing with GET"),
        }
        let resp = client
            .get(url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            anyhow::bail!("HTTP {} for {url}", resp.status());
        }
        Ok(true)
    }

    async fn discard(files: &[PathBuf]) {
        for file in files {
            let _ = tokio::fs::remove_file(file).await;
        }
    }

    /// Fetch what this release publishes for `platform` into `download_dir`.
    /// Never fails: anything missing or broken is logged and left out.
    pub(crate) async fn download(
        base: &str,
        grok_version: &str,
        platform: &str,
        download_dir: &Path,
    ) -> Payload {
        let base = base.trim_end_matches('/');
        let (grove, mingit) = tokio::join!(
            download_grove_exes(base, grok_version, platform, download_dir),
            download_mingit(base, grok_version, platform, download_dir),
        );
        Payload { grove, mingit }
    }

    /// All three hook exes or none: a partial set would leave git hooks that
    /// point at nothing. The signer publishes them one by one, so a
    /// not-yet-signed exe is an ordinary "not published" here. The three
    /// probes run together, then (all published) the three downloads.
    async fn download_grove_exes(
        base: &str,
        grok_version: &str,
        platform: &str,
        download_dir: &Path,
    ) -> Option<[PathBuf; 3]> {
        let objects = GROVE_EXES.map(|exe| grove_object_name(exe, grok_version, platform));
        let urls = objects
            .each_ref()
            .map(|object| format!("{base}/{object}.exe"));
        let probes = join_all(urls.iter().map(|url| object_published(url))).await;
        for (object, probe) in objects.iter().zip(probes) {
            match probe {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(
                        "{object}.exe is not published; grove hook exes left as they are"
                    );
                    return None;
                }
                Err(e) => {
                    tracing::warn!("grove payload: probing {object}.exe failed: {e:#}");
                    return None;
                }
            }
        }
        let dests = objects
            .each_ref()
            .map(|object| download_dir.join(format!("{object}.exe")));
        let downloads = join_all(
            objects
                .iter()
                .zip(&dests)
                .map(|(object, dest)| download_cli_artifact_from_gcs(base, object, dest, false)),
        )
        .await;
        for (object, download) in objects.iter().zip(downloads) {
            if let Err(e) = download {
                tracing::warn!("grove payload: downloading {object}.exe failed: {e:#}");
                discard(&dests).await;
                return None;
            }
        }
        Some(dests)
    }

    /// The MinGit archive, sha256-verified against its sidecar, unless the
    /// version it names is already installed.
    async fn download_mingit(
        base: &str,
        grok_version: &str,
        platform: &str,
        download_dir: &Path,
    ) -> Option<MinGitDownload> {
        let object = mingit_object_base(grok_version, platform);
        let version = match fetch_text(&format!("{base}/{object}.version")).await {
            Ok(Some(text)) => text.trim().to_owned(),
            Ok(None) => {
                tracing::info!("{object}.version is not published; bundled git left as it is");
                return None;
            }
            Err(e) => {
                tracing::warn!("MinGit payload: fetching {object}.version failed: {e:#}");
                return None;
            }
        };
        if !valid_mingit_version(&version) {
            tracing::warn!("MinGit payload: refusing version string {version:?}");
            return None;
        }
        let root = mingit_root()?;
        if payload_usable(&root.join(&version)) {
            tracing::debug!("bundled git {version} already installed");
            return None;
        }
        let expected = match fetch_text(&format!("{base}/{object}.zip.sha256")).await {
            Ok(Some(text)) => match parse_sha256_sidecar(&text) {
                Some(digest) => digest,
                None => {
                    tracing::warn!("MinGit payload: unparsable sha256 sidecar for {object}.zip");
                    return None;
                }
            },
            Ok(None) => {
                tracing::warn!("MinGit payload: {object}.zip.sha256 is not published");
                return None;
            }
            Err(e) => {
                tracing::warn!("MinGit payload: fetching {object}.zip.sha256 failed: {e:#}");
                return None;
            }
        };
        let zip = download_dir.join(format!("{object}.zip"));
        eprintln!("  Downloading bundled git {version}...");
        if let Err(e) = download_silent(&format!("{base}/{object}.zip"), &zip).await {
            tracing::warn!("MinGit payload: downloading {object}.zip failed: {e:#}");
            let _ = tokio::fs::remove_file(&zip).await;
            return None;
        }
        let hash_of = zip.clone();
        let actual = tokio::task::spawn_blocking(move || sha256_hex_of_file(&hash_of)).await;
        match actual {
            Ok(Ok(actual)) if actual == expected => Some(MinGitDownload { zip, version }),
            Ok(Ok(actual)) => {
                tracing::warn!(
                    "MinGit payload: sha256 mismatch for {object}.zip (expected {expected}, got {actual})"
                );
                let _ = tokio::fs::remove_file(&zip).await;
                None
            }
            Ok(Err(e)) => {
                tracing::warn!("MinGit payload: hashing {object}.zip failed: {e}");
                let _ = tokio::fs::remove_file(&zip).await;
                None
            }
            Err(e) => {
                tracing::warn!("MinGit payload: hash task panicked: {e}");
                let _ = tokio::fs::remove_file(&zip).await;
                None
            }
        }
    }

    /// Install what [`download`] fetched: the hook exes beside `grok.exe` in
    /// `bin_dir` and MinGit under `git\<version>` (independent, so together),
    /// then prune old MinGit versions. Best-effort; the downloads are removed
    /// afterwards either way.
    pub(crate) async fn activate(payload: &Payload, bin_dir: &Path, grok_version: &str) {
        let root = mingit_root();
        tokio::join!(
            activate_grove(payload.grove.as_ref(), bin_dir),
            activate_mingit(payload.mingit.as_ref(), root.as_deref(), grok_version),
        );
        if let Some(root) = root {
            prune_mingit(&root, grok_version).await;
        }
    }

    async fn activate_grove(files: Option<&[PathBuf; 3]>, bin_dir: &Path) {
        let Some(files) = files else {
            return;
        };
        match install_grove_exes(files, bin_dir).await {
            Ok(()) => tracing::info!("installed grove hook exes to {}", bin_dir.display()),
            Err(e) => tracing::warn!("grove hook exes not updated: {e:#}"),
        }
        discard(files).await;
    }

    async fn activate_mingit(
        mingit: Option<&MinGitDownload>,
        root: Option<&Path>,
        grok_version: &str,
    ) {
        let (Some(mingit), Some(root)) = (mingit, root) else {
            return;
        };
        match install_mingit(mingit, root, grok_version).await {
            Ok(()) => tracing::info!("installed bundled git {}", mingit.version),
            Err(e) => tracing::warn!("bundled git {} not installed: {e:#}", mingit.version),
        }
        let _ = tokio::fs::remove_file(&mingit.zip).await;
    }

    /// The same all-or-nothing replace as the managed `grok.exe`/`agent.exe`
    /// swap, over the three hook exes.
    async fn install_grove_exes(files: &[PathBuf; 3], bin_dir: &Path) -> Result<()> {
        let pairs: Vec<(PathBuf, PathBuf)> = files
            .iter()
            .zip(GROVE_EXES)
            .map(|(src, exe)| (src.clone(), bin_dir.join(format!("{exe}.exe"))))
            .collect();
        replace_managed_bins(&pairs).await
    }

    /// Extract into `git\.staging-<grok-version>`, check the tree is usable,
    /// then rename onto `git\<version>` so the locator only ever sees a
    /// complete tree under a version name.
    async fn install_mingit(
        mingit: &MinGitDownload,
        root: &Path,
        grok_version: &str,
    ) -> Result<()> {
        let version_dir = root.join(&mingit.version);
        let staging = root.join(staging_dir_name(grok_version));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        tokio::fs::create_dir_all(&staging)
            .await
            .with_context(|| format!("create {}", staging.display()))?;
        let (zip, dest) = (mingit.zip.clone(), staging.clone());
        let extracted = tokio::task::spawn_blocking(move || extract_zip(&zip, &dest))
            .await
            .map_err(|e| anyhow::anyhow!("extract task panicked: {e}"))
            .and_then(|r| r);
        if let Err(e) = extracted {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(e);
        }
        if !payload_usable(&staging) {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            anyhow::bail!("archive is not a usable MinGit tree (cmd\\git.exe, helpers, bin)");
        }
        if payload_usable(&version_dir) {
            // A concurrent updater finished first; theirs is as good as ours.
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Ok(());
        }
        // Only an unusable leftover of this same version can be here.
        if tokio::fs::metadata(&version_dir).await.is_ok() {
            tokio::fs::remove_dir_all(&version_dir)
                .await
                .with_context(|| format!("remove unusable {}", version_dir.display()))?;
        }
        if let Err(e) = tokio::fs::rename(&staging, &version_dir).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(e).with_context(|| format!("publish {}", version_dir.display()));
        }
        Ok(())
    }

    /// Keep the newest [`KEEP_MINGIT_VERSIONS`] payloads; never remove the one
    /// this process resolved (`bundled_git` is memoized, other processes may
    /// hold it too). A locked tree fails to delete and is retried next time.
    async fn prune_mingit(root: &Path, grok_version: &str) {
        let in_use = distill_tty_utils::bundled_git().and_then(|git| {
            git.cmd_dir
                .parent()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        });
        let Ok(mut entries) = tokio::fs::read_dir(root).await else {
            return;
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.is_ok_and(|t| t.is_dir())
                && let Some(name) = entry.file_name().to_str()
            {
                names.push(name.to_owned());
            }
        }
        let staging = staging_dir_name(grok_version);
        for name in prune_plan(&names, KEEP_MINGIT_VERSIONS, in_use.as_deref(), &staging) {
            if let Err(e) = tokio::fs::remove_dir_all(root.join(&name)).await {
                tracing::debug!("could not remove old bundled git {name}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn object_names_follow_the_release_layout() {
        assert_eq!(
            grove_object_name("grove-fsmonitor", "0.2.10", "windows-x86_64"),
            "grove-fsmonitor-0.2.10-windows-x86_64"
        );
        assert_eq!(
            mingit_object_base("0.2.10", "windows-aarch64"),
            "grok-0.2.10-windows-aarch64-mingit"
        );
        assert_eq!(staging_dir_name("0.2.10"), ".staging-0.2.10");
    }

    #[test]
    fn sha256_sidecar_parses_sha256sum_output_only() {
        let digest = "56D7B226B7693196CFC71FEF26568F536C4A021AB6C37FF2DB4287BED908E96E";
        assert_eq!(
            parse_sha256_sidecar(&format!(
                "{digest}  grok-0.2.10-windows-x86_64-mingit.zip\n"
            )),
            Some(digest.to_ascii_lowercase())
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("{digest}\n")),
            Some(digest.to_ascii_lowercase())
        );
        assert_eq!(parse_sha256_sidecar(""), None);
        assert_eq!(parse_sha256_sidecar("<html>not found</html>"), None);
        assert_eq!(parse_sha256_sidecar(&digest[..40]), None);
    }

    #[test]
    fn mingit_version_must_be_a_plain_directory_name() {
        assert!(valid_mingit_version("2.55.0.windows.5"));
        assert!(valid_mingit_version("2.47.1-rc0"));
        assert!(!valid_mingit_version(""));
        assert!(!valid_mingit_version(".."));
        assert!(!valid_mingit_version(".staging-0.2.10"));
        assert!(!valid_mingit_version("2.55\\evil"));
        assert!(!valid_mingit_version("2.55/evil"));
        assert!(!valid_mingit_version("2.55 0"));
    }

    #[test]
    fn prune_keeps_the_newest_two_and_the_one_in_use() {
        let entries = names(&[
            "2.9.1.windows.1",
            "2.47.1.windows.2",
            "2.50.0.windows.1",
            "2.55.0.windows.5",
        ]);
        let mut plan = prune_plan(&entries, KEEP_MINGIT_VERSIONS, None, ".staging-0.2.10");
        plan.sort();
        assert_eq!(plan, names(&["2.47.1.windows.2", "2.9.1.windows.1"]));

        let plan = prune_plan(
            &entries,
            KEEP_MINGIT_VERSIONS,
            Some("2.9.1.windows.1"),
            ".staging-0.2.10",
        );
        assert_eq!(plan, names(&["2.47.1.windows.2"]));

        assert!(
            prune_plan(
                entries.get(2..).unwrap_or(&[]),
                KEEP_MINGIT_VERSIONS,
                None,
                ".staging-0.2.10"
            )
            .is_empty()
        );
    }

    #[test]
    fn prune_sweeps_stale_staging_but_not_the_current_one_or_other_hidden_entries() {
        let entries = names(&[
            ".staging-0.2.9",
            ".staging-0.2.10",
            ".keep",
            "2.55.0.windows.5",
        ]);
        assert_eq!(
            prune_plan(&entries, KEEP_MINGIT_VERSIONS, None, ".staging-0.2.10"),
            names(&[".staging-0.2.9"])
        );
    }

    #[test]
    fn payload_usable_is_the_locators_predicate_not_just_the_launcher() {
        let tmp = tempfile::tempdir().unwrap();
        let version_dir = tmp.path().join("2.55.0.windows.5");
        assert!(!payload_usable(&version_dir));
        std::fs::create_dir_all(version_dir.join("cmd")).unwrap();
        assert!(!payload_usable(&version_dir));
        std::fs::write(version_dir.join("cmd").join("git.exe"), b"MZ").unwrap();
        assert!(
            !payload_usable(&version_dir),
            "launcher-only tree is not usable"
        );
        let git_core = version_dir.join("mingw64").join("libexec").join("git-core");
        std::fs::create_dir_all(&git_core).unwrap();
        std::fs::write(git_core.join("git-upload-pack.exe"), b"MZ").unwrap();
        assert!(
            !payload_usable(&version_dir),
            "helpers without their DLL bin"
        );
        std::fs::create_dir_all(version_dir.join("mingw64").join("bin")).unwrap();
        assert!(payload_usable(&version_dir));
    }

    fn write_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
        use std::io::Write as _;
        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            match body {
                Some(bytes) => {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(bytes).unwrap();
                }
                None => writer.add_directory(*name, options).unwrap(),
            }
        }
        writer.finish().unwrap();
    }

    #[test]
    fn extract_zip_reproduces_the_mingit_layout_and_hash_matches_sidecar_format() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("mingit.zip");
        write_zip(
            &zip,
            &[
                ("cmd/git.exe", Some(b"MZ launcher".as_slice())),
                ("etc/", None),
                (
                    "mingw64/bin/git-upload-pack.exe",
                    Some(b"MZ helper".as_slice()),
                ),
                (
                    "mingw64/libexec/git-core/git-remote-https.exe",
                    Some(b"MZ".as_slice()),
                ),
            ],
        );
        let staging = tmp.path().join(".staging-0.2.10");
        extract_zip(&zip, &staging).unwrap();
        assert!(payload_usable(&staging));
        assert!(staging.join("etc").is_dir());
        assert_eq!(
            std::fs::read(staging.join("mingw64/bin/git-upload-pack.exe")).unwrap(),
            b"MZ helper"
        );
        let digest = sha256_hex_of_file(&zip).unwrap();
        assert_eq!(
            parse_sha256_sidecar(&format!("{digest}  mingit.zip")),
            Some(digest)
        );
    }

    #[test]
    fn extract_zip_refuses_entries_that_escape_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("evil.zip");
        write_zip(
            &zip,
            &[
                ("cmd/git.exe", Some(b"MZ".as_slice())),
                ("../escaped.txt", Some(b"outside".as_slice())),
            ],
        );
        let staging = tmp.path().join("root").join(".staging-0.2.10");
        std::fs::create_dir_all(&staging).unwrap();
        let err = extract_zip(&zip, &staging).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err:#}");
        assert!(!tmp.path().join("root").join("escaped.txt").exists());
    }

    #[test]
    fn extract_zip_refuses_symlink_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("link.zip");
        {
            use std::io::Write as _;
            let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("cmd/git.exe", options).unwrap();
            writer.write_all(b"MZ").unwrap();
            writer
                .add_symlink("cmd/link.exe", "../../outside.exe", options)
                .unwrap();
            writer.finish().unwrap();
        }
        let staging = tmp.path().join(".staging-0.2.10");
        let err = extract_zip(&zip, &staging).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err:#}");
        assert!(!staging.join("cmd").join("link.exe").exists());
    }

    #[test]
    fn extract_zip_refuses_non_plain_path_components() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("dot.zip");
        write_zip(&zip, &[("./cmd/git.exe", Some(b"MZ".as_slice()))]);
        let staging = tmp.path().join(".staging-0.2.10");
        let err = extract_zip(&zip, &staging).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err:#}");
        assert!(!staging.join("cmd").join("git.exe").exists());
    }
}
