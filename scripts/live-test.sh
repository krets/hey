#!/usr/bin/env bash
# Live smoke test against the real provider APIs.
#
# Reads NAME=value lines from .api_keys_for_testing (parsed, never sourced) and
# hands them to hey through the HEY_<PROVIDER>_KEY variables, the same route a
# user would use. Keys are not written to any config file. Each call is capped
# at a few tokens.
#
#   scripts/live-test.sh [path/to/hey]      HEY_LIVE_GROK_MODEL overrides the xAI model
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
hey=${1:-$root/target/release/hey}
keyfile=$root/.api_keys_for_testing
hey=$(realpath "$hey" 2>/dev/null || echo "$hey")
[ -x "$hey" ] || { echo "no binary at $hey (cargo build --release)" >&2; exit 2; }
[ -f "$keyfile" ] || { echo "missing $keyfile" >&2; exit 2; }

while IFS='=' read -r name value; do
    case $name in
        OPENAI_API_KEY)  export HEY_OPENAI_KEY=$value ;;
        CLAUDE_API_KEY|ANTHROPIC_API_KEY) export HEY_ANTHROPIC_KEY=$value ;;
        GEMINI_API_KEY|GOOGLE_API_KEY)    export HEY_GEMINI_KEY=$value ;;
        XAI_API_KEY)     export HEY_GROK_KEY=$value ;;
    esac
done < "$keyfile"

# Isolated config: results must not depend on the developer's ~/.hey.
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
export HEY_CONFIG=$tmp/.hey HOME=$tmp
cd "$tmp"
"$hey" config set core.max_tokens 32
"$hey" config set provider.grok.type openai
"$hey" config set provider.grok.url https://api.x.ai/v1
"$hey" config set provider.grok.model "${HEY_LIVE_GROK_MODEL:-grok-4}"

fail=0
check() { # name, provider, extra flags...
    local label=$1 provider=$2; shift 2
    local out rc
    out=$(HEY_MODEL= "$hey" -n -p "$provider" "$@" "Reply with the single word: pong" 2>&1); rc=$?
    if [ $rc -eq 0 ] && [ -n "$out" ]; then
        printf 'ok    %-22s %s\n' "$label" "$(printf %s "$out" | head -c 60 | tr '\n' ' ')"
    else
        printf 'FAIL  %-22s exit %d: %s\n' "$label" $rc "$(printf %s "$out" | head -c 300 | tr '\n' ' ')"; fail=1
    fi
}

for p in anthropic openai gemini grok; do
    if [ -n "$(eval "printf %s \"\${HEY_$(echo $p | tr a-z A-Z)_KEY:-}\"")" ]; then
        check "$p" "$p"
    else
        echo "skip  $p (no key in .api_keys_for_testing)"
    fi
done

# Streaming path, one provider per wire format.
"$hey" config set core.stream true
for p in anthropic openai gemini; do
    [ -n "$(eval "printf %s \"\${HEY_$(echo $p | tr a-z A-Z)_KEY:-}\"")" ] && check "$p (stream)" "$p"
done
"$hey" config unset core.stream

# Shell-integration context reaches a real model.
out=$("$hey" -p anthropic --shell bash --exit-code 127 --last-command "gti status" "Which command did I mean? Answer with the command only." 2>&1)
case $out in *"git status"*) echo "ok    context (last cmd)     $out" ;; *) echo "FAIL  context (last cmd)     $out"; fail=1 ;; esac

"$hey" doctor --ping | grep -E 'ping|key:' | sed 's/^/      /'
exit $fail
