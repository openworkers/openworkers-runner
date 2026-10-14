# All runner tests; the wasm ones build their guest and need the wasm32-wasip2 target
test:
    cargo test --features v8,wasm

# Only the wasm backend, to check what a build without a JavaScript engine does
test-wasm:
    cargo test --features wasm

fmt:
    cargo fmt

# The checks of the Lint workflow; each feature set compiles different code
check:
    cargo fmt --check
    cargo clippy --features v8,wasm --lib --tests --bins -- -D warnings
    cargo clippy --features wasm --lib --tests --bins -- -D warnings
    cargo clippy --features quickjs,wasm --lib --tests --bins -- -D warnings
    cargo clippy --no-default-features --features v8 --lib --tests --bins -- -D warnings

# Regenerate runtime snapshot
snapshot:
    cargo run --features v8 --bin snapshot
