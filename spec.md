# hey: specification

A single-binary CLI that sends terminal context (piped output, the previous command, its exit status, light system state) plus an optional question to an LLM, and prints a concise, terminal-ready answer.

Design goals:

- Fast startup. Native binary, no runtime, no SDKs. One HTTPS POST per invocation.
- Works as a plain binary on `PATH` with zero setup beyond a key.
- Optional shell integration that adds context the binary cannot see on its own.
- Configuration managed like `git config`: editable by hand, fully manageable from the CLI.
- Never executes anything it suggests.

---

## 1. Invocation

```
hey [OPTIONS] [PROMPT...]
<cmd> | hey [OPTIONS] [PROMPT...]
hey <subcommand> [ARGS...]
```

Argument resolution, in order:

1. First positional matches a builtin subcommand (`config`, `shell`, `doctor`, `version`, `help`): run it.
2. First positional matches a configured alias (`alias.<name>`): expand it (section 6).
3. Otherwise all positionals are joined with spaces and treated as the prompt.
4. `--` forces everything after it to be prompt text, bypassing 1 and 2.

Stdin is read only when it is not a TTY. The user is responsible for merging streams (`cmd 2>&1 | hey`).

### Options

| Flag | Meaning |
|---|---|
| `-p, --provider <name>` | Override `core.provider` for this call |
| `-m, --model <name>` | Override the provider's model for this call |
| `-x, --escalate` | Use the `core.escalate` provider/model (the "big model" path) |
| `-n, --no-context` | Send only stdin and prompt; skip system and shell context |
| `--dry-run` | Print the assembled request payload (keys redacted) and exit |
| `--raw` | Print the raw response body |
| `-v, --verbose` | Log timing, provider, model, token usage to stderr |
| `--last-command <str>` | Set by shell integration |
| `--exit-code <int>` | Set by shell integration |
| `--shell <name>` | Set by shell integration |

The `--last-command`, `--exit-code`, `--shell` flags are the contract between the shell function and the binary. They are documented but not intended for manual use.

---

## 2. Configuration

### 2.1 Files and precedence

| Scope | Path | Flag |
|---|---|---|
| Local | `./.hey` (current directory only, no upward walk) | `--local` |
| Global | `~/.hey` (Windows: `%USERPROFILE%\.hey`) | `--global` |

Resolution: environment variables > local > global > built-in defaults. Values merge per key; a local file only needs the keys it overrides.

`HEY_CONFIG=<path>` replaces the global path (useful for testing).

### 2.2 Format

INI in git-config style. Section headers with an optional quoted subsection. Keys are flattened to dotted names for the CLI.

```ini
[core]
    provider = anthropic
    escalate = anthropic-big
    max_tokens = 800
    timeout = 30

[provider "anthropic"]
    type = anthropic
    model = claude-haiku-4-5
    key = sk-ant-...

[provider "anthropic-big"]
    type = anthropic
    model = claude-sonnet-5
    key_from = anthropic          # reuse another provider's credentials

[provider "openai"]
    type = openai
    model = gpt-4.1-nano
    key_cmd = pass show api/openai   # key fetched from a command

[provider "grok"]
    type = openai
    url = https://api.x.ai/v1
    model = <model>
    key = ...

[provider "gemini"]
    type = gemini
    model = <model>
    key = ...

[provider "ollama"]
    type = openai
    url = http://localhost:11434/v1
    model = llama3.1
    # no key required

[provider "lmstudio"]
    type = openai
    url = http://localhost:1234/v1
    model = <loaded model id>

[context]
    system = true
    git = false
    max_stdin_bytes = 32768

[prompt]
    system = You are a command line assistant. The user is in a terminal. Answer as concisely as possible. Output must be accurate, correct, and scoped to the question. Prefer a single corrected command when one exists. No markdown, no preamble.

[alias "explain"]
    prompt = Explain what this output means in two sentences.

[alias "fix"]
    prompt = The previous command failed. Give the corrected command only.
```

Dotted key mapping: `provider.anthropic.model`, `context.git`, `alias.fix.prompt`, `prompt.system`.

### 2.3 Provider wire types

Three request formats cover every target. Everything OpenAI-compatible (OpenAI, Grok, Ollama, LM Studio, vLLM, llama.cpp server, Gemini's OpenAI-compat endpoint) uses `type = openai` with a custom `url`.

| `type` | Endpoint | Auth header | Response text path |
|---|---|---|---|
| `anthropic` | `{url}/v1/messages` (default url `https://api.anthropic.com`) | `x-api-key`, `anthropic-version` | `content[*].text` where `type == "text"` |
| `openai` | `{url}/chat/completions` (default url `https://api.openai.com/v1`) | `Authorization: Bearer` | `choices[0].message.content` |
| `gemini` | `{url}/models/{model}:generateContent` | `x-goog-api-key` | `candidates[0].content.parts[*].text` |

`url` overrides the default for any type. A provider with no key sends no auth header.

### 2.4 Key sources

Resolved in this order, first hit wins:

1. `HEY_<PROVIDER>_KEY` environment variable (name uppercased, `-` to `_`)
2. `key_cmd`: run via the platform shell, trimmed stdout is the key
3. `key`
4. `key_from`: resolve another provider's key by the same rules

### 2.5 Environment overrides

| Variable | Overrides |
|---|---|
| `HEY_PROVIDER` | `core.provider` |
| `HEY_MODEL` | model of the active provider |
| `HEY_CONFIG` | global config path |
| `HEY_NO_CONTEXT=1` | same as `-n` |

---

## 3. `hey config`

Mirrors `git config` semantics.

```
hey config get <key>
hey config set <key> <value>        [--local | --global]
hey config unset <key>              [--local | --global]
hey config list                     [--show-origin] [--show-secrets]
hey config edit                     [--local | --global]
hey config init                     # interactive first-run setup
hey config providers                # list configured providers, marking the active one
```

- Default write scope is `--global`. Reads search all scopes unless one is given.
- `list` redacts any `key` value to the first 4 and last 4 characters unless `--show-secrets`.
- `list --show-origin` prefixes each line with its source file.
- `edit` opens `$VISUAL`, then `$EDITOR`, then `vi`.
- Writes preserve comments, ordering, and unrelated sections. Implement against a format-preserving INI representation, not a parse-then-serialize map.
- `set provider.<name>.key` with no value reads the key from a hidden TTY prompt so it never lands in shell history.
- `init` walks through: pick a provider, enter a key (hidden), pick a model, optionally set an escalate provider, optionally install shell integration. Writes the file with correct permissions.

---

## 4. Security

### 4.1 Permission check

On every run, before reading keys:

- Unix: if the config file mode has any group or other bits set (`mode & 0o077 != 0`), refuse to run and print:
  `hey: ~/.hey is readable by others (mode 0644). Run: chmod 600 ~/.hey  or  hey doctor --fix`
- All writes by `hey config` create or leave the file at `0600`.
- Windows: best-effort ACL check; warn (do not refuse) if the file grants read to principals other than the owner, SYSTEM, and Administrators.

### 4.2 Local config guard

- A local `./.hey` containing any `key` field triggers a warning. If the directory is inside a git work tree and `.hey` is not ignored, refuse to use the keys and say so.
- Recommended practice: keys only in global; local files for provider, model, and prompt overrides per project.

### 4.3 No execution

`hey` prints text. It never runs a suggested command, never offers a "run this?" prompt, never writes outside its own config and the shell profile block (section 5). This is a deliberate constraint.

### 4.4 Redaction

`--dry-run`, `--verbose`, and error messages never print keys. Errors from providers are shown with auth headers stripped.

---

## 5. Shell integration

The bare binary sees stdin, argv, and the environment. A shell function adds the previous command text and its exit status, which only the live shell has.

### 5.1 Commands

```
hey shell init <bash|zsh|pwsh>      # print the snippet to stdout
hey shell install [--shell <name>]  # append the snippet to the profile
hey shell uninstall [--shell <name>]
hey shell status                    # installed? which profile? snippet version current?
```

- Shell detection: `$SHELL` basename on Unix, `$PSVersionTable` presence / parent process on Windows. `--shell` always wins, since inherited `$SHELL` is often wrong.
- Profile targets: bash `~/.bashrc`, zsh `~/.zshrc`, PowerShell `$PROFILE` (CurrentUserCurrentHost).
- The snippet is wrapped in markers so it can be found, updated, and removed:

```
# >>> hey shell integration v1 >>>
...
# <<< hey shell integration <<<
```

- `install` is idempotent: an existing block is replaced in place, never duplicated. A backup of the profile is written as `<profile>.hey.bak` before the first modification.
- The binary embeds the snippet templates; the version in the marker lets `shell status` report staleness after upgrades.

### 5.2 Function contract

The function shares the name `hey` and calls the binary via `command hey` (bash/zsh) or the resolved application path (PowerShell), so no rename is needed and there is no recursion.

Requirements for every shell's snippet:

1. Capture the exit status on the first statement, before anything else runs.
2. Fetch the last two history entries (see 5.3).
3. Pass stdin through untouched.
4. Forward all user arguments unchanged.
5. Call the binary with `--shell`, `--last-command`, `--exit-code`.

### 5.3 History semantics and known limits

- bash and zsh add the current command line to in-memory history before executing it. Inside the function, `fc -ln -1` is usually the line that invoked `hey` itself, not the previous one. The function sends both the current line and the one before it; the binary decides:
  - Current line contains a pipe into `hey`: the producer is the text left of the `| hey`. That is the command of interest.
  - Current line is a bare `hey ...`: the previous entry is the command of interest.
- Exit status is only meaningful in the bare form (`failing-cmd` then `hey fix`). In the pipe form, the producer runs concurrently and its status is not observable from inside the pipeline. The binary omits exit status from the prompt in pipe mode rather than sending a misleading value.
- Settings like `HISTCONTROL=ignorespace`, `ignoredups`, or `HIST_IGNORE_SPACE` can drop entries. The binary tolerates empty or missing `--last-command`.
- bash runs the last pipeline element in a subshell; verify `fc` still returns entries there on the target bash versions (it reads the inherited in-memory list). Fall back to no last-command if empty.
- PowerShell: `Get-History -Count 1` returns the previous completed command (the current one is not yet recorded). Status from `$?` (bool) and `$LASTEXITCODE` (native exit code). Capture both before the first statement that could reset them.

### 5.4 Without integration

Everything works; the prompt just lacks the command text and status. `hey doctor` reports integration as absent, not as an error.

---

## 6. Aliases

Replacement for scattered shell aliases: named prompt templates stored in config.

```ini
[alias "fix"]
    prompt = The previous command failed. Give the corrected command only.
    provider = anthropic          # optional per-alias override
    model = claude-haiku-4-5      # optional
    no_context = false            # optional
```

- `hey fix` expands to the alias prompt.
- `hey fix also keep the --force flag` appends the extra words to the alias prompt.
- `hey config set alias.<name>.prompt "<text>"` creates one.
- Builtin subcommand names cannot be used as alias names.
- `hey config list` shows aliases; `hey help aliases` lists them with their prompts.

---

## 7. Context collection

Each block is optional and cheap. Anything that would cost more than a few milliseconds is off by default.

| Block | Key | Default | Contents |
|---|---|---|---|
| System | `context.system` | on | OS, kernel/version, distro (from `/etc/os-release`), arch, shell, cwd, username, WSL detection |
| Shell | (flags) | when present | last command, exit status, shell name |
| Git | `context.git` | off | branch, short status summary (counts only), remote name. Only if cwd is in a work tree |
| Stdin | `context.max_stdin_bytes` | 32768 | piped content |

### 7.1 Stdin truncation

If stdin exceeds the cap, keep the first 40% and last 60% and insert a marker line: `[... N bytes truncated ...]`. Errors tend to be at the end; the command echo and setup at the start.

Strip ANSI escape sequences before sending.

### 7.2 Git collection

Run `git` as a subprocess with a short timeout (200ms). On timeout or failure, omit silently.

---

## 8. Prompt assembly

System message: `prompt.system` (configurable, default shown in 2.2).

User message, sections included only when non-empty, fixed order:

```
<environment>
os: Ubuntu 24.04 (WSL2)
shell: bash
cwd: /home/jesse/src/hey
</environment>

<command exit_code="128">
git push origin main
</command>

<output>
fatal: ...
</output>

<question>
why
</question>
```

- With no prompt text and no stdin: print usage and exit 2. Do not call the API.
- With stdin but no prompt text: question defaults to `prompt.default_question` (default: "Explain this output and, if it indicates an error, give the fix.").

---

## 9. Output

- Response text to stdout, nothing else. Diagnostics to stderr.
- Colour only when stdout is a TTY and `NO_COLOR` is unset. Default is no colour.
- If the model returns a fenced code block anyway, strip the fences when stdout is not a TTY so the output is pipeable.
- Streaming (`core.stream = true`) prints tokens as they arrive; off by default to keep the fence-stripping logic simple.

---

## 10. `hey doctor`

Checks and reports, one line each:

- Config files found, their scopes, parse errors
- Permissions on each file; `--fix` sets `0600`
- Active provider and model; key resolvable (without printing it)
- Local config containing keys inside an unignored git tree
- Shell integration installed, snippet version current
- Optional `--ping`: send a minimal request to the active provider and report latency

---

## 11. Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Provider/network error |
| 2 | Usage error |
| 3 | Config error (parse, missing key, missing provider) |
| 4 | Refused: insecure config permissions |

---

## 12. Implementation notes

- Rust. HTTP via a blocking client with rustls (e.g. `ureq`) to avoid pulling in an async runtime. JSON via `serde_json`. No provider SDKs.
- Format-preserving INI editing: hand-rolled line model (section, key, comment, blank) is simpler and more predictable than most crates for git-config style files.
- Target cold-start overhead under 10ms before the network call.
- Distribution: static-ish release binaries per target (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`) built in CI, attached to tagged releases. Single file, drop on `PATH`.
- `hey version` prints binary version and embedded shell snippet version.

---

## 13. Out of scope for v1

- Multi-turn conversation / session memory
- Tool use or command execution
- Automatic model routing based on input size or difficulty (manual `-x` only)
- Any telemetry
