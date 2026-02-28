# mux

A persistent terminal session manager. Like `screen`, but focused solely on session preservation — no panes, no splits, no multiplexing.

Sessions survive disconnects. Reconnect and pick up exactly where you left off. The server runs a headless terminal emulator with diff-based rendering at up to 60 FPS.

## Features

- **Session persistence** — the PTY keeps running when you disconnect
- **Single-client semantics** — reconnecting boots the previous client
- **Diff-based rendering** — only changed regions are sent, up to 60 FPS
- **Kitty Keyboard Protocol** — KKP is supported when applications negotiate it
- **Configurable startup program and environment** via TOML config

## Installation

### With Nix

Add as a flake input:

```nix
inputs.mux.url = "github:mbrumlow/mux";

# Direct package reference
environment.systemPackages = [ inputs.mux.packages.${system}.default ];

# Or use the overlay
nixpkgs.overlays = [ inputs.mux.overlays.default ];
environment.systemPackages = [ pkgs.mux ];
```

Or build locally:

```sh
nix build
./result/bin/mux
```

### With Cargo

```sh
cargo install --path .
```

## Usage

```sh
# Create or attach to a session
mux work

# Create a session running a specific program
mux work -- emacs -nw

# Restart a session (kill existing, then create and attach)
mux -r work
mux work -r

# List active sessions
mux --list

# Kill a session
mux --kill work
```

Detach from a session with `Ctrl+\ d`.

## Configuration

Config file location: `$XDG_CONFIG_HOME/mux/config.toml` (defaults to `~/.config/mux/config.toml`).

```toml
# Default program to run when creating a session.
# Overridden by `mux <name> -- <program>` on the command line.
# If unset, defaults to $SHELL, then /bin/bash.
program = "emacs -nw"

# Environment variables set before launching the program.
# Values can reference $MUX_SESSION and existing environment variables.
[env]
SESSION_NAME = "$MUX_SESSION"
EMACS_SOCKET_NAME = "$MUX_SESSION"
```

### Environment variables

The `MUX_SESSION` environment variable is always set to the session name inside the spawned program. Additional variables can be configured in the `[env]` section with `$VAR` or `${VAR}` expansion.

## Architecture

The project is a Rust workspace with four crates:

| Crate | Role |
|---|---|
| `mux` (root) | CLI binary — session management, daemonization, config |
| `persistterm-server` | Server — PTY, headless terminal emulator, diff engine |
| `persistterm-client` | Client — attach, render, input capture |
| `persistterm-proto` | Shared wire protocol and types |

The server spawns a PTY, parses its output through a `vt100` terminal emulator, and sends screen diffs to the client over a Unix socket. Input flows back as structured events.

## License

TBD
