import { defineConfig, presets, task } from "@xevion/tempo";

const WINDOWS_TARGET = "x86_64-pc-windows-msvc";

export default defineConfig({
  tasks: [
    ...presets.rust({
      name: "rebinded",
      allFeatures: true,
      override: {
        // cargo fmt doesn't take --all-features; the preset's default would append it.
        format: "cargo fmt --all -- --check",
        "format-fix": "cargo fmt --all",
        build: false,
      },
    }),

    // Matches CI's dedicated `cargo check` step, distinct from clippy.
    task({
      name: "rebinded:check",
      body: "cargo check --all-features",
      tags: ["check"],
    }),

    // CI cross-compiles and lints Windows too; it can't run the test suite from Linux.
    task({
      name: "rebinded:check-win",
      body: `cargo check --all-targets --all-features --target ${WINDOWS_TARGET}`,
      tags: ["check"],
    }),
    task({
      name: "rebinded:lint-win",
      body: `cargo clippy --all-targets --all-features --target ${WINDOWS_TARGET} -- -D warnings`,
      tags: ["check", "lint"],
    }),

    task({
      name: "rebinded:run",
      body: "cargo run --bin rebinded",
      passthrough: true,
    }),

    task({
      name: "rebinded:install",
      body: [
        "cargo build --release",
        "install -Dm755 target/release/rebinded ~/.local/bin/rebinded",
        "mkdir -p ~/.config/systemd/user",
        "install -Dm644 rebinded.service ~/.config/systemd/user/rebinded.service",
        "systemctl --user daemon-reload",
        "systemctl --user enable rebinded.service",
        "echo '✓ Installed! Start with: systemctl --user start rebinded'",
      ].join(" && "),
    }),
    task({
      name: "rebinded:update",
      body: [
        "cargo build --release",
        "install -Dm755 target/release/rebinded ~/.local/bin/rebinded",
        "systemctl --user restart rebinded.service",
        "echo '✓ Updated and restarted'",
      ].join(" && "),
    }),
    task({
      name: "rebinded:uninstall",
      body: [
        "systemctl --user stop rebinded.service || true",
        "systemctl --user disable rebinded.service || true",
        "rm -f ~/.config/systemd/user/rebinded.service",
        "rm -f ~/.local/bin/rebinded",
        "systemctl --user daemon-reload",
        "echo '✓ Uninstalled'",
      ].join(" && "),
    }),
  ],

  commands: {
    check: {
      description: "Everything CI runs: check, clippy, and tests for Linux + Windows",
      tags: ["check"],
    },
    fmt: { description: "Format all sources", tags: ["format"] },
    start: {
      description: "Run the daemon",
      tasks: ["rebinded:run"],
      passthrough: true,
    },
    install: {
      description: "Build and install the rebinded systemd service",
      tasks: ["rebinded:install"],
    },
    update: {
      description: "Rebuild and restart the installed service",
      tasks: ["rebinded:update"],
    },
    uninstall: {
      description: "Remove the installed binary and systemd service",
      tasks: ["rebinded:uninstall"],
    },
  },
});
