# WhisperDrop

A background cross-platform (macOS & Windows) file-transfer app with an **always-on-top screen-edge drop zone**, LAN discovery, and an optional cross-network tunnel.

Drag a file onto the 12px strip at the right edge of your screen — it expands to a 160px blurred drop target and streams the file to every discovered peer on your WiFi. No IPs to type, nothing buffered in memory.

## How it works

- **Edge drop zone** — a slim glowing tab on the right (or left) screen edge. The whole edge height is a drop target: drag a file toward it and it grows into a "Drop to send" card; drop, and a glass panel lists nearby devices (and your tunnel group) with live per-file progress. Click the tab to peek at devices, right-click for the menu (switch edge, open received files, preferences, quit).
- **Every screen, every Space** — one drop zone per monitor on both macOS and Windows, re-created when displays are plugged or unplugged. On macOS it joins every Space and stays above fullscreen apps.
- **Same look on both OS** — macOS renders `src/edge.html` in a transparent Tauri window (`src-tauri/src/edge.rs` manages the windows); Windows draws the identical design with a software renderer into per-pixel-alpha layered windows (`cli/src/glass.rs`, `cli/src/dropzone.rs`) — no browser runtime needed.
- **One binary, send and receive** — `cli/` builds `whisperdrop`: run it with no arguments and it is the full app (receiver, edge drop zone, tray, tunnel); the subcommands work from any shell, even while the app is running:

  ```bash
  whisperdrop devices                          # LAN peers + tunnel group members
  whisperdrop send report.pdf                  # to the only device on the LAN
  whisperdrop send *.jpg --to DESKTOP-9F2K     # by device name, or --to 192.168.1.20
  whisperdrop send big.zip --to 482913         # 6-digit group id → encrypted tunnel
  whisperdrop status                           # config summary + relay check
  whisperdrop setup                            # wizard
  whisperdrop group create                     # new group, this device is the head
  whisperdrop group join 482913                # ask the head to admit this device
  whisperdrop group pending | approve <id> | members | kick <device> | leave
  ```
- **Streaming receiver** (`src-tauri/src/server.rs`) — an axum HTTP server on port **51730** (auto-increments if busy) that streams request bodies chunk-by-chunk to `~/Downloads/BridgeReceived/`. Handles 10GB+ files without memory spikes; filenames are sanitized and collisions deduplicated (`a (1).txt`).
- **Streaming sender** (`src-tauri/src/client.rs`) — `reqwest` POST with a `ReaderStream`-wrapped `tokio::fs::File`, so the file is never fully loaded into RAM.
- **mDNS discovery** (`src-tauri/src/mdns.rs`) — registers `_bridge-service._tcp.local.` and browses for peers; `get_online_peers()` returns live peers (stale entries expire after 75s, self filtered out).
- **Owned groups** — cross-network transfers go through a group on the relay. One device *creates* the group and becomes its **head**; the relay issues the 6-digit number (never chosen by a client, never reused). Any other device *requests* to join with that number, and the head approves or denies it right in the drop zone (or via `whisperdrop group approve`). Only approved devices hold a member token, and the relay refuses everything else — knowing the number alone reveals nothing: no presence, no file names, no frames. The head can list, remove and re-admit members.
- **Trusted tunnel pairing** — on top of membership, set the same pairing passphrase on each device in Preferences. Tunnel payloads then use XChaCha20-Poly1305 end-to-end encryption; the relay sees only encrypted bytes. Never share the passphrase in chat or add it to source control.
- **Transfer queue and history** — multiple dropped files are sent sequentially, with per-file progress, failure state and an activity log available from the tray.
- **System tray** — Preferences, received-files folder, activity log and Quit. Windows is a GUI/tray app rather than a console process.

## Install

**Homebrew (macOS / Linux)**

```bash
brew tap kridsadar357/whisperdrop
brew trust kridsadar357/whisperdrop      # newer Homebrew asks once for third-party taps
brew install whisperdrop
```

**Prebuilt binaries** — every [release](https://github.com/kridsadar357/whisperdrop/releases) ships `whisperdrop` for macOS (arm64 + Intel), Linux x86_64 and Windows x86_64 (`.zip`), with `checksums.txt`. The Windows zip is the full app + CLI; the macOS GUI app (`WhisperDrop.app`) is built separately with `npm run tauri build` and needs signing before it can be distributed as a cask.

## Development

```bash
npm install
npm run tauri dev
```

```bash
cd src-tauri
cargo test      # 50MB loopback streaming test with SHA256 verification
cargo clippy
```

Build a release bundle: `npm run tauri build`.

### Releasing

```bash
git tag v0.3.1 && git push origin v0.3.1
```

`.github/workflows/release.yml` builds `whisperdrop` for all four targets, publishes the GitHub release with `checksums.txt`, and — if the repo has a `TAP_GITHUB_TOKEN` secret (a PAT with write access to `homebrew-whisperdrop`) — regenerates the tap formula. Without the secret, update it by hand:

```bash
gh release download vX.Y.Z --pattern checksums.txt
scripts/gen-formula.sh kridsadar357 X.Y.Z checksums.txt > ../homebrew-whisperdrop/Formula/whisperdrop.rb
```

`tap-check.yml` then installs the formula on a clean macOS runner.

The `whisperdrop` binary (Windows app + CLI on every platform; cross-compiled for Windows from the Mac with Homebrew MinGW):

```bash
cd cli
cargo build --release                                  # native CLI: target/release/whisperdrop
cargo build --release --target x86_64-pc-windows-gnu   # Windows app+CLI
cp target/x86_64-pc-windows-gnu/release/whisperdrop.exe ../dist/WhisperDrop.exe
```

## Release operations

`installer/WhisperDrop.iss` builds a Windows setup executable with Inno Setup on Windows. Release signing/notarization and the HTTPS update manifest are documented in [release/README.md](release/README.md). Those operations require organization-owned certificates and credentials and are intentionally not automated with secrets in this repository.

## Tunnel security

Two layers protect cross-network transfers:

1. **Membership** — the relay only routes between devices holding a member token for the same group. Tokens are issued when the head creates the group or approves a join request, stored in the local config, and revoked by `kick`/`leave`. Guessing a group number gets you nothing.
2. **Encryption** — enable Tunnel and enter an identical, strong pairing passphrase on every device in the group. The passphrase derives an in-memory XChaCha20-Poly1305 key; file contents are opaque to the relay. Never put the passphrase in relay frames, chat or source control.

LAN transfers remain direct and use your local network's security boundary.

## Transfer between two instances on one machine

The server port auto-increments on collision, and each instance registers separately over mDNS, so running two instances locally works — each receives into the same `~/Downloads/BridgeReceived/`.
