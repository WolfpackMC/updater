# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Rust CLI toolset for the "Wolfpack" NeoForge modpack, distributed via S3/CDN. Two binaries in one crate:

- `src/main.rs` → `wolfpacker` — client-side updater. Runs inside a PrismLauncher instance (reads `INST_NAME`, `INST_ID`, `INST_DIR`, `INST_MC_DIR`, `INST_JAVA`, `INST_JAVA_ARGS` env vars set by the launcher). Checks remote version at `https://wolfpackmc.s3.us-east-1.amazonaws.com/version`, updates NeoForge version in `mmc-pack.json`, downloads `WFP.zip`, diffs it against the local `mods` folder (MD5-equivalent byte comparison), backs up changed/removed mods into a timestamped `mods_backup_<epoch>` folder, and applies the new mod set.
- `src/bin/deploy.rs` → `deploy` — dev/publisher-side tool. Zips a local mods folder (hardcoded path `E:\PrismLauncher\instances\server\minecraft`), computes its MD5, compares against S3 state (`WFP.hash`, `version`, `neoforge-version` objects in bucket `wolfpackmc`), and uploads only if something changed, bumping `version` by 1. Reads AWS creds from `.env` (`AWS_ACCESS_ID`, `AWS_ACCESS_SECRET`) via `dotenvy`.

`deploy` is only ever run locally by the pack maintainer (hardcoded Windows path) — not part of CI and not meant to be portable.

## Commands

```bash
cargo build --release          # builds both wolfpacker and deploy
cargo run --bin wolfpacker      # run the updater (needs INST_* env vars set, as PrismLauncher would)
cargo run --bin deploy         # run the publisher tool (needs .env with AWS_ACCESS_ID/AWS_ACCESS_SECRET, and the hardcoded mods dir to exist)
cargo test                     # no tests currently exist in this repo
```

CI (`.github/workflows/rust.yml`) runs `cargo build --release`, `cargo test`, and uploads `target/release` as an artifact on push/PR to `main`.

## Architecture notes

- Both binaries independently read/write the same `mmc-pack.json` NeoForge component (`uid == "net.neoforged"`) — one to apply the remote version locally, the other to read the local version for upload comparison. Keep the component-lookup logic consistent if changed in one place.
- Remote state is a flat set of objects in the `wolfpackmc` S3 bucket, mirrored at CDN `https://wolfpack-cdn.kalkafox.dev`: `version` (u32 counter), `WFP.hash` (MD5 of the mods zip), `WFP.zip` (the mods archive), `neoforge-version` (string). `wolfpacker` reads through the S3 URL directly (not the CDN) for `version`/`neoforge-version`/`WFP.zip`.
- Version comparison in `wolfpacker` treats local version `0` as "unset" (always update), not "up to date" — see the `local_version >= remote_version && local_version != 0` check in `main.rs`.
- Mod sync in `wolfpacker` is not a naive overwrite: existing mods identical to the incoming version are left in place; anything else present locally gets moved to a timestamped backup dir before the new set is moved in from the staging dir (`.mods_staging` under `INST_MC_DIR`).
- `deploy`'s zipping step excludes any path containing `.connector` or a `server` path segment.
