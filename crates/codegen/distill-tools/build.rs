// Modified for Distill by Samuel Fajreldines, 2026.
//! Build script for bundling ripgrep for the distill-tools crate.
//!
//! - If `GROK_TOOLS_BUNDLE_RG_PATH` is set, always bundle it
//! - Otherwise, only bundle in release builds
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

const RG_VER: &str = "15.0.0";
const BFS_VER: &str = "4.1";
const UGREP_VER: &str = "7.7.0";
const FD_VER: &str = "10.4.2";
// fd stopped publishing x86_64-apple-darwin assets after 10.3.0.
const FD_VER_MACOS_X64: &str = "10.3.0";

/// Pinned SHA-256 of each `(version, triple)` fd release tarball we embed.
const FD_TARBALL_SHA256: &[(&str, &str, &str)] = &[
    (
        "10.4.2",
        "x86_64-unknown-linux-musl",
        "e3257d48e29a6be965187dbd24ce9af564e0fe67b3e73c9bdcd180f4ec11bdde",
    ),
    (
        "10.4.2",
        "aarch64-unknown-linux-musl",
        "f32d3657473fba74e2600babc8db0b93420d51169223b7e8143b2ed55d8fd9e8",
    ),
    (
        "10.4.2",
        "aarch64-apple-darwin",
        "623dc0afc81b92e4d4606b380d7bc91916ba7b97814263e554d50923a39e480a",
    ),
    (
        "10.3.0",
        "x86_64-apple-darwin",
        "50d30f13fe3d5914b14c4fff5abcbd4d0cdab4b855970a6956f4f006c17117a3",
    ),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    bundle_rg()?;
    bundle_fd()?;
    bundle_search_tool("bfs", "BFS", BFS_VER)?;
    bundle_search_tool("ugrep", "UGREP", UGREP_VER)?;
    Ok(())
}

/// Download + embed fd as an optional vendored file-search binary, mirroring the ripgrep bundling
/// (release-only or `GROK_TOOLS_BUNDLE_FD_PATH` override), plus pinned per-asset SHA-256
/// verification of the downloaded tarball.
fn bundle_fd() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=GROK_TOOLS_BUNDLE_FD_PATH");
    println!("cargo:rustc-check-cfg=cfg(bundle_fd)");

    if env::var_os("CARGO_FEATURE_PI").is_none() {
        return Ok(());
    }

    let gen_dir = PathBuf::from(env::var("OUT_DIR")?).join("bundle-fd");
    fs::create_dir_all(&gen_dir)?;

    // The consuming vendor extraction is unix-only — never bundle on
    // Windows targets, mirroring the bfs/ugrep skip.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        return Ok(());
    }

    let path_override = env::var("GROK_TOOLS_BUNDLE_FD_PATH").ok();
    let is_release = env::var("PROFILE").as_deref() == Ok("release");
    if path_override.is_none() && !is_release {
        return Ok(());
    }

    // Per-target version: macOS x86_64 pins the last release with that asset.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let (ver, asset_triple) = match (target_os.as_str(), target_arch.as_str()) {
        ("macos", "aarch64") => (FD_VER, "aarch64-apple-darwin"),
        ("macos", "x86_64") => (FD_VER_MACOS_X64, "x86_64-apple-darwin"),
        ("linux", "x86_64") => (FD_VER, "x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => (FD_VER, "aarch64-unknown-linux-musl"),
        _ => {
            if path_override.is_none() {
                return Err(format!(
                    "Unsupported target for fd bundling: {target_os}-{target_arch}. Set GROK_TOOLS_BUNDLE_FD_PATH to a local fd binary for offline or unsupported builds.",
                )
                .into());
            }
            (FD_VER, "override")
        }
    };

    println!("cargo:rustc-cfg=bundle_fd");
    println!("cargo:rustc-env=GROK_TOOLS_FD_VER={ver}");

    if let Some(path) = path_override {
        let dest = gen_dir.join(format!("fd-{ver}-override.bin"));
        println!("cargo:rustc-env=GROK_TOOLS_FD_TARGET=override");
        let _ = fs::remove_file(&dest);
        fs::copy(PathBuf::from(path.clone()), &dest).map_err(|e| {
            format!(
                "Failed copying GROK_TOOLS_BUNDLE_FD_PATH: {e} from path {path} to dest {}",
                dest.display()
            )
        })?;
        compress_and_pin(&dest, "FD")?;
        return Ok(());
    }

    println!("cargo:rustc-env=GROK_TOOLS_FD_TARGET={asset_triple}");
    let dest = gen_dir.join(format!("fd-{ver}-{asset_triple}.bin"));
    let _ = fs::remove_file(&dest);

    let url = format!(
        "https://github.com/sharkdp/fd/releases/download/v{ver}/fd-v{ver}-{asset_triple}.tar.gz"
    );

    let bytes: Vec<u8> = {
        let resp = reqwest::blocking::get(&url).map_err(|e| {
            format!(
                "Failed to download fd: {e}\nSet GROK_TOOLS_BUNDLE_FD_PATH to a local fd for offline builds."
            )
        })?;
        if !resp.status().is_success() {
            return Err(format!(
                "HTTP {} downloading fd. Set GROK_TOOLS_BUNDLE_FD_PATH for offline builds.",
                resp.status()
            )
            .into());
        }
        resp.bytes()?.to_vec()
    };

    // Verify the tarball against the pinned per-asset hash before unpacking.
    let expected_sha = FD_TARBALL_SHA256
        .iter()
        .find(|(v, t, _)| *v == ver && *t == asset_triple)
        .map(|(_, _, sha)| *sha)
        .ok_or_else(|| format!("No pinned SHA-256 for fd {ver} {asset_triple}"))?;
    let actual_sha = {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        hex_encode(&hasher.finalize())
    };
    if actual_sha != expected_sha {
        return Err(format!(
            "SHA-256 mismatch for {url}:\n  expected {expected_sha}\n  actual   {actual_sha}"
        )
        .into());
    }

    let gz = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut ar = tar::Archive::new(gz);
    let mut found = false;
    for entry in ar.entries()? {
        let mut e = entry?;
        let p = e.path()?;
        if p.file_name().is_some_and(|n| n == "fd") {
            let data: Vec<u8> = {
                let mut v = Vec::new();
                io::copy(&mut e, &mut v)?;
                v
            };
            fs::write(&dest, &data)?;
            found = true;
            break;
        }
    }

    if !found {
        return Err(format!(
            "Could not find 'fd' in fd archive {url}. Set GROK_TOOLS_BUNDLE_FD_PATH for offline builds."
        )
        .into());
    }

    compress_and_pin(&dest, "FD")?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn compress_and_pin(
    dest: &std::path::Path,
    name_uc: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(dest)?;
    let sha = {
        use sha2::Digest as _;
        hex_encode(&sha2::Sha256::digest(&bytes))
    };

    let compressed = zstd::encode_all(bytes.as_slice(), 19)?;
    let mut zst = dest.to_path_buf().into_os_string();
    zst.push(".zst");
    fs::write(&zst, &compressed)?;

    println!("cargo:rustc-env=GROK_TOOLS_{name_uc}_SHA256={sha}");
    Ok(())
}

/// Bundle a prebuilt **static** search-tool binary (`bfs`/`ugrep`) when `GROK_TOOLS_BUNDLE_<NAME>_PATH` points at one (supplied by the release
/// pipeline). Emits `cfg(bundle_<name>)` so the crate's `include_bytes!` + self-extract engages. No auto-download (unlike ripgrep): bfs/ugrep
/// publish no prebuilt static release assets, so the release pipeline supplies the path.
fn bundle_search_tool(
    name: &str,
    name_uc: &str,
    ver: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let override_env = format!("GROK_TOOLS_BUNDLE_{name_uc}_PATH");
    println!("cargo:rerun-if-env-changed={override_env}");
    println!("cargo:rustc-check-cfg=cfg(bundle_{name})");

    // The consumer (`embedded_search_tools`) is `#[cfg(unix)]`, so embedding on a
    // Windows target is dead weight — skip (mirrors the ripgrep Windows skip).
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return Ok(());
    }

    let Some(src) = env::var(&override_env).ok().filter(|s| !s.is_empty()) else {
        return Ok(());
    };

    let gen_dir = PathBuf::from(env::var("OUT_DIR")?).join(format!("bundle-{name}"));
    fs::create_dir_all(&gen_dir)?;
    let dest = gen_dir.join(format!("{name}-{ver}-override.bin"));
    let _ = fs::remove_file(&dest);
    fs::copy(&src, &dest)
        .map_err(|e| format!("copy {override_env} from {src} to {}: {e}", dest.display()))?;

    println!("cargo:rustc-cfg=bundle_{name}");
    println!("cargo:rustc-env=GROK_TOOLS_{name_uc}_VER={ver}");
    println!("cargo:rustc-env=GROK_TOOLS_{name_uc}_TARGET=override");
    compress_and_pin(&dest, name_uc)?;
    Ok(())
}

/// Download + embed ripgrep. Unchanged behavior; split out of `main` so the new
/// search-tool bundling runs regardless of ripgrep's early returns.
fn bundle_rg() -> Result<(), Box<dyn std::error::Error>> {
    // Only bundle in release builds to avoid slowing down cargo check.
    println!("cargo:rerun-if-env-changed=GROK_TOOLS_BUNDLE_RG_PATH");
    println!("cargo:rustc-check-cfg=cfg(bundle_rg)");

    let gen_dir = PathBuf::from(env::var("OUT_DIR")?).join("bundle-rg");
    fs::create_dir_all(&gen_dir)?;

    // Decide whether to bundle: path override OR release build
    let path_override = env::var("GROK_TOOLS_BUNDLE_RG_PATH").ok();
    let is_release = env::var("PROFILE").as_deref() == Ok("release");
    if path_override.is_none() && !is_release {
        return Ok(());
    }

    // Skip auto-bundling on Windows: ripgrep ships .zip on Windows (not .tar.gz) and we have no zip-extraction path. Returning here BEFORE
    // emitting `cargo:rustc-cfg=bundle_rg` keeps include_bytes! macros gated on cfg(bundle_rg) compiled-out, so the runtime falls back to `rg` on
    // PATH. Users install ripgrep separately (winget / scoop). An explicit GROK_TOOLS_BUNDLE_RG_PATH still bundles regardless of target.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" && path_override.is_none() {
        return Ok(());
    }

    // Expose cfg so the crate can include the bundled bytes.
    println!("cargo:rustc-cfg=bundle_rg");
    println!("cargo:rustc-env=GROK_TOOLS_RG_VER={}", RG_VER);

    // If a local rg binary is provided, copy it directly (skips target check).
    if let Some(path) = path_override {
        let dest = gen_dir.join(format!("rg-{}-override.bin", RG_VER));
        println!("cargo:rustc-env=GROK_TOOLS_RG_TARGET=override");
        let _ = fs::remove_file(&dest);
        fs::copy(PathBuf::from(path.clone()), &dest).map_err(|e| {
            format!(
                "Failed copying GROK_TOOLS_BUNDLE_RG_PATH: {e} from path {path} to dest {}",
                dest.display()
            )
        })?;
        compress_and_pin(&dest, "RG")?;
        return Ok(());
    }

    // Determine supported ripgrep asset triple for auto-download.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let asset_triple = match (target_os.as_str(), target_arch.as_str()) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => {
            return Err(format!(
                "Unsupported target for ripgrep bundling: {os}-{arch}. Set GROK_TOOLS_BUNDLE_RG_PATH to a local rg binary for offline or unsupported builds.",
                os = target_os,
                arch = target_arch
            ).into());
        }
    };

    println!("cargo:rustc-env=GROK_TOOLS_RG_TARGET={}", asset_triple);
    let dest = gen_dir.join(format!("rg-{}-{}.bin", RG_VER, asset_triple));
    let _ = fs::remove_file(&dest);

    let url = format!(
        "https://github.com/BurntSushi/ripgrep/releases/download/{v}/ripgrep-{v}-{t}.tar.gz",
        v = RG_VER,
        t = asset_triple
    );

    let bytes: Vec<u8> = {
        let resp = reqwest::blocking::get(&url).map_err(|e| {
            format!(
                "Failed to download ripgrep: {}\nSet GROK_TOOLS_BUNDLE_RG_PATH to a local rg for offline builds.",
                e
            )
        })?;
        if !resp.status().is_success() {
            return Err(format!(
                "HTTP {} downloading ripgrep. Set GROK_TOOLS_BUNDLE_RG_PATH for offline builds.",
                resp.status()
            )
            .into());
        }
        resp.bytes()?.to_vec()
    };

    let gz = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut ar = tar::Archive::new(gz);
    let mut found = false;
    for entry in ar.entries()? {
        let mut e = entry?;
        let p = e.path()?;
        if p.file_name().is_some_and(|n| n == "rg") {
            let data: Vec<u8> = {
                let mut v = Vec::new();
                io::copy(&mut e, &mut v)?;
                v
            };
            fs::write(&dest, &data)?;
            found = true;
            break;
        }
    }

    if !found {
        return Err(format!(
            "Could not find 'rg' in ripgrep archive {}. Set GROK_TOOLS_BUNDLE_RG_PATH for offline builds.",
            url
        )
        .into());
    }

    compress_and_pin(&dest, "RG")?;
    Ok(())
}
