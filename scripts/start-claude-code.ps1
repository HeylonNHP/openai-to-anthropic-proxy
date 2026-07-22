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
#      SDK prefers it over ANTHROPIC_API_KEY, so we remove it for the
#      duration of the child and rely on ANTHROPIC_API_KEY='any'.
#   3. CLAUDE_CODE_CHILD_SESSION / CLAUDE_CODE_ENTRYPOINT / etc. set by
#      a parent Claude Code session. We remove them so the child is not
#      treated as a sub-agent inheriting the parent's model.
#
# PowerShell has no equivalent of `env -i`, so we cannot fully isolate
# the child's environment the way the bash version does. Instead we
# explicitly scrub the variables that are known to leak. The
# --setting-sources flag in Layer 1 is the real fix; the scrubbing is
# belt-and-braces.

[CmdletBinding()]
param(
    [string]$ProxyUrl = 'http://localhost:8085',
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ClaudeArgs
)

# Default setting-sources: skip the user-level ~/.claude/settings.json,
# which is what is pinning Claude Code to the Ollama endpoint. This is
# the standard, supported way to opt out for a single invocation.
# PowerShell's parameter binder will eat any --setting-sources the user
# passes, so we expose this as a constant. Users who need a different
# value can pass --setting-sources=... directly to claude (our loop
# detects that form and skips auto-injection).
$SettingSources = 'project,local'

if ($null -eq $ClaudeArgs) {
    $ClaudeArgs = @()
}

# ─── Argument parsing ──────────────────────────────────────────────────
# PowerShell's parameter binder already handles the first positional
# arg as $ProxyUrl (it's the first declared [string] parameter, which
# is implicitly Position=0). We only need to scan the remaining args
# for --setting-sources=... so we can suppress our auto-injection and
# not pass the flag twice.
$userSettingSources = $false
$newClaudeArgs = @()
foreach ($a in $ClaudeArgs) {
    if ($a -like '--setting-sources=*') {
        # The user is overriding our default. Forward to claude and
        # suppress our auto-injection so we don't pass the flag twice.
        $userSettingSources = $true
        $newClaudeArgs += $a
        continue
    }
    $newClaudeArgs += $a
}

if (-not [Uri]::IsWellFormedUriString($ProxyUrl, [UriKind]::Absolute)) {
    throw "ProxyUrl must be an absolute URL like http://localhost:8085."
}

$ClaudeArgs = $newClaudeArgs

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path

# ─── Capture/restore helpers ───────────────────────────────────────────
# Like the bash version: record each var's prior value (or absence) so
# cleanup() can restore the caller's env exactly.
$scrubbedVars = @(
    'ANTHROPIC_BASE_URL',
    'ANTHROPIC_API_KEY',
    'ANTHROPIC_AUTH_TOKEN',
    'ANTHROPIC_CUSTOM_HEADERS',
    'CLAUDE_CODE_CHILD_SESSION',
    'CLAUDE_CODE_ENTRYPOINT',
    'CLAUDE_CODE_SESSION_ID',
    'CLAUDE_CODE_PARENT_SESSION_ID',
    'CLAUDE_CODE_SSE_PORT'
)

$prior = @{}
foreach ($name in $scrubbedVars) {
    $val = [Environment]::GetEnvironmentVariable($name, 'Process')
    if ($null -ne $val) {
        $prior[$name] = @{ Set = $true; Value = $val }
    }
    else {
        $prior[$name] = @{ Set = $false; Value = $null }
    }
}

$exitCode = 0

try {
    if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
        throw "Claude Code CLI 'claude' was not found on PATH. Install Claude Code or open a shell where it is available."
    }

    # ─── Layer 4: fast proxy preflight (advisory only) ─────────────
    # ~1.5s timeout via HttpClient so the user's wait is bounded.
    try {
        $client = [System.Net.Http.HttpClient]::new()
        $client.Timeout = [TimeSpan]::FromSeconds(1.5)
        $req = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::Get, "$ProxyUrl/v1/models")
        $req.Headers.Add('Authorization', 'Bearer any')
        $client.SendAsync($req).GetAwaiter().GetResult() | Out-Null
    }
    catch {
        Write-Warning @"
proxy not reachable on $ProxyUrl (preflight failed: $($_.Exception.Message)).
  -> start it with:  cargo run --release
  -> or pass a different URL as the first arg, e.g.:
       scripts/start-claude-code.ps1 http://localhost:9090
  Continuing anyway; claude will surface the real error.
"@
    }

    # ─── Set proxy env for the child ───────────────────────────────
    $env:ANTHROPIC_BASE_URL = $ProxyUrl
    $env:ANTHROPIC_API_KEY = 'any'
    # ANTHROPIC_AUTH_TOKEN is intentionally removed so the SDK falls
    # back to ANTHROPIC_API_KEY. (It was captured+restored above.)
    foreach ($v in 'ANTHROPIC_AUTH_TOKEN',
                       'CLAUDE_CODE_CHILD_SESSION',
                       'CLAUDE_CODE_ENTRYPOINT',
                       'CLAUDE_CODE_SESSION_ID',
                       'CLAUDE_CODE_PARENT_SESSION_ID',
                       'CLAUDE_CODE_SSE_PORT') {
        if (Test-Path "Env:$v") {
            Remove-Item "Env:$v" -ErrorAction SilentlyContinue
        }
    }

    # ─── Layer 3: proxy_key → X-Proxy-Key header ──────────────────
    # If the proxy is configured with `proxy_key` (env PROXY_KEY or
    # proxy.json), every request must include a matching `X-Proxy-Key`
    # header or the proxy returns 401. The Anthropic SDK reads custom
    # headers from the `ANTHROPIC_CUSTOM_HEADERS` env var, which takes
    # a `Name: Value` string. We forward `PROXY_KEY` from the calling
    # shell if it's set.
    if (Test-Path 'Env:PROXY_KEY') {
        $env:ANTHROPIC_CUSTOM_HEADERS = "X-Proxy-Key: $env:PROXY_KEY"
        Write-Host 'note: forwarding PROXY_KEY to the child as X-Proxy-Key.' -ForegroundColor Yellow
    }

    # ─── Layer 1: --setting-sources=project,local ──────────────────
    # Skips the user-level ~/.claude/settings.json, which is what is
    # pinning Claude Code to the Ollama endpoint. This is the standard,
    # supported way to opt out for a single invocation. Skip if the
    # user already passed --setting-sources (in any form).
    if (-not $userSettingSources) {
        $ClaudeArgs = @('--setting-sources', $SettingSources) + $ClaudeArgs
    }

    & claude @ClaudeArgs
    $exitCode = $LASTEXITCODE
}
finally {
    foreach ($name in $scrubbedVars) {
        $entry = $prior[$name]
        if ($entry.Set) {
            [Environment]::SetEnvironmentVariable($name, $entry.Value, 'Process')
        }
        else {
            if (Test-Path "Env:$name") {
                Remove-Item "Env:$name" -ErrorAction SilentlyContinue
            }
        }
    }
}

$global:LASTEXITCODE = $exitCode
exit $exitCode
