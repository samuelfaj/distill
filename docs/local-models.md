# Accounts and local models

The home menu has separate login and logout actions for Grok, ChatGPT, and
OpenRouter. Signing out of one does not sign out of the others. ChatGPT uses
OAuth and the model catalog available to your account. OpenRouter supports
browser login or an `OPENROUTER_API_KEY` environment variable.

For a local model, start an OpenAI-compatible server and add an entry to your
profile's `config.toml`. Replace the model ID and URL with your server's values:

```toml
[models]
default = "local"

[model.local]
name = "Local model"
model = "your-local-model-id"
base_url = "http://127.0.0.1:8000/v1"
api_backend = "chat_completions"
```

New profiles use `~/.distill`. Set `DISTILL_HOME` to use another directory. An
existing `~/.grok` profile remains supported when no new profile exists, so
previous credentials and sessions remain accessible. An explicit
`DISTILL_HOME` takes priority.
