## Boil the ocean

When planning, do not be afraid to suggest seemingly ambitious solutions.
Agentopsy should make opaque Codex sessions understandable without sending trace
data anywhere. It must remain responsive on large trace histories while showing
enough evidence that every timing and status can be audited.

## Every number needs a receipt

A duration without provenance is misleading. Tool duration comes from matched
call and output timestamps. Model time is estimated from active-turn duration
minus the union of tool intervals. Time between turns is excluded. Keep these
rules visible in the UI and cover changes with trace fixtures.

## A diagnostic without context is not a diagnostic

Users and agents must be able to act on parse and matching failures. Diagnostics
name the session, event or `call_id`, observed condition, and relevant counts.
Never silently discard malformed records or invent precision the trace does not
contain.

## Fight for the obvious solution

Measure twice, cut once: understand the trace shape before adding special cases.
Prefer the smallest implementation that preserves accurate diagnostics,
local-only operation, and fast page loads. Do not add a database, frontend
framework, external requests, uploads, authentication, or AI analysis.

# Agentopsy Repository

Agentopsy is a Rust 2024 binary that recursively reads Codex JSONL traces from
`~/.codex/sessions` and serves a local diagnostics dashboard at
`http://127.0.0.1:8765`.

## Architecture

`src/trace.rs` discovers, parses, matches, and aggregates trace events.
`src/view.rs` renders server-side HTML with embedded CSS and minimal JavaScript.
`src/main.rs` owns the in-memory cache and localhost-only Axum server.

Trace contents never leave the process. Tool calls and outputs are matched only
by `call_id`; overlapping tool intervals count once when estimating model time.

## Code Review Rules

Be deliberately nitpicky. Report bugs, regressions, privacy risks, misleading
metrics, weak tests, unclear code, unnecessary complexity, and meaningful
consistency issues. Number findings, order them by severity, cite files and
lines, and distinguish blockers from improvements.

## Development

- Write `Agentopsy` for the project and `agentopsy` for the executable.
- Keep the server bound to `127.0.0.1:8765` and print the local URL on startup.
- Use existing dependencies before adding new ones.
- Keep Rust imports at the top of files and prefer short imports.
- Document non-obvious contracts, units, invariants, and failure behavior. Do
  not restate names, types, signatures, or implementation steps.
- Avoid `panic!`, `unreachable!`, `.unwrap()`, unsafe code, and Clippy ignores.
  Encode constraints in the type system.
- Prefer `if let` and let chains for fallibility.
- Use `#[expect(...)]` rather than `#[allow(...)]` when suppressing a lint.

## Tests

- Add focused parser tests using realistic JSONL event shapes.
- Prefer direct assertions for scalar metrics and snapshot tests for substantial
  rendered HTML or diagnostics.
- Test call/output matching, unmatched events, overlapping tools, turn gaps,
  malformed records, and command previews when changing those paths.
- Never depend on the developer's real `~/.codex/sessions` in automated tests.

## Verification

- Focused tests: `cargo test <test_name>`.
- Full suite: `just test`; it uses nextest when installed, otherwise
  `cargo test`.
- Clippy: `cargo clippy --all-targets --all-features --locked -- -D warnings`.
- Run locally: `cargo run --locked`.
- After workflow changes, run `uvx prek run --files <paths>`; actions must use
  full commit SHAs.
- Before finishing, run `uvx prek run -a`.

## Contributor Workflow

See `CONTRIBUTING.md` for documentation and pull requests.
