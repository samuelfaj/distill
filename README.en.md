<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Distill

**Distill**, by **Samuel Fajreldines**, is an independent terminal coding harness.
It can open without a Grok, ChatGPT, or OpenRouter account. Connect a local
OpenAI-compatible model server, configure your own endpoint, or optionally sign
in to a supported provider. Generating answers requires a reachable model;
starting the harness does not require a provider account.

## Build and run

From this repository, with Rust and the native build dependencies installed:

```sh
cargo build --release -p distill-pager-bin --bin distill
./target/release/distill
```

The source installer is `crates/codegen/distill-pager/scripts/install.sh` on
Unix, or `install.ps1` in that directory on Windows. It builds locally and
installs `distill` in `~/.local/bin`. No binary update channel is configured;
rebuild from source when updating.

## Accounts are optional

The home menu offers separate login/logout actions for Grok, ChatGPT, and
OpenRouter. Signing out of one provider does not sign out of another.
Grok login and inference remain supported. ChatGPT uses its own OAuth session
and account model catalog; select a model and its supported reasoning effort
with `/model`. OpenRouter uses its own credentials.

## Local model without provider login

Start an OpenAI-compatible local server and put this in your profile's
`config.toml`, replacing the model ID and port with those served locally:

```toml
[models]
default = "local"

[model.local]
name = "Local model"
model = "your-local-model-id"
base_url = "http://127.0.0.1:8000/v1"
api_backend = "chat_completions"
```

Distill uses `~/.distill` for new profiles. `DISTILL_HOME` explicitly selects a
profile. Existing `~/.grok` profiles and the legacy `GROK_HOME` override remain
supported to preserve credentials, settings, and sessions. An explicit
`DISTILL_HOME` takes priority; an existing `~/.distill` takes priority over the
legacy default. Project configuration compatibility is preserved.

See the [Portuguese guide](README.md) for Jev model routing and configuration.

## License and attribution

Distill is distributed under [Apache License 2.0](LICENSE). Its new branding and
modifications are by Samuel Fajreldines. Original copyrights and third-party
attributions remain in [LICENSE](LICENSE), [NOTICE](NOTICE), and
[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES). Provider protocol identifiers are
retained where required for interoperability. Distill is an independent project.
