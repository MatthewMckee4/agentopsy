# Agentopsy

> See what your agent is doing.

Agentopsy reads Codex JSONL traces from `~/.codex/sessions` and serves a local diagnostics dashboard.

```sh
cargo run
```

Open <http://127.0.0.1:8765>.

Sessions are ranked by active-turn duration. Time between turns is excluded. Tool duration is derived by matching calls and outputs through `call_id`; overlapping tool calls count once. Model time is estimated as active-turn duration minus tool execution time.

Agentopsy binds only to localhost. It has no uploads, authentication, database, external requests, frontend framework, or AI analysis.
