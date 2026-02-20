.PHONY: build build-dev run test lint check clean

build:
	cargo build --release

build-dev:
	cargo build

run: build
	target/release/clhorde

test:
	cargo test

lint:
	cargo clippy -- -D warnings

check: lint test

clean:
	cargo clean
