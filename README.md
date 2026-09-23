# hey

Ask an LLM about your terminal. `hey` sends piped output, the previous command,
its exit status and light system context to a model and prints a short answer.
One static binary, one HTTPS POST per call, no runtime. It never executes anything
it suggests.

```sh
export HEY_ANTHROPIC_KEY=sk-ant-...        # that is all the setup there is
make 2>&1 | hey why did this fail
hey how do I undo the last commit
```

## Install

Download the archive for your platform from the releases page and put `hey` on
your `PATH`, or build it: `cargo build --release` (binary in `target/release/hey`).

`hey config init` walks through provider, key (hidden), model, an optional
escalate model for `hey -x`, and shell integration.

## Shell integration

Without it, `hey` sees stdin and arguments. With it, `hey fix` also knows the
previous command and its exit status.

```sh
hey shell install        # bash, zsh or pwsh; --shell <name> to override $SHELL
hey shell status
hey shell uninstall
```

`hey shell init <shell>` prints the snippet for `eval "$(hey shell init bash)"`.

## Configuration

Git-config style INI in `~/.hey` (global) and `./.hey` (local, per project).
Environment beats local beats global beats built-in defaults. Manage it like
`git config`:

```sh
hey config set core.provider openai
hey config set provider.openai.key          # prompts, hidden
hey config set alias.fix.prompt "The previous command failed. Give the corrected command only."
hey config list --show-origin
hey doctor            # checks files, permissions, key, shell integration; --fix, --ping
```

Keys can come from `HEY_<PROVIDER>_KEY`, `key_cmd` (any command, e.g.
`pass show api/openai`), `key`, or `key_from` another provider. Keep keys in the
global file; `hey` refuses to run when it is readable by others, and ignores keys
in a local `.hey` inside an unignored git work tree.

Providers use one of three wire types (`anthropic`, `openai`, `gemini`); anything
OpenAI-compatible (Ollama, LM Studio, Grok, vLLM) is `type = openai` with a `url`.

Options go before the prompt: `hey -x why`, `hey -v -p openai ...`. Use `--` to
start a prompt with a dash. After the first word everything is prompt text, so
`hey how to tar -xzf` works as typed.

## Development

```sh
cargo test                     # unit tests plus end-to-end tests against a mock server
scripts/live-test.sh           # real APIs; reads .api_keys_for_testing (git-ignored)
```

`.api_keys_for_testing` holds `NAME=value` lines (`OPENAI_API_KEY`, `CLAUDE_API_KEY`,
`GEMINI_API_KEY`, `XAI_API_KEY`). The script maps them onto `HEY_<PROVIDER>_KEY`
and uses an empty throwaway config, so nothing is written to your `~/.hey`.
