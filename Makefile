.PHONY: test test-wasm fmt check snapshot

# Run all runner tests; the wasm ones build their guest, so this needs the
# wasm32-wasip2 target
test:
	cargo test --features v8,wasm

# Only the wasm backend, to check what a build without a JavaScript engine does
test-wasm:
	cargo test --features wasm

# Format code
fmt:
	cargo fmt

# Check compilation
check:
	cargo check --features v8,wasm

# Regenerate runtime snapshot
snapshot:
	cargo run --features v8 --bin snapshot
