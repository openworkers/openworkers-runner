.PHONY: test fmt check snapshot

# Run all runner tests
test:
	cargo test --features v8

# Format code
fmt:
	cargo fmt

# Check compilation
check:
	cargo check --features v8

# Regenerate runtime snapshot
snapshot:
	cargo run --features v8 --bin snapshot
