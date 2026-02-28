# Persistent Remote Terminal with Kitty Keyboard Protocol (KKP) Negotiation (Rust Design Doc)

**Status:** Design draft  
**Primary goal:** *Session preservation* for a single interactive terminal session, with **exactly one active client** at a time.  
**Secondary goals:** Accurate input handling with **Kitty Keyboard Protocol (KKP)** when negotiated by applications inside the PTY; efficient **diff-based rendering** with a **maximum of 60 FPS**, and **immediate flush on input**.

This document is intentionally verbose and implementation-oriented. It is written to be handed to an AI coding agent.

---

## Table of Contents

1. [Problem Statement](#problem-statement)  
2. [Non-Goals](#non-goals)  
3. [High-Level Overview](#high-level-overview)  
4. [Core Concepts and Invariants](#core-concepts-and-invariants)  
5. [System Architecture](#system-architecture)  
6. [Session Preservation and Single-Client Semantics](#session-preservation-and-single-client-semantics)  
7. [Terminal Emulation Model](#terminal-emulation-model)  
8. [Input Handling](#input-handling)  
   1. [Client-side decoding](#client-side-decoding)  
   2. [Structured input events](#structured-input-events)  
   3. [Server-side PTY input encoding](#server-side-pty-input-encoding)  
   4. [KKP negotiation detection](#kkp-negotiation-detection)  
9. [Rendering Pipeline](#rendering-pipeline)  
   1. [Screen model](#screen-model)  
   2. [Diff algorithm](#diff-algorithm)  
   3. [60 FPS scheduler + input-triggered flush](#60-fps-scheduler--input-triggered-flush)  
   4. [Client renderer](#client-renderer)  
10. [Transport and Wire Protocol](#transport-and-wire-protocol)  
11. [PTY Management](#pty-management)  
12. [Resizing](#resizing)  
13. [Robustness: Resync, Drift, and Full Snapshots](#robustness-resync-drift-and-full-snapshots)  
14. [Security Model](#security-model)  
15. [Observability](#observability)  
16. [Testing Plan](#testing-plan)  
17. [Implementation Plan / Milestones](#implementation-plan--milestones)  
18. [Module Layout (Rust Crate Structure)](#module-layout-rust-crate-structure)  
19. [Appendix A: Data Structures (Rust Sketch)](#appendix-a-data-structures-rust-sketch)  
20. [Appendix B: Protocol Message Sketch](#appendix-b-protocol-message-sketch)  

---

## Problem Statement

We want a program "like `screen`" **only in the sense of session preservation**, not multiplexing. There is exactly **one logical session** running on the server side:

- A persistent PTY (shell + child processes) continues running even when clients disconnect.
- When a new client connects, it **boots** the previous client (if any) and becomes the only active viewer/input source.
- The program supports modern keyboard input via **Kitty Keyboard Protocol (KKP)** *when applications request it*, but must degrade gracefully when the **client terminal** does not support KKP.
- Rendering is done via **server-side terminal emulation** and **diff-based frame updates**, with a **max of 60 FPS** (16.67ms per frame) and the ability to **flush immediately on input** for interactivity.

The key conceptual decision:

> KKP is not forced. Applications inside the PTY negotiate KKP by emitting escape sequences.  
> The server (as a headless terminal emulator) detects KKP enable/disable sequences and switches PTY input encoding accordingly.

---

## Non-Goals

- **No multiplexing of panes/windows/tabs**.
- **No shared collaborative sessions**.
- **No attempt to invent a new terminal type** (we will use `TERM=xterm-256color` by default).
- **No hard requirement that every app uses KKP**. If an app never enables KKP, input remains legacy encoding.
- **No attempt to provide full terminal compatibility for every obscure feature** initially (start with “works for shells + common TUIs”).
- **No guarantee of perfect behavior when the client terminal lacks the ability to produce certain key combos** (degrade gracefully).
- **No browser client** in initial scope (but protocol should not preclude it).

---

## High-Level Overview

The system is split into **server** and **client** components.

### Server responsibilities

- Spawn and manage a PTY and child process (shell).
- Parse PTY output (ANSI/VT escapes) into a canonical **screen model**.
- Track terminal modes relevant to rendering and input (cursor, alt screen, bracketed paste, KKP enabled state, etc.).
- Produce **frame diffs** at a maximum of **60 FPS**, coalescing output between frames.
- On input events, **immediately render/flush** the next frame (do not wait for the next tick).
- Provide single-client semantics: only one client receives frames and can send input at a time.

### Client responsibilities

- Attach to server and perform a capability handshake (including whether local terminal supports KKP).
- Capture local terminal input, decode into structured `KeyEvent`s (using KKP decoder if available; otherwise legacy decoder).
- Send structured input events to server.
- Receive server frames and render them to the local terminal by emitting escape sequences (SGR, cursor moves, etc.).
- Handle resize events and forward to server.

---

## Core Concepts and Invariants

### Invariants

1. **Exactly one live client** per server session.
2. Server maintains the **authoritative screen model**.
3. Client is a “dumb renderer” that applies server diffs.
4. Input is transported as **structured events**, not raw bytes.
5. Server decides PTY input encoding based on **app negotiation** (KKP enable/disable observed in PTY output).

### Why structured input?

Raw terminal byte streams are ambiguous and vary by terminal. A structured input stream enables:

- KKP support without “byte soup” escaping problems.
- Cleaner fallbacks (client may not support KKP but server can still reason about keys).
- Future extension (mouse events, command mode, etc.) without breaking protocol.

---

## System Architecture

### Components

- `persistterm-server`: daemon or foreground process that owns PTY and session state.
- `persistterm-client`: attach program run inside a local terminal.

### Data flow

```
Local Terminal
    ↓ (keys)
Client: decode → KeyEvent stream
    ↓ (wire protocol)
Server: encode → PTY input bytes
    ↓
PTY child process

PTY output bytes
    ↓
Server: ANSI parser → Screen model
    ↓ (frame diff at <= 60fps)
Client: apply diff → emit terminal escapes
    ↓
Local Terminal
```

---

## Session Preservation and Single-Client Semantics

### Server is the session owner

The server continues to run regardless of clients. The PTY persists.

### Connection rules

- When a client connects:
  - If no active client: it becomes active.
  - If another client is active: server **disconnects** the old client and activates the new one.
- Server should send a **full snapshot** to the new client immediately on activation.

### Booting policy

- Booting an old client should be graceful:
  - send a “kicked” control message, then close.
  - stop accepting input from the old client immediately.

### Client reconnection UX

- Client can reconnect and pick up exactly where the PTY is.
- The initial frame after connection should match the server’s current screen model.

---

## Terminal Emulation Model

This project requires a **headless terminal emulator** inside the server:

- It parses ANSI/VT sequences and updates a screen model.
- It tracks cursor position, attributes, modes, and alt-screen state.

### Libraries / approach

Prefer using a well-tested Rust crate for ANSI/VT parsing rather than writing from scratch.

Candidates (examples; choose based on fit):
- `vt100` crate (common terminal state machine)
- or a minimal custom parser + `vte` crate (escape parsing) with custom screen model

The important part is that we end up with an internal **grid** representation that we can diff.

---

## Input Handling

### Client-side decoding

The client reads raw stdin bytes. It should attempt to enable and decode KKP **only if the local terminal supports it**.

#### Detecting local terminal KKP support

Pragmatic approach:
- If `TERM` begins with `xterm-kitty` or `KITTY_WINDOW_ID` exists, assume KKP support.
- Optionally allow a CLI flag `--kkp=on|off|auto`.
- In `auto`, attempt to enable KKP and then detect KKP-formatted sequences.

**Important:** local terminals that do not support KKP may ignore the enable sequence harmlessly.

#### Enabling local KKP

Client can emit the kitty keyboard protocol enable sequence. Specific sequences vary by “level”; implement the commonly used one and allow configuration.

### Structured input events

Define a stable `KeyEvent` type:

- `KeyCode`: Unicode text key or named key (Enter, Escape, Left, F1, etc.)
- `Modifiers`: bitflags (shift/alt/ctrl/super)
- `Action`: press/repeat/release (if available)
- `Text`: optional text payload (IME/composition; can be left None in MVP)

All input (KKP or legacy) becomes `KeyEvent`.

### Server-side PTY input encoding

Server maintains a per-session `PtyInputMode`:

- `Legacy`
- `KittyKeyboardProtocol` (enabled because app asked for it)

When receiving `KeyEvent` from the client:
- If `PtyInputMode == Legacy`: encode to legacy sequences (xterm-ish).
- If `PtyInputMode == KKP`: encode to KKP sequences.

**Degrading gracefully:** If the client cannot produce a certain key combination in legacy mode, it cannot be recovered. But basic input must still work.

### KKP negotiation detection

Server must observe PTY output to detect when apps enable/disable KKP.

Mechanism:
- As server parses PTY output, it should recognize the specific escape sequences used to enable/disable KKP.
- On enable: `PtyInputMode = KKP`
- On disable: `PtyInputMode = Legacy`

Even if TERM is `xterm-256color`, apps that want KKP can still enable it by emitting the sequence.

---

## Rendering Pipeline

### Screen model

At minimum:

- `width`, `height`
- `cells[y][x]` where each `Cell` includes:
  - grapheme (char(s))
  - style (fg/bg colors, bold/italic/underline/reverse, etc.)
  - width (1 or 2 for wide chars)
- cursor position + cursor style
- current attributes state
- alternate screen buffer support

### Diff algorithm

Goal: produce patch operations at most 60fps.

Suggested diff representation:
- Row-based spans:
  - For each row `y`, find contiguous spans where cells changed.
  - Emit ops like `SetRowSpans(y, spans)` where each span includes `x_start` and `styled_text_runs`.

This keeps protocol simple and efficient.

### 60 FPS scheduler + input-triggered flush

Requirements:
- Do not exceed 60 frames per second.
- If no changes since last frame, do not send a frame.
- If input arrives, flush ASAP (send frame immediately) to keep interaction snappy.

Implementation approach:
- Maintain a `dirty` flag set by:
  - PTY output handling (screen changed)
  - input handling (we expect a reaction; also helps echo)
- Use a timer tick (16.67ms). On tick:
  - if dirty: compute diff and send frame, clear dirty.
- On input:
  - set dirty
  - schedule a “flush now” task that runs ASAP, but still respects max fps:
    - if last frame was sent < 16.67ms ago, delay until allowed
    - else send immediately

This gives “immediate” while still enforcing a max rate.

### Client renderer

Client keeps a local cache of current screen state (or at least cursor + style state) to minimize escapes.

When applying `SetRowSpans`, client:
- moves cursor to `(x,y)`
- sets SGR style for runs
- prints text
- optionally clears trailing region if needed

Client must also:
- hide/show cursor during bulk updates if it reduces flicker
- restore final cursor state at end of frame

---

## Transport and Wire Protocol

### Transport options

Start simple:
- Use a single duplex byte stream (e.g., TCP or Unix socket) or SSH stdio.
- The protocol is application-level framed messages.

### Framing

Use length-prefixed binary frames:
- `u32 length` + `u8 message_type` + payload
- Payload encoded via `bincode`, `postcard`, or a custom encoding.

**Recommendation:** `postcard` (no-std-friendly, compact) with `serde` types.

### Message types

Client → Server:
- `Hello { client_capabilities }`
- `Input { seq, events }`
- `Resize { width, height }`
- `Ping { t }`

Server → Client:
- `Welcome { server_info, session_id }`
- `Snapshot { full_screen_state }`
- `Frame { seq, ops, cursor_state }`
- `Kicked { reason }`
- `Pong { t }`

---

## PTY Management

Use a Rust PTY crate appropriate for Unix (initially):
- `portable-pty` is a strong candidate.

Server should:
- spawn PTY with child process (shell default `/bin/bash` or user’s `$SHELL`)
- set environment (`TERM=xterm-256color`)
- read PTY output asynchronously
- write PTY input bytes produced by encoder

Windows support can be future work (ConPTY), but start on Unix.

---

## Resizing

Client detects terminal resize (SIGWINCH):
- send `Resize { w, h }`

Server:
- resize PTY
- update screen model dimensions (may require reflow depending on parser lib)
- mark dirty, then send a snapshot or diff

Often resizing is simpler if you send a full snapshot after resize.

---

## Robustness: Resync, Drift, and Full Snapshots

Even with a correct diff engine, clients can desync due to bugs or partial writes.

Add a periodic checksum:
- server computes a hash of the composed screen model every N frames
- include it in `Frame`
- client can compute hash of its local model
- if mismatch: request `Snapshot`

Alternatively simpler:
- client can request `Snapshot` anytime via a control message when it detects weirdness.

---

## Security Model

Assume a trusted environment at first. Later, add:
- optional authentication token
- restrict to localhost sockets
- if using TCP, use SSH tunnel or TLS

Do not run the server as root.

---

## Observability

- Structured logs (`tracing` crate)
- Metrics counters:
  - frames_sent
  - bytes_sent/received
  - input_events
  - average diff size
  - frame time
- Debug mode: dump protocol frames to file for replay.

---

## Testing Plan

### Unit tests

- KeyEvent encoders (legacy and KKP)
- KKP enable/disable detection from PTY output
- Diff engine correctness (apply diff and compare screen)
- Protocol codec round-trip

### Integration tests

- Spawn server + client connected via loopback
- Run a known command in PTY (e.g., `printf` patterns)
- Verify client renders expected output
- Simulate reconnect (disconnect client, reconnect new one) and verify snapshot correctness

### Fuzzing (optional but good)

- Fuzz ANSI parser inputs
- Fuzz diff engine

---

## Implementation Plan / Milestones

### Milestone 1: Basic persistent PTY + snapshot rendering

- Server spawns PTY and parses output into screen model.
- Client connects and receives periodic full snapshots (no diffs yet).
- Input forwarded as raw bytes (temporary).

### Milestone 2: Structured input + legacy encoding

- Client decodes legacy keys into KeyEvent.
- Server encodes KeyEvent to legacy PTY bytes.

### Milestone 3: Diff-based rendering @ 60fps

- Implement dirty flag, tick scheduler, and diff ops.
- Client applies diffs.

### Milestone 4: KKP decode (client) + KKP encode (server) + negotiation tracking

- Add KKP decoder on client (auto/flag).
- Add KKP encoder on server.
- Add detection of KKP enable/disable in PTY output; switch PTY input mode.

### Milestone 5: Reconnect/boot semantics hardened

- Single active client enforced.
- Full snapshot on connect.
- Kick old client on new connect.

---

## Module Layout (Rust Crate Structure)

Workspace:

- `persistterm-proto`  
  - Protocol message definitions (serde).
  - Shared types: `KeyEvent`, `Cell`, `FrameOp`.

- `persistterm-server`  
  - PTY management
  - Terminal parsing + screen model
  - Diff engine
  - Frame scheduler
  - Connection handling and single-client logic

- `persistterm-client`  
  - Terminal capability detection
  - Input capture + decoder (legacy + KKP)
  - Renderer that applies server frame ops
  - Resize detection

Suggested internal modules (server):
- `pty.rs`
- `terminal.rs` (ANSI parser integration)
- `screen.rs` (models)
- `diff.rs`
- `scheduler.rs`
- `net.rs` (protocol + framing)
- `session.rs` (single-client + snapshot)

Suggested internal modules (client):
- `input.rs`
- `kkp.rs`
- `legacy_keys.rs`
- `render.rs`
- `net.rs`
- `resize.rs`

---

## Appendix A: Data Structures (Rust Sketch)

```rust
// persistterm-proto

use serde::{Serialize, Deserialize};
use bitflags::bitflags;

bitflags! {
    #[derive(Serialize, Deserialize)]
    pub struct Modifiers: u8 {
        const SHIFT = 0b0001;
        const ALT   = 0b0010;
        const CTRL  = 0b0100;
        const SUPER = 0b1000;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8), // F1..F24 => 1..24
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub mods: Modifiers,
    pub action: KeyAction,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    // cursor shape could be added here
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Color {
    Default,
    Ansi256(u8),
    Rgb(Rgb),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    pub ch: char,
    pub style: CellStyle,
    pub width: u8, // 1 or 2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>, // row-major: y*width + x
    pub cursor: CursorState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub x: u16,
    pub text: String,
    pub style: CellStyle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrameOp {
    SetSize { width: u16, height: u16 },
    SetCursor(CursorState),
    SetRowSpans { y: u16, spans: Vec<Span> },
    ClearRowFrom { y: u16, x: u16 }, // optional
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub seq: u64,
    pub ops: Vec<FrameOp>,
    pub cursor: CursorState,
    pub checksum: Option<u64>,
}
```

---

## Appendix B: Protocol Message Sketch

```rust
// persistterm-proto

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCapabilities {
    pub supports_kkp: bool,
    pub supports_truecolor: bool,
    pub term: String, // local TERM
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum C2S {
    Hello { caps: ClientCapabilities },
    Input { seq: u64, events: Vec<KeyEvent> },
    Resize { width: u16, height: u16 },
    Ping { t: u64 },
    RequestSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum S2C {
    Welcome { session_id: String },
    Snapshot(ScreenSnapshot),
    Frame(Frame),
    Kicked { reason: String },
    Pong { t: u64 },
}
```

---

## Notes for the Implementer

1. The server must never exceed 60 FPS.  
2. The server should flush a frame ASAP on input, but still respect the max frame interval.  
3. KKP is enabled only when negotiated by apps (detected in PTY output).  
4. If the client cannot generate certain keys (no KKP), accept degraded behavior.  
5. Keep the protocol versioned from day one (even if v1 only).

---

**End of document.**
