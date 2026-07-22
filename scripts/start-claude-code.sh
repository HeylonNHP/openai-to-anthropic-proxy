#!/usr/bin/env bash
# Launch Claude Code against the local proxy.
# The proxy should already be running on the configured URL.
#
# This script defends against three ways Claude Code can end up routed
# somewhere other than the local proxy:
#   1. The user's ~/.claude/settings.json "env" block (which Claude Code
#      re-applies on every request, overriding the process env). We pass
#      --setting-sources=project,local so the user-level file is not
#      loaded for this invocation.
#   2. ANTHROPIC_AUTH_TOKEN leaked from the calling shell. The Anthropic
#      SDK prefers it over ANTHROPIC_API_KEY, so we unset it for the
#      duration of the child and rely on ANTHROPIC_API_KEY='any'.
#   3. CLAUDE_CODE_CHILD_SESSION / CLAUDE_CODE_ENTRYPOINT / etc. set by
#      a parent Claude Code session. We unset them so the child is not
#      treated as a sub-agent inheriting the parent's model.
#
# We additionally run claude under `env -i` with a minimal allowlist,
# so even if Layer 1 fails for some Claude Code build, the env-var
# bleed from settings.json cannot reach the spawned process.

set -euo pipefail

ProxyUrl='http://localhost:8085'
SettingSources='project,local'
ClaudeArgs=()

# Parse args: first non-flag arg (if any) is ProxyUrl; rest are passed through.
# Flags (anything starting with -) and subsequent args are forwarded to claude.
positional=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --)
            shift
            ClaudeArgs+=("$@")
            break
            ;;
        --setting-sources)
            # Accept --setting-sources explicitly so the user can override
            # the default (e.g. to add 'user' back, or restrict to 'project').
            shift
            [[ $# -gt 0 ]] || { echo "--setting-sources requires a value." >&2; exit 1; }
            SettingSources="$1"
            shift
            ;;
        --setting-sources=*)
            SettingSources="${1#--setting-sources=}"
            shift
            ;;
        -*)
            ClaudeArgs+=("$1")
            shift
            ;;
        *)
            positional+=("$1")
            shift
            ;;
    esac
done

if [[ ${#positional[@]} -gt 0 ]]; then
    ProxyUrl="${positional[0]}"
fi

if ! [[ "$ProxyUrl" =~ ^https?:// ]]; then
    echo "ProxyUrl must be an absolute URL like http://localhost:8085." >&2
    exit 1
fi

repoRoot="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Capture every variable we intend to touch so cleanup() can restore
# the caller's environment exactly. We use a sentinel because
# `${!name+x}` errors under `set -u` when the var is unset.
_SAVED_SENTINEL='__start_claude_code_unset__'
save_var() {
    local name="$1"
    # `${!name:-x}` -> $x if the var named $name is unset, else its value.
    # (Bash evaluates the default branch only when the var is unset, so
    # this also handles set -u correctly.)
    local val="${!name:-$_SAVED_SENTINEL}"
    if [[ "$val" == "$_SAVED_SENTINEL" ]]; then
        printf -v "previous_${name//[^A-Za-z0-9_]/_}_set" '%s' no
    else
        printf -v "previous_${name//[^A-Za-z0-9_]/_}" '%s' "$val"
        printf -v "previous_${name//[^A-Za-z0-9_]/_}_set" '%s' yes
    fi
}

restore_var() {
    local name="$1"
    local slot="previous_${name//[^A-Za-z0-9_]/_}"
    local set_slot="${slot}_set"
    if [[ "${!set_slot-}" == "yes" ]]; then
        export "$name=${!slot}"
    else
        unset "$name"
    fi
}

# env vars we mutate or that leak from settings.json / a parent session.
for v in \
    ANTHROPIC_BASE_URL \
    ANTHROPIC_API_KEY \
    ANTHROPIC_AUTH_TOKEN \
    CLAUDE_CODE_CHILD_SESSION \
    CLAUDE_CODE_ENTRYPOINT \
    CLAUDE_CODE_SESSION_ID \
    CLAUDE_CODE_PARENT_SESSION_ID \
    CLAUDE_CODE_SSE_PORT; do
    save_var "$v"
done

exitCode=0

cleanup() {
    for v in \
        ANTHROPIC_BASE_URL \
        ANTHROPIC_API_KEY \
        ANTHROPIC_AUTH_TOKEN \
        CLAUDE_CODE_CHILD_SESSION \
        CLAUDE_CODE_ENTRYPOINT \
        CLAUDE_CODE_SESSION_ID \
        CLAUDE_CODE_PARENT_SESSION_ID \
        CLAUDE_CODE_SSE_PORT; do
        restore_var "$v"
    done
}
trap cleanup EXIT

# ─── Layer 4: fast proxy preflight (advisory only) ─────────────────────
# We don't want claude to sit for a minute on a generic SDK timeout if
# the proxy simply isn't running. 1.5s is short enough to be invisible
# on a healthy machine and long enough to catch a missing listener.
if command -v curl >/dev/null 2>&1; then
    if ! curl --silent --show-error --output /dev/null --max-time 1.5 \
            -H "Authorization: Bearer any" "$ProxyUrl/v1/models" 2>/dev/null; then
        cat >&2 <<EOF
warning: proxy not reachable on $ProxyUrl (curl preflight failed).
  -> start it with:  cargo run --release
  -> or pass a different URL as the first arg, e.g.:
       scripts/start-claude-code.sh http://localhost:9090
  Continuing anyway; claude will surface the real error.
EOF
    fi
fi

# If we were launched from inside an existing Claude Code session, the
# parent may have already pinned a model. The env-i + setting-sources
# layers below should still redirect the new claude, but flag it so
# the user isn't surprised if the parent overrides anything.
if [[ -n "${CLAUDE_CODE_CHILD_SESSION+x}" || -n "${CLAUDE_CODE_SESSION_ID+x}" ]]; then
    echo "note: launched from inside a Claude Code session. The proxy-mapped model and base URL will apply to the new claude process; the parent session is unaffected." >&2
fi

# ─── Set proxy env for the child ───────────────────────────────────────
export ANTHROPIC_BASE_URL="$ProxyUrl"
export ANTHROPIC_API_KEY='any'
# ANTHROPIC_AUTH_TOKEN is intentionally left unset so the SDK falls
# back to ANTHROPIC_API_KEY. (It was captured+restored above.)
unset ANTHROPIC_AUTH_TOKEN
unset CLAUDE_CODE_CHILD_SESSION CLAUDE_CODE_ENTRYPOINT \
      CLAUDE_CODE_SESSION_ID CLAUDE_CODE_PARENT_SESSION_ID \
      CLAUDE_CODE_SSE_PORT

if ! command -v claude >/dev/null 2>&1; then
    echo "Claude Code CLI 'claude' was not found on PATH. Install Claude Code or open a shell where it is available." >&2
    exit 1
fi

# ─── Layer 1: --setting-sources=project,local ──────────────────────────
# Skips the user-level ~/.claude/settings.json, which is what is pinning
# Claude Code to the Ollama endpoint. This is the standard, supported
# way to opt out for a single invocation.
# Skip if the user already passed --setting-sources (in any form) to
# claude — let their value win instead of duplicating the flag.
UserSettingSources=0
for a in "${ClaudeArgs[@]}"; do
    if [[ "$a" == --setting-sources || "$a" == --setting-sources=* ]]; then
        UserSettingSources=1
        break
    fi
done
if [[ $UserSettingSources -eq 0 ]]; then
    ClaudeArgs=(--setting-sources "$SettingSources" "${ClaudeArgs[@]}")
fi

# ─── Layer 2: env -i with minimal allowlist ────────────────────────────
# Guarantee that no ANTHROPIC_* / CLAUDE_CODE_* bleed from the calling
# shell can reach claude, even if a future Claude Code build ignores
# --setting-sources. We pass through only the variables claude needs
# to find its binary, render output, and locate the user's config dir.
#
# Note: values must not contain literal newlines (env -i does not
# support that). Locale-related vars are fine in practice.
allowlist=(PATH HOME USER SHELL LANG LC_ALL TERM TMPDIR)
[[ -n "${DISPLAY-}" ]] && allowlist+=(DISPLAY)
[[ -n "${XDG_RUNTIME_DIR-}" ]] && allowlist+=(XDG_RUNTIME_DIR)
[[ -n "${COLORTERM-}" ]] && allowlist+=(COLORTERM)
[[ -n "${SSH_AUTH_SOCK-}" ]] && allowlist+=(SSH_AUTH_SOCK)
[[ -n "${SSH_CONNECTION-}" ]] && allowlist+=(SSH_CONNECTION)
[[ -n "${NO_PROXY-}" ]] && allowlist+=(NO_PROXY)
[[ -n "${http_proxy-}" ]] && allowlist+=(http_proxy)
[[ -n "${https_proxy-}" ]] && allowlist+=(https_proxy)

env_args=()
for v in "${allowlist[@]}"; do
    # `${v-}` is empty if unset; skip empties to avoid `VAR=` (which
    # is technically a set-but-empty var and some tools treat as a
    # non-empty string).
    val="${!v-}"
    [[ -n "$val" ]] || continue
    env_args+=("$v=$val")
done

# These two must always be set explicitly for the child:
env_args+=("ANTHROPIC_BASE_URL=$ProxyUrl")
env_args+=("ANTHROPIC_API_KEY=any")

# ─── Layer 3: proxy_key → X-Proxy-Key header ────────────────────────────
# If the proxy is configured with `proxy_key` (env PROXY_KEY or
# proxy.json), every request must include a matching `X-Proxy-Key`
# header or the proxy returns 401. The Anthropic SDK reads custom
# headers from the `ANTHROPIC_CUSTOM_HEADERS` env var, which takes
# a `Name: Value` string. We forward `PROXY_KEY` from the calling
# shell if it's set, and strip the var from the allowlist so it
# doesn't get passed twice or leak into the child unintended.
if [[ -n "${PROXY_KEY-}" ]]; then
    env_args+=("ANTHROPIC_CUSTOM_HEADERS=X-Proxy-Key: $PROXY_KEY")
    echo "note: forwarding PROXY_KEY to the child as X-Proxy-Key." >&2
fi

env -i "${env_args[@]}" claude "${ClaudeArgs[@]}"
exitCode=$?

exit "$exitCode"
