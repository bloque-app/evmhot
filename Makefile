.PHONY: build test fmt clippy check image
build: ; cargo build --release --bin evmhot
test: ; cargo test
fmt: ; cargo fmt --all
clippy: ; cargo clippy --all-targets -- -D warnings
check: ; cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings
image: ; docker build -t evmhot:local .
