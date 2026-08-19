.PHONY: test test-wasm fmt check snapshot

# Run all runner tests
test:
	cargo test --features v8

# The binding test builds its guest, so this one needs the wasm32-wasip2 target
test-wasm:
	cargo test --features wasm

# Format code
fmt:
	cargo fmt

# Check compilation
check:
	cargo check --features v8

# Regenerate runtime snapshot
snapshot:
	cargo run --features v8 --bin snapshot
