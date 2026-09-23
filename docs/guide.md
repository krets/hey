# hey guide

## Everyday use

Type a question, or pipe something in and ask about it:

```sh
hey how do I list files by size
make 2>&1 | hey why did this fail
cat notes.txt | hey summarize this
```

`hey` only prints text. It never runs anything it suggests.

Options go before the prompt, so words like `-xzf` in a question are left alone:

```sh
hey -x why is this failing        # use your bigger model
hey -p openai how do I ...        # use a different provider this once
hey -n ...                        # send only the prompt and stdin, no shell context
hey --dry-run ...                 # show what would be sent (keys redacted)
hey -v ...                        # timing and token usage on stderr
hey -- -x is a strange flag       # start a prompt with a dash
```

## Shell integration

Without it, `hey` sees what you pipe in and what you type. With it, `hey fix`
also knows the previous command and how it exited. `hey config init` offers to
install it; you can also manage it directly (bash, zsh and PowerShell):

```sh
hey shell install
hey shell status
hey shell uninstall
```

## Providers and models

```sh
hey config providers                        # what is configured; * marks the default
hey config set core.provider <name>         # change the default
hey config set provider.<name>.model <model>
hey config set provider.<name>.key          # prompts, hidden
```

`anthropic`, `gemini` and `openai` are built in. Anything OpenAI-compatible
(Grok, Ollama, LM Studio, vLLM) is a provider with `type = openai` and a `url`:

```sh
hey config set provider.ollama.type openai
hey config set provider.ollama.url http://localhost:11434/v1
hey config set provider.ollama.model llama3.1
```

`core.escalate` names the provider `hey -x` uses.

## Aliases

Named prompts, stored in your config:

```sh
hey config set alias.fix.prompt "The previous command failed. Give the corrected command only."
hey fix
hey fix and keep the --force flag
hey help aliases
```

## Configuration

Git-config style INI, managed like `git config`:

```sh
hey config list --show-origin
hey config get core.provider
hey config unset alias.fix.prompt
hey config edit
hey doctor            # checks config, permissions, key, shell integration; --fix, --ping
```

- Global settings live in `~/.hey`, project overrides in `./.hey`.
  Local beats global; both keep your comments and layout when edited.
- Keys come from `key_cmd` (any command, such as `pass show api/openai`), `key`,
  or `key_from` another provider.
- `hey` refuses to run if `~/.hey` is readable by others (`hey doctor --fix`).
  Keep keys in the global file; keys in a project `.hey` are ignored inside a git
  work tree unless the file is git-ignored.
- The full reference is [spec.md](../spec.md).

## Building from source

```sh
cargo build --release          # binary in target/release/hey
cargo test                     # unit tests plus end-to-end tests against a mock server
```

`scripts/live-test.sh` runs against the real APIs. It reads `.api_keys_for_testing`
(git-ignored) as `NAME=value` lines (`OPENAI_API_KEY`, `CLAUDE_API_KEY`,
`GEMINI_API_KEY`, `XAI_API_KEY`), passes them through `HEY_<PROVIDER>_KEY`, and uses
an empty throwaway config so your `~/.hey` is untouched.

## Releases

Every push to `main` builds binaries as downloadable CI artifacts. Pushing a tag
such as `v0.1.0` publishes them, with checksums, as a GitHub Release.
