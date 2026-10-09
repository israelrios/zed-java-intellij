# Build WASM extension in release mode
ext-build:
    cargo build --release

# Format all code
fmt:
    cargo fmt --all
    ts_query_ls format languages

# Run clippy on the extension
clippy:
    cargo clippy --all-targets --fix --allow-dirty

# Format and clippy all code
lint: fmt clippy

# Build everything
all: lint ext-build
