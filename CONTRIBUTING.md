# Contributing

## Before Starting

[`contributor-friendly`](https://github.com/MatthewMckee4/agentopsy/issues?q=is%3Aissue%20state%3Aopen%20label%3Acontributor-friendly)
issues are ready for contributions. [`bug`](https://github.com/MatthewMckee4/agentopsy/issues?q=is%3Aissue%20state%3Aopen%20label%3Abug)
issues are also good candidates when the expected behavior is clear.

Comment before starting work so another contributor does not duplicate it and
the maintainer can confirm the issue is current. Discuss larger changes and new
features first; Agentopsy deliberately stays local-only and dependency-light.

Use [GitHub issues](https://github.com/MatthewMckee4/agentopsy/issues/new) for
bug reports, feature proposals, and documentation problems.

## Development

Run Agentopsy against local Codex traces:

```sh
cargo run --locked
```

Open <http://127.0.0.1:8765>. Never add real trace files to tests or commits;
use minimal synthetic JSONL fixtures.

Run before opening a pull request:

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
uvx prek run -a
```

## Opening a Pull Request

Keep pull requests minimal and focused. Use the pull request template and link
relevant issues. Keep it draft while substantial work remains.

Write the summary and test plan as concise prose, not lists. If CI is the only
test plan, write `ci`. Keep commits focused with descriptive one-line subjects.
Do not mix formatter churn with logic changes or add AI tools as authors.
