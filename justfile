# The checks of the Lint workflow; each feature set compiles different code
check:
    cargo fmt --check
    cargo clippy --features v8,wasm --lib --tests --bins -- -D warnings
    cargo clippy --features wasm --lib --tests --bins -- -D warnings
    cargo clippy --features quickjs,wasm --lib --tests --bins -- -D warnings
    cargo clippy --no-default-features --features v8 --lib --tests --bins -- -D warnings
