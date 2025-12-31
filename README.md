# rebinded

Cross-platform key remapping daemon for context-sensitive function key bindings.

## Usage

```bash
# Run with default config
rebinded

# Run with custom config
rebinded --config /path/to/config.toml

# With just (see Justfile)
just run
just run --release
```

## Configuration

Place your config at `~/.config/rebinded/config.toml` (or specify with `--config`).

```toml
# Simple media control
[bindings.f13]
action = "media_play_pause"

# Context-sensitive browser navigation
[bindings.f17]
action = [
    { condition = { window = { title = "*Vivaldi*" } }, action = "browser_back" },
    { condition = { window = { title = "*Firefox*" } }, action = "browser_back" },
    # No match = defaults to passthrough
]

# Debounced scroll wheel button
[debounce.scroll]
initial_hold_ms = 110
repeat_window_ms = 2000

[bindings.f16]
action = "media_next"
debounce = "scroll"
```

### Supported Actions

- `media_play_pause`, `media_next`, `media_prev`, `media_stop`
- `browser_back`, `browser_forward`
- `passthrough` (send the original key through)
- `block` (ignore the key entirely)

### Condition Matching

Conditions support:
- `window.title` - Match window title (glob pattern)
- `window.class` - Match window class (Linux)
- `window.binary` - Match executable name (glob pattern)
- Negation with `not_` prefix: `not_title`, `not_class`, `not_binary`

All fields in a condition are ANDed. First matching rule wins.

## Development

```bash
just check    # Run clippy
just test     # Run tests
just format   # Format code
```

## License

LGPL-3.0 - see [LICENSE](LICENSE) for details.

## Author

Ryan Walters <ryan@walters.to>
