# Poe

A minimal AI coding agent. Named after Poe from Altered Carbon.

## Setup

Install the binary:

```sh
cargo install --git https://github.com/bluegamma88/poe
```

Then add your model and API key to `config.toml`:

```toml
# model = "openai/gpt-oss-120b"
model = "anthropic/claude-opus-4.8"

[openrouter]
api_key = "sk-..."
```

## Update

Update the installed binary:

```sh
cargo install --git https://github.com/bluegamma88/poe --force
```
