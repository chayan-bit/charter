# MCP server for AI agents

Status: implemented, tested on the in-memory fake, and driven live on Rabdi by a Codex agent (`docs/live/agent-bounty.md`).
Date: 2026-11-07.
Builds on `docs/p3/agents.md` (scoped grants), `docs/p3/cob-ops.md`, `docs/p3/escrow.md`; code in `crates/charter-mcp`.

## Why

`docs/p3/agents.md` gives an agent key bounded authority, but the agent still has to drive a CLI by parsing text.
`chart-mcp` exposes the same commands as Model Context Protocol tools with JSON schemas, so any MCP client (Claude Code, Claude Desktop, other agent harnesses) can find a bounty, push a fix, attach a proof and post the patch.

## SDK survey

| Candidate | Version, licence | Maintenance | Fit |
| --- | --- | --- | --- |
| `rmcp` (modelcontextprotocol/rust-sdk, the official SDK) | 3.9.0, Apache-2.0 | pushed 2026-11-05, about 4,100 stars, 45.8 M downloads, 67 open issues, many maintainers | `#[tool]` macros generate the JSON schema from `schemars` structs; stdio transport behind `transport-io` |
| `rust-mcp-sdk` (rust-mcp-stack) | 2.0.0, MIT | updated 2026-08-27, about 200 stars, 0.3 M downloads | full-featured, but a much smaller user base and more single-maintainer risk |
| `mcp-protocol-sdk` (mcp-rust) | 0.5.1 | last release 2025-08-03 | stale, behind the current protocol |

We picked `rmcp`: it is the reference implementation, it is actively maintained, its licence is compatible with this workspace's `MIT OR Apache-2.0`, and with `default-features = false` plus `server`, `macros`, `transport-io` it adds only `rmcp`, `rmcp-macros`, `schemars_derive`, `serde_derive_internals` and `signal-hook-registry` to the lockfile (`schemars`, `tokio` and `serde` were already there).

## Design

The server is a thin wrapper: every tool runs one `chart` (or `git`) child process, so signing, fetching, sealed views of private repos, program folds and escrow rules stay in the CLI with no duplication.
It finds the CLI through `CHARTER_BIN`, else `chart` next to its own executable, else `chart` on `PATH`, and it puts that directory first on the children's `PATH` so `git` finds `git-remote-arkiv`.
Children inherit the server's environment, so the agent's identity is whatever `CHARTER_HOME` points at and the backend is `CHARTER_BACKEND` (Rabdi by default, with the optional `ARKIV_ACCESS_KEY`).
No tool takes or returns key material.

A failed child is a tool error (`isError: true`) whose text is `{"code", "message", "exit", "stdout"}`.
`code` is the first stable `E_*` code in the child's stderr (for example `E_PROGRAM_REJECTED`, `E_INSUFFICIENT_FUNDS`, `E_BAD_FORMAT`), else `usage` or `flagged` from the CLI's own line, else `null` (a plain git failure); `exit` is the CLI exit status (1 verification, 2 usage, 3 transport, 4 funds).

## Tools

| Tool | Arguments | Runs | Returns |
| --- | --- | --- | --- |
| `repo_clone` | `repo`, `dir` | `git clone arkiv::<rid> <dir>` | `{dir}` |
| `git_push` | `repo`, `dir`, `branch`, optional `message` | `git add -A`, `git commit` (when `message` is set), `git push <url> HEAD:refs/heads/<branch>` | `{head}` |
| `op_list` | `repo`, optional `type`, `action`, `object` | `chart op list --json` | JSON array of verified ops `{id, object, author, type, action, parents, ts_ms, block, payload}` |
| `op_post` | `repo`, `type`, `action`, optional `object`, `parents`, `payload` | `chart op post` | `{id}` |
| `attach_proof` | `repo`, `payload`, `receipt`, `public`, optional `type` (default `xyz.arkiv.patch`), `action` (default `create`), `object`, `parents` | `chart op attach-proof` | `{id}` of the op that carries `proofs` |
| `program_status` | `repo` | `chart program status` | the fold: state, heads, rejected pushes, obligations |
| `escrow_status` | `repo` | `chart escrow audit` (keyless) | the ledger; a flagged ledger is an error with code `flagged` |

`repo` is `arkiv::<rid>` or the bare rid.
`action` filtering in `op_list` happens in the server, because the CLI filters only by type and object.
`git_push` runs git in the server process, so an agent whose own shell is sandboxed without network (Codex `workspace-write`) can still push, and the access key stays in the server's environment instead of the model's shell.
Relative `dir`, `receipt` and `public` paths resolve against the server's working directory, which is also the working area: a path that starts with `-`, contains `..`, or resolves (through symlinks) outside it is refused with code `usage`, so a prompt-injected agent cannot pass git an option or post a file from elsewhere, such as a key, as a receipt.
`repo` must be `arkiv::z...` or a bare rid, and every value reaches git after `--` and the CLI as `--name=value`.

`chart op attach-proof` is the CLI half of `attach_proof`: it chunks the receipt into `xyz.arkiv.blob` ops of at most 30,000 bytes, skips chunks whose `sha256` is already posted on the repo (so a retry after `E_INSUFFICIENT_FUNDS` only posts what is missing), then posts the given op with `{kind: "risc0", receipt, chunks, public}` appended to its payload's `proofs`.

## Setup with Claude Code

Build or install both binaries so they sit in one directory:

```sh
cargo install --path crates/charter-cli
cargo install --path crates/charter-mcp
chart keys init   # the agent's own identity, never a human's key
```

Register the server for the current project:

```sh
claude mcp add charter -e CHARTER_HOME="$HOME/.charter-agent" -- chart-mcp
```

Or commit an `.mcp.json`:

```json
{
  "mcpServers": {
    "charter": {
      "command": "chart-mcp",
      "env": { "CHARTER_HOME": "/path/to/agent/.charter" }
    }
  }
}
```

Put `ARKIV_ACCESS_KEY` in the environment Claude Code starts in rather than in the JSON, so it never lands in a committed file.
A headless agent run allows only the MCP tools, git and file edits:

```sh
claude -p "<task>" --mcp-config .mcp.json --strict-mcp-config \
  --allowedTools "mcp__charter__*" Read Edit Write
```

## Setup with Codex

Register the server once:

```sh
codex mcp add charter --env CHARTER_HOME="$HOME/.charter-agent" -- chart-mcp
```

Or put it in `~/.codex/config.toml`:

```toml
[mcp_servers.charter]
command = "chart-mcp"
env = { CHARTER_HOME = "/path/to/agent/.charter" }
env_vars = ["ARKIV_ACCESS_KEY"]        # forwarded by name, never written here
tool_timeout_sec = 900                 # attach_proof posts several blob ops
default_tools_approval_mode = "approve"
```

The live demo passes the same settings per run with `-c` and leaves the user config alone:

```sh
codex exec --json --ephemeral --ignore-user-config --skip-git-repo-check \
  -s workspace-write -C <session dir> \
  -c 'mcp_servers.charter.command="/abs/path/chart-mcp"' \
  -c 'mcp_servers.charter.cwd="<session dir>"' \
  -c 'mcp_servers.charter.env={CHARTER_HOME="<agent home>/.charter"}' \
  -c 'mcp_servers.charter.env_vars=["ARKIV_ACCESS_KEY"]' \
  -c 'mcp_servers.charter.tool_timeout_sec=900' \
  -c 'mcp_servers.charter.default_tools_approval_mode="approve"' \
  "<task>"
```

Without `default_tools_approval_mode = "approve"`, `codex exec` refuses every MCP call with "MCP tool call requires approval, but approval policy is never".

## Tests

`crates/charter-mcp/src/tests.rs` drives every tool through the real `chart` binary on the file-persisted fake chain: post and list with type and action filters, clone, `git_push` of an edit to the pusher's namespace, `attach_proof` twice to check blob reuse and the `proofs` entry, `program_status`, and coded errors from the CLI (`E_*` with exit 1), from git (exit 128) and from the escrow audit.
`crates/charter-cli/tests/agent_bounty.rs` now posts its proof with `op attach-proof` and still checks `proven: true`, the approval gate and the escrow payout end to end.
A stdio handshake (`initialize`, `tools/list`) against the built binary lists the tools with their schemas.

## Live demo

`demo/agent-bounty.sh` runs the whole bounty with a headless Codex CLI as the agent: `setup` creates the alice, escrow, agent and bank identities, `alice` creates the repo, appoints the escrow, pins `agent-bounty.rhai` with `agent_proof()` set to `"required"`, grants the agent `patch` with a cap, opens an issue whose `spec_hash` matches the committed receipt and deposits the bounty, `agent` runs `codex exec` with this MCP server and a workspace-write sandbox without network, `merge` shows `main` refusing the agent head until alice approves, records the merge and runs `escrow run --once`, and `verify` runs the keyless audit and reads the payout tx and the agent's balance delta from the chain.
`demo/show-agent.py` turns the `codex exec --json` events into one line per agent message, MCP call, edit and shell command.
`crates/charter-cli/tests/rabdi_agent.rs` funds the demo from the throwaway key (one transfer, at most 0.08 tGLM) and sweeps it back; `demo/agent-bounty.tape` records the live phases.
Results, transactions and costs are in `docs/live/agent-bounty.md`.
Video of the live run: https://raw.githubusercontent.com/chayan-bit/charter/demo-assets/agent-bounty-codex-live.mp4 (GIF: https://raw.githubusercontent.com/chayan-bit/charter/demo-assets/agent-bounty-codex-live.gif).

A headless Claude Code run was tried first and could not authenticate in this environment (`claude auth status` reports `loggedIn: false`; `claude -p` inside the desktop app's session fails with "OAuth session expired and could not be refreshed"); the `.mcp.json` setup above is unchanged for a logged-in Claude Code.

## Limits

- One child process per call: `op_list` and `program_status` refetch from Arkiv each time, which is fine for an agent loop but not for a hot path.
- `program_status` returns the CLI's text lines (the state line is JSON) rather than a parsed object.
- The server trusts its own environment: whoever starts it chooses the identity it signs with, so give an agent its own `CHARTER_HOME` and a grant, never a delegate's home.
