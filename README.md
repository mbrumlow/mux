# mux

A persistent terminal session manager. Like `screen`, but focused solely on session preservation — no panes, no splits, no multiplexing.

Sessions survive disconnects. Reconnect — locally or over SSH — and pick up exactly where you left off. The server runs a headless terminal emulator with diff-based rendering at up to 60 FPS.

## Features

- **Session persistence** — the PTY keeps running when you disconnect
- **Remote sessions over SSH** — attach to a session on another host with `mux host/session`, with automatic reconnection
- **Single-client semantics** — reconnecting boots the previous client; the kicked client can reclaim the session
- **Diff-based rendering** — only changed regions are sent, up to 60 FPS
- **Transparent pass-through** — clipboard (OSC 52), mouse / DEC private modes, and the Kitty Keyboard Protocol (KKP) are forwarded when applications negotiate them
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
mux --list      # or: mux --ls

# Kill a session
mux --kill work
```

### Remote sessions

Attach to a session on another host by prefixing the session name with `host/`.
mux connects over SSH and runs the persistent server on the remote machine, so
the session survives even if your SSH connection drops — mux reconnects
automatically and restores the screen.

```sh
# Attach to session "work" on server1 over SSH
mux server1/work

# Specify user and port
mux user@server1:2222/work

# Remote session running a specific program
mux server1/work -- htop
```

Requirements and behavior:

- `mux` must be installed on the remote host and on its `PATH`. Override the
  remote binary path with the `MUX_REMOTE_BIN` environment variable.
- The remote host key must already be trusted — connect with `ssh server1`
  once first to verify and accept it.
- Authentication uses your SSH agent, then falls back to
  `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, and `~/.ssh/id_ecdsa`.
- Your SSH agent is forwarded into the session, and the forwarding target is
  refreshed on each reconnect.

## Keybindings

The prefix is `Ctrl+\`. Press it, then the command key:

| Keys | Action |
|---|---|
| `Ctrl+\` `d` | Detach from the session |
| `Ctrl+\` `k` | Kill the session |
| `Ctrl+\` `r` | Force a full screen redraw |
| `Ctrl+\` `i` | Show the session info overlay |
| `Ctrl+\` `Ctrl+\` | Send a literal `Ctrl+\` to the program |

When a client has been kicked (another client took the session) or a remote
connection is reconnecting, an overlay lets you press **SPACE**/**ENTER** to
reclaim or retry, or **q**/**ESC** to exit.

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

# Options for remote (SSH) sessions.
[ssh]
# Enable zlib compression on the SSH transport.
compression = false
```

### Environment variables

- `MUX_SESSION` is always set to the session name inside the spawned program.
- Additional variables can be configured in the `[env]` section with `$VAR` or
  `${VAR}` expansion.
- `MUX_REMOTE_BIN` overrides the path to the `mux` binary invoked on a remote
  host (default: `mux`).

## Architecture

The project is a Rust workspace with four crates:

| Crate | Role |
|---|---|
| `mux` (root) | CLI binary — session management, daemonization, config, SSH bridge |
| `persistterm-server` | Server — PTY, headless terminal emulator, diff engine |
| `persistterm-client` | Client — attach, render, input capture, SSH transport |
| `persistterm-proto` | Shared wire protocol and types |

The server spawns a PTY and parses its output through a headless
[wezterm terminal emulator](https://crates.io/crates/tattoy-wezterm-term) into
an authoritative screen model, then sends screen diffs to the client over a Unix
socket. Input flows back as raw byte events. For remote sessions, a lightweight
`bridge` process on the remote host relays that socket over the SSH channel, so
the client speaks the same protocol whether the server is local or remote.

## License

TBD
