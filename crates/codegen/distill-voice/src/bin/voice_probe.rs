// Modified for Distill by Samuel Fajreldines, 2026.
//! Standalone voice debug harness: capture the mic, stream to STT, print the transcript.
//!
//! ```bash
//! export XAI_API_KEY=...
//! cargo run -p distill-voice --bin voice-probe -- --seconds 5
//! ```

#![deny(clippy::indexing_slicing)]

use std::path::PathBuf;

use distill_voice::{
    StaticVoiceAuth, VoiceConfig, VoiceProbeOptions, format_probe_report, run_streaming_probe,
};

fn main() -> anyhow::Result<()> {
    // Hidden mic-capture helper intercept (macOS): the capture backend re-execs the current binary, here voice-probe itself
    // It runs before any runtime/TLS init so the capture child stays minimal
    if let Some(code) = distill_voice::maybe_run_capture_subprocess() {
        std::process::exit(code);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    distill_extra_ca::ensure_default_crypto_provider();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,distill_voice=debug".into()),
        )
        .init();

    let args = parse_args(std::env::args().skip(1).collect());

    let auth = std::env::var("XAI_API_KEY")
        .ok()
        .and_then(StaticVoiceAuth::shared)
        .ok_or_else(|| {
            anyhow::anyhow!("set XAI_API_KEY (standalone probe has no login session)")
        })?;

    let config = load_config(args.config_path.as_deref());

    eprintln!(
        "Voice probe: listening {}s (sample_rate={})",
        args.seconds, config.sample_rate
    );
    eprintln!("Speak now...\n");

    if args.mic_only {
        #[cfg(feature = "audio")]
        {
            let (bytes, chunks) =
                distill_voice::run_mic_only_probe(config.sample_rate, args.seconds)?;
            println!("Mic-only OK: {bytes} bytes in {chunks} chunks");
            if bytes == 0 {
                println!("WARNING: no audio — grant mic access to the terminal");
            }
            return Ok(());
        }
        #[cfg(not(feature = "audio"))]
        anyhow::bail!("built without `audio` feature");
    }

    let report = run_streaming_probe(VoiceProbeOptions {
        config,
        auth,
        capture_secs: args.seconds,
    })
    .await?;

    print!("{}", format_probe_report(&report));
    if report
        .transcript
        .as_ref()
        .is_none_or(|t| t.trim().is_empty())
    {
        std::process::exit(1);
    }
    Ok(())
}

struct Args {
    seconds: u32,
    config_path: Option<PathBuf>,
    mic_only: bool,
}

fn parse_args(argv: Vec<String>) -> Args {
    let mut out = Args {
        seconds: 5,
        config_path: None,
        mic_only: false,
    };
    let mut i = 0;
    while i < argv.len() {
        let Some(arg) = argv.get(i) else {
            break;
        };
        match arg.as_str() {
            "--seconds" | "-s" => {
                i += 1;
                if let Some(v) = argv.get(i) {
                    out.seconds = v.parse().unwrap_or(5);
                }
            }
            "--config" => {
                i += 1;
                if let Some(v) = argv.get(i) {
                    out.config_path = Some(PathBuf::from(v));
                }
            }
            "--mic-only" => out.mic_only = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other if !other.starts_with('-') => {}
            _ => eprintln!("unknown arg: {arg}"),
        }
        i += 1;
    }
    out
}

fn load_config(path: Option<&std::path::Path>) -> VoiceConfig {
    // The probe has no shell config stack; env is the resolved fallback (config table still beats it, matching the pager's precedence)
    let env_base = std::env::var("GROK_XAI_API_BASE_URL").ok();
    if let Some(path) = path
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(table) = toml::from_str::<toml::Table>(&raw)
    {
        return VoiceConfig::from_config_table(&table, env_base.as_deref());
    }
    if let Some(home) = distill_dirs::resolve_distill_home()
        && let Ok(raw) = std::fs::read_to_string(home.join("config.toml"))
        && let Ok(table) = toml::from_str::<toml::Table>(&raw)
    {
        return VoiceConfig::from_config_table(&table, env_base.as_deref());
    }
    VoiceConfig::from_config_table(&toml::Table::new(), env_base.as_deref())
}

fn print_help() {
    eprintln!(
        r#"voice-probe — debug mic + STT outside the pager

Usage:
  voice-probe [--seconds 5] [--mic-only]

Environment:
  XAI_API_KEY     required
  RUST_LOG        optional (default info,distill_voice=debug)

Reads [voice] from ~/.grok/config.toml unless --config PATH is set.
"#
    );
}
