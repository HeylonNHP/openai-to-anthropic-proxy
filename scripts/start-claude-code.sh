#!/usr/bin/env bash
# Launch Claude Code against the local proxy.
# The proxy should already be running on the configured URL.

set -euo pipefail

ProxyUrl='http://localhost:8085'
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
previousLocation="$(pwd)"
previousAnthropicBaseUrl="${ANTHROPIC_BASE_URL-}"
previousAnthropicApiKey="${ANTHROPIC_API_KEY-}"

exitCode=0

cleanup() {
    cd "$previousLocation"

    if [[ -n "${previousAnthropicBaseUrl+x}" ]]; then
        export ANTHROPIC_BASE_URL="$previousAnthropicBaseUrl"
    else
        unset ANTHROPIC_BASE_URL
    fi

    if [[ -n "${previousAnthropicApiKey+x}" ]]; then
        export ANTHROPIC_API_KEY="$previousAnthropicApiKey"
    else
        unset ANTHROPIC_API_KEY
    fi
}
trap cleanup EXIT

cd "$repoRoot"
export ANTHROPIC_BASE_URL="$ProxyUrl"
export ANTHROPIC_API_KEY='any'

if ! command -v claude >/dev/null 2>&1; then
    echo "Claude Code CLI 'claude' was not found on PATH. Install Claude Code or open a shell where it is available." >&2
    exit 1
fi

claude "${ClaudeArgs[@]}"
exitCode=$?

exit "$exitCode"
