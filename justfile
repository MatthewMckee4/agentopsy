# https://just.systems

run:
    cargo run --locked

test *args:
    @if command -v cargo-nextest > /dev/null 2>&1; then \
        cargo nextest run --locked {{args}}; \
    else \
        cargo test --locked {{args}}; \
    fi

check:
    cargo fmt --all --check
    cargo test --locked
    cargo clippy --all-targets --all-features --locked -- -D warnings
    uvx prek run -a
