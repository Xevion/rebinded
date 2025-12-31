# Default recipe
default:
    @just --list

# Run clippy
clippy *args:
    cargo clippy --all-targets --all-features {{args}}

# Alias for clippy
check *args:
    @just clippy {{args}}

# Run clippy for Windows target
check-win *args:
    cargo clippy --all-targets --all-features --target x86_64-pc-windows-msvc {{args}}

# Run tests
test *args:
    cargo nextest run {{args}}

# Format code
format:
    cargo fmt

# Run the daemon (forwards all args to cargo run)
run *args:
    cargo run --bin rebinded {{args}}
