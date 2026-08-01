# mcupdater

Self-updating mod sync + S3 publishing tool pair for the Wolfpack NeoForge modpack.

[![Build Status](https://img.shields.io/github/actions/workflow/status/WolfpackMC/updater/rust.yml?branch=main)](https://github.com/WolfpackMC/updater/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> Not published to crates.io. Two internal binaries built and distributed as standalone executables (see CI artifact).

## What it does

`mcupdater` runs inside a PrismLauncher instance and keeps a player's local mod set in sync with a modpack published on S3/CDN — checking version, updating NeoForge, downloading and diffing the mod archive, and applying only what changed. `deploy` is the companion tool the pack maintainer runs locally to zip, hash, and publish a new mod set to S3.

## Why

Manually distributing modpack updates (mod adds/removes/version bumps, NeoForge version bumps) to a set of players is error-prone and wastes bandwidth re-downloading unchanged mods. This tool pair automates both ends: a one-shot diffing updater for players, and a change-detecting publisher for the maintainer.

## Features

- Version-gated updates — skips work entirely if the local version already matches remote
- Byte-level diff against the existing `mods` folder; unchanged mods are left untouched, changed/removed ones are moved to a timestamped backup folder (no silent deletion)
- NeoForge version sync via `mmc-pack.json` patching
- Streamed download with progress/speed reporting
- Publisher (`deploy`) only uploads when the mod archive MD5 or NeoForge version actually changed, and auto-increments a remote version counter
- Publisher excludes `.connector`-related and `server`-scoped paths from the packaged zip

## Installation

Build from source (no crates.io install):

```bash
git clone https://github.com/WolfpackMC/updater.git
cd updater
cargo build --release
```

Produces two binaries in `target/release/`: `mcupdater` and `deploy`. CI also uploads a prebuilt `target/release` artifact on every push/PR to `main`.

## Quick start

### `mcupdater` (player-side)

Invoked by PrismLauncher as a pre-launch task, which sets the required `INST_*` environment variables. To run manually for testing:

```bash
INST_DIR="/path/to/instance" \
INST_MC_DIR="/path/to/instance/minecraft" \
./target/release/mcupdater
```

Programmatic equivalent of the core check, for reference:

```rust
let local_version: u32 = std::fs::read_to_string(version_path)
    .map(|v| v.trim().parse().unwrap_or(0))
    .unwrap_or(0);

if local_version >= remote_version && local_version != 0 {
    println!("Up to date, skipping.");
}
```

### `deploy` (maintainer-side)

```bash
./target/release/deploy
```

Zips the local mods folder, hashes it, compares against remote S3 state, and uploads only on change, bumping the remote version.

## Configuration

`mcupdater` reads these environment variables (set by PrismLauncher automatically when configured as a pre-launch command):

| Variable | Purpose |
|---|---|
| `INST_NAME`, `INST_ID` | Instance identity (currently informational) |
| `INST_DIR` | Instance root — where `version` and `mmc-pack.json` live |
| `INST_MC_DIR` | `.minecraft` dir — where `mods/` lives |
| `INST_JAVA`, `INST_JAVA_ARGS` | Passed through by PrismLauncher (currently unused) |

`deploy` reads AWS credentials from a `.env` file in the working directory (via `dotenvy`):

```
AWS_ACCESS_ID=...
AWS_ACCESS_SECRET=...
```

`deploy`'s source mods directory and S3 bucket (`wolfpackmc`) are currently hardcoded in `src/bin/deploy.rs`, not configurable via env/flags.

## Documentation

Not published to docs.rs (not a library crate). See inline docs and `CLAUDE.md` for architecture notes.

## Contributing

Open an issue or PR against `main`. CI runs `cargo build --release` and `cargo test` on every push/PR — note the project currently has no automated tests.

## License

MIT — see [LICENSE](LICENSE).
