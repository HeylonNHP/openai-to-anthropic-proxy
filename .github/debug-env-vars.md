# Debug Environment Variables

These environment-level debugging knobs were introduced during the investigation of the Atlassian tool schema 400 error. Set them before launching the proxy (`cargo run`).

---

## `DUMP_BODY`

**Type:** Boolean

**Usage:** `DUMP_BODY=1` (or `true`/`yes`/`on`)

**Purpose:** Writes the raw inbound Anthropic JSON request body to
`target/logs/dump/<timestamp>-<model>.json` for debugging tool schema errors.

**Example:**

```powershell
$env:DUMP_BODY = "1"
cargo run
```

**Note:** This feature was part of the investigation workflow but was rolled out of
the final `7d75b93` commit to keep the fix minimal. The scaffolding was added across
`src/config.rs`, `src/proxy.rs`, and the e2e test fixtures in commit `9a3c9ff`, then
reverted in the final squash. If you want to re-enable it, the code was in `9a3c9ff`
but not present in `7d75b93`.

---

## `DUMP_BODY_TOOL`

**Type:** String (substring)

**Usage:** `DUMP_BODY_TOOL=createJiraIssue` (or any substring matching a tool name)

**Purpose:** Filters which requests get dumped when `DUMP_BODY` is set. Only requests
containing a tool whose name matches the substring (case-insensitive) will have their
body written to disk. This prevents huge dumps on every request, which can be
prohibitive during active development.

**Example:**

```powershell
$env:DUMP_BODY = "1"
$env:DUMP_BODY_TOOL = "createJiraIssue"
cargo run
```

**Note:** `DUMP_BODY` must also be set (or set to "1") -- `DUMP_BODY_TOOL` has no
effect on its own.

---

Both variables are set via environment only (not `proxy.json`) and default to off/unset.
