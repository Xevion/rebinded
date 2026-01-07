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

# Install rebinded binary and systemd service
install:
    cargo build --release
    install -Dm755 target/release/rebinded ~/.local/bin/rebinded
    mkdir -p ~/.config/systemd/user
    install -Dm644 rebinded.service ~/.config/systemd/user/rebinded.service
    systemctl --user daemon-reload
    systemctl --user enable rebinded.service
    @echo "✓ Installed! Start with: systemctl --user start rebinded"

# Update rebinded (rebuild and restart)
update:
    cargo build --release
    install -Dm755 target/release/rebinded ~/.local/bin/rebinded
    systemctl --user restart rebinded.service
    @echo "✓ Updated and restarted"

# Uninstall rebinded
uninstall:
    -systemctl --user stop rebinded.service
    -systemctl --user disable rebinded.service
    rm -f ~/.config/systemd/user/rebinded.service
    rm -f ~/.local/bin/rebinded
    systemctl --user daemon-reload
    @echo "✓ Uninstalled"
