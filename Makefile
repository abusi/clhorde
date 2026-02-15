.PHONY: build test lint check clean

build:
	cargo build

test:
	cargo test

lint:
	cargo clippy -- -D warnings

check: lint test

clean:
	cargo clean
