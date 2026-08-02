# mcupdater

Self-updating mod sync + S3 publishing tool pair for the Wolfpack NeoForge modpack.

[![Build Status](https://img.shields.io/github/actions/workflow/status/WolfpackMC/updater/rust.yml?branch=main)](https://github.com/WolfpackMC/updater/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> Not published to crates.io. Two internal binaries built and distributed as standalone executables (see CI artifact).

## What it does

`mcupdater` runs inside a PrismLauncher instance and keeps a player's local mod set in sync with a modpack published on S3/CDN — checking version, updating NeoForge, downloading and diffing the mod archive, and applying only what changed. `deploy` is the companion tool the pack maintainer runs locally to zip, hash, and publish a new mod set to S3.

Both read/write through CloudFront (`wolfpack-cdn.kalkafox.dev`) rather than hitting the `wolfpackmc` S3 bucket directly, for regional download speed. Content-addressed objects (`mods/{hash}.jar`) are cached for a year (immutable — the hash guarantees the content never changes at that key). Mutable pointers (`version`, `neoforge-version`, `manifest.json`, `packs.json`, the `.mrpack`) are cached for only 60 seconds, so a publish is visible everywhere within a minute without needing an explicit CloudFront invalidation.

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
./target/release/deploy --force   # republish even if nothing changed (also (re)builds the mrpack + packs.json)
./target/release/deploy --server  # publish the dedicated-server mod set instead (skips mrpack/packs.json)
```

Zips the local mods folder, hashes it, compares against remote S3 state, and uploads only on change, bumping the remote version.

On any client-side publish that changes mods or the NeoForge version, `deploy` also builds and uploads a `.mrpack` (every mod listed in `files[]` pointing at the same `mods/{hash}.jar` blobs it just published, no Modrinth lookups needed) and updates `packs.json` at the bucket root — the manifest PrismLauncher's "Wolfpack" new-instance page reads to list installable packs. Only a fixed allowlist of top-level dirs (`config/`, `icon.png`, `options.txt`, `servers.dat`) is included as overrides; everything else under the instance's minecraft dir (saves, screenshots, logs, per-player caches, stray zips, etc.) is deliberately left out. Server publishes (`--server`) skip mrpack/packs.json entirely, since there's no from-scratch instance-creation flow for the server side.

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
