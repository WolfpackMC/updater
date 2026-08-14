use md5::{Digest, Md5};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::{env, fs, io};
use std::fs::{rename, File};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const CDN_URL: &str = "https://wolfpackmc.s3.us-east-1.amazonaws.com";

#[derive(serde::Deserialize)]
struct ModEntry {
    name: String,
    hash: String,
    profiles: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ResourcePackEntry {
    name: String,
    hash: String,
}

#[derive(serde::Deserialize)]
struct FtbQuestEntry {
    name: String,
    hash: String,
}

fn parse_arg(flag: &str, default: &str) -> String {
    let args: Vec<String> = env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn parse_flag(flag: &str) -> bool {
    env::args().any(|a| a == flag)
}

fn file_md5(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Md5::new();
    let mut buffer = [0u8; 8192];

    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Walks `dir` and MD5-hashes every file in parallel (rayon, bounded by available cores).
/// `key_fn` derives the map key (and an arbitrary extra payload, e.g. the ".disabled" flag)
/// from the path relative to `dir`; returning `None` skips the file.
fn hash_dir_parallel<T, F>(dir: &Path, key_fn: F) -> anyhow::Result<HashMap<String, (String, T)>>
where
    T: Send,
    F: Fn(String) -> Option<(String, T)> + Sync,
{
    let entries: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .collect();

    entries
        .par_iter()
        .filter_map(|entry| {
            let rel = entry
                .path()
                .strip_prefix(dir)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            let (key, extra) = key_fn(rel)?;
            let hash = match file_md5(entry.path()) {
                Ok(h) => h,
                Err(e) => return Some(Err(anyhow::anyhow!(e))),
            };
            Some(Ok((key, (hash, extra))))
        })
        .collect()
}

fn read_neoforge_version(mmc_pack_path: &Path) -> anyhow::Result<String> {
    let contents = fs::read_to_string(mmc_pack_path)?;
    let mmc_pack: serde_json::Value = serde_json::from_str(&contents)?;

    let version = mmc_pack
        .get("components")
        .and_then(|c| c.as_array())
        .and_then(|components| {
            components
                .iter()
                .find(|c| c.get("uid").and_then(|u| u.as_str()) == Some("net.neoforged"))
        })
        .and_then(|c| c.get("version"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("NeoForge component not found in mmc-pack.json"))?;

    Ok(version.to_string())
}

fn write_neoforge_version(mmc_pack_path: &Path, remote_version: &str) -> anyhow::Result<()> {
    let contents = fs::read_to_string(mmc_pack_path)?;
    let mut mmc_pack: serde_json::Value = serde_json::from_str(&contents)?;

    let components = mmc_pack
        .get_mut("components")
        .and_then(|c| c.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("mmc-pack.json missing components array"))?;

    let neoforge = components
        .iter_mut()
        .find(|c| c.get("uid").and_then(|u| u.as_str()) == Some("net.neoforged"))
        .ok_or_else(|| anyhow::anyhow!("NeoForge component not found in mmc-pack.json"))?;

    neoforge["version"] = serde_json::Value::String(remote_version.to_string());
    neoforge["cachedVersion"] = serde_json::Value::String(remote_version.to_string());

    fs::write(mmc_pack_path, serde_json::to_string_pretty(&mmc_pack)?)?;

    Ok(())
}

// PrismLauncher can rewrite mmc-pack.json from its own in-memory component state, silently
// clobbering an external edit — this can happen well after the pre-launch command exits, so
// checking for the process isn't a valid gate (Prism is always running; it's the parent).
// Verify the write actually stuck and retry; if it never sticks, fail loudly instead of
// proceeding with a mismatched loader.
fn update_neoforge_version(inst_dir: &str, remote_version: &str) -> anyhow::Result<bool> {
    let mmc_pack_path = Path::new(inst_dir).join("mmc-pack.json");

    if read_neoforge_version(&mmc_pack_path)? == remote_version {
        return Ok(false);
    }

    const MAX_ATTEMPTS: u32 = 3;
    for attempt in 1..=MAX_ATTEMPTS {
        write_neoforge_version(&mmc_pack_path, remote_version)?;
        std::thread::sleep(std::time::Duration::from_secs(2));

        if read_neoforge_version(&mmc_pack_path)? == remote_version {
            return Ok(true);
        }

        println!(
            "NeoForge version write did not stick (attempt {}/{}), retrying...",
            attempt, MAX_ATTEMPTS
        );
    }

    anyhow::bail!(
        "Failed to persist NeoForge version {} into mmc-pack.json after {} attempts. \
         PrismLauncher is likely re-saving the file from a stale in-memory copy of this \
         instance's component list — fully restart PrismLauncher (not just this instance) \
         and retry.",
        remote_version,
        MAX_ATTEMPTS
    )
}

// PrismLauncher-only (dedicated servers have no instance.cfg/launcher and set heap/JVM args via
// their own start script/egg config). Patched in place with `config_patch::patch_properties_text`,
// the same key=value patcher `config-patch.json` uses for .properties files — instance.cfg's
// `[General]` header has no `=`, so it (and every other line we don't own) passes through
// untouched. This only takes effect on the *next* launch: PrismLauncher reads instance.cfg to
// build the JVM's args before this pre-launch command ever runs, so a change made here can't
// affect the game process about to start.
//
// Each setting group needs its own `Override*` flag flipped on for PrismLauncher to actually use
// the value instead of its own default/inherited one, so only flip the flag for a group whose
// keys are actually present in `remote` — an empty `JvmArgs` upstream (nothing published) must
// not force `OverrideJavaArgs=true` and blank out a player's own custom args.
fn update_launch_settings(inst_dir: &str, remote: &BTreeMap<String, String>) -> anyhow::Result<bool> {
    if remote.is_empty() {
        return Ok(false);
    }

    let cfg_path = Path::new(inst_dir).join("instance.cfg");
    let text = fs::read_to_string(&cfg_path).unwrap_or_default();
    let current = wolfpacker::config_patch::extract_properties(&text);

    // A player who's bumped MaxMemAlloc above the pack's default has a reason to (more RAM to
    // spare) — never clamp them back down to the published value, only ever raise it.
    let mut effective = remote.clone();
    if let (Some(remote_max), Some(current_max)) = (
        remote.get("MaxMemAlloc").and_then(|v| v.parse::<i64>().ok()),
        current.get("MaxMemAlloc").map(|v| v.to_string()).and_then(|v| v.parse::<i64>().ok()),
    ) {
        if current_max > remote_max {
            effective.remove("MaxMemAlloc");
        }
    }

    let up_to_date = effective.iter().all(|(k, v)| {
        current.get(k).map(|cv| &cv.to_string() == v).unwrap_or(false)
    });
    if up_to_date {
        return Ok(false);
    }

    let mut patch: BTreeMap<String, wolfpacker::config_patch::ConfigValue> = effective
        .iter()
        .map(|(k, v)| (k.clone(), wolfpacker::config_patch::ConfigValue::String(v.clone())))
        .collect();
    if effective.contains_key("MinMemAlloc") || effective.contains_key("MaxMemAlloc") {
        patch.insert(
            "OverrideMemory".to_string(),
            wolfpacker::config_patch::ConfigValue::String("true".to_string()),
        );
    }
    if remote.contains_key("JvmArgs") {
        patch.insert(
            "OverrideJavaArgs".to_string(),
            wolfpacker::config_patch::ConfigValue::String("true".to_string()),
        );
    }

    let patched = wolfpacker::config_patch::patch_properties_text(&text, &patch);
    fs::write(&cfg_path, patched)?;

    Ok(true)
}

// Server variant has no mmc-pack.json (no PrismLauncher) and no automated loader install here —
// just track the last-seen remote version in a plain file so a mismatch is visible in logs
// and the operator knows to reinstall the loader (e.g. via the Pterodactyl egg).
fn check_neoforge_version_file(inst_dir: &str, remote_version: &str) -> anyhow::Result<bool> {
    let version_file = Path::new(inst_dir).join("neoforge-version");

    let local_version = fs::read_to_string(&version_file).ok();
    let changed = local_version.as_deref() != Some(remote_version);

    fs::write(&version_file, remote_version)?;

    Ok(changed)
}

/// Downloads every `(name, hash)` pair not already present with a matching hash, in parallel
/// (rayon, bounded by available cores). `url_for` builds the download URL from an entry's hash;
/// `disk_name_for` builds the on-disk filename from an entry (used to preserve e.g. a mod's
/// ".disabled" suffix across an update).
fn download_missing_parallel<'a, T, F, N>(
    client: &reqwest::blocking::Client,
    dest_dir: &Path,
    entries: &'a [T],
    is_up_to_date: impl Fn(&T) -> bool + Sync,
    disk_name_for: N,
    url_for: F,
) -> anyhow::Result<Vec<&'a T>>
where
    T: Sync,
    F: Fn(&T) -> String + Sync,
    N: Fn(&T) -> String + Sync,
{
    let to_download: Vec<&T> = entries.iter().filter(|e| !is_up_to_date(e)).collect();

    to_download
        .par_iter()
        .try_for_each(|entry| -> anyhow::Result<()> {
            let on_disk_name = disk_name_for(entry);
            println!("Downloading {}...", on_disk_name);
            let target_path = dest_dir.join(&on_disk_name);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }

            let mut response = client.get(url_for(entry)).send()?;
            let mut out_file = File::create(&target_path)?;
            io::copy(&mut response, &mut out_file)?;
            Ok(())
        })?;

    Ok(to_download)
}

fn sync_mods(
    client: &reqwest::blocking::Client,
    prefix: &str,
    profile: &str,
    inst_mc_dir: &str,
) -> anyhow::Result<()> {
    println!("Fetching mod manifest...");
    let full_manifest: Vec<ModEntry> = client
        .get(format!("{}/{}/manifest.json", CDN_URL, prefix))
        .send()?
        .json()?;

    let manifest: Vec<ModEntry> = full_manifest
        .into_iter()
        .filter(|m| m.profiles.iter().any(|p| p == &profile))
        .collect();

    let mods_dir = Path::new(inst_mc_dir).join("mods");
    fs::create_dir_all(&mods_dir)?;

    println!("Hashing local mods...");
    // keyed by base name (Prism's ".disabled" suffix stripped), so a disabled mod still
    // matches its manifest entry instead of being treated as missing/stale.
    let local_hashes: HashMap<String, (String, bool)> = hash_dir_parallel(&mods_dir, |rel| {
        let disabled = rel.ends_with(".disabled");
        let base = rel.strip_suffix(".disabled").unwrap_or(&rel).to_string();
        Some((base, disabled))
    })?;

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let backup_dir = Path::new(inst_mc_dir).join(format!("mods_backup_{}", timestamp));
    let mut backup_created = false;

    println!("Backing up outdated and removed mods...");
    for (base_name, (local_hash, disabled)) in &local_hashes {
        let up_to_date = manifest
            .iter()
            .any(|m| &m.name == base_name && &m.hash == local_hash);

        if !up_to_date {
            if !backup_created {
                fs::create_dir_all(&backup_dir)?;
                backup_created = true;
            }
            let on_disk_name = if *disabled {
                format!("{}.disabled", base_name)
            } else {
                base_name.clone()
            };
            let src = mods_dir.join(&on_disk_name);
            let dst = backup_dir.join(&on_disk_name);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            rename(&src, &dst)?;
        }
    }

    println!("Downloading new and updated mods...");
    download_missing_parallel(
        client,
        &mods_dir,
        &manifest,
        |entry| {
            local_hashes
                .get(&entry.name)
                .map(|(h, _)| h == &entry.hash)
                .unwrap_or(false)
        },
        |entry| {
            // preserve disabled state across an update instead of silently re-enabling the mod
            let disabled = local_hashes.get(&entry.name).map(|(_, d)| *d).unwrap_or(false);
            if disabled {
                format!("{}.disabled", entry.name)
            } else {
                entry.name.clone()
            }
        },
        |entry| format!("{}/mods/{}.jar", CDN_URL, entry.hash),
    )?;

    Ok(())
}

fn sync_resourcepacks(
    client: &reqwest::blocking::Client,
    prefix: &str,
    inst_mc_dir: &str,
) -> anyhow::Result<()> {
    println!("Fetching resourcepack manifest...");
    let resourcepack_manifest_res = client
        .get(format!("{}/{}/resourcepacks-manifest.json", CDN_URL, prefix))
        .send()?;

    if !resourcepack_manifest_res.status().is_success() {
        println!("No resourcepack manifest published. Skipping resourcepack sync.");
        return Ok(());
    }

    let resourcepack_manifest: Vec<ResourcePackEntry> = resourcepack_manifest_res.json()?;

    let resourcepacks_dir = Path::new(inst_mc_dir).join("resourcepacks");
    fs::create_dir_all(&resourcepacks_dir)?;

    println!("Hashing local resourcepacks...");
    let local_resourcepack_hashes: HashMap<String, String> =
        hash_dir_parallel(&resourcepacks_dir, |rel| Some((rel, ())))?
            .into_iter()
            .map(|(k, (hash, ()))| (k, hash))
            .collect();

    // Some mods (e.g. Continuity) extract their own bundled resourcepacks straight into this
    // folder as real .zip files. Those never appear in the pack's manifest, so without this
    // guard the loop below would treat them as "removed" and back them up/strip them from
    // options.txt on every single sync. Only ever touch files this tool previously placed
    // here itself, tracked via a local state file — anything else (player's own packs,
    // mod-injected packs) is left alone regardless of whether it's in the current manifest.
    let resourcepack_state_path = Path::new(inst_mc_dir).join(".wolfpacker_resourcepacks.json");
    let previously_managed: Vec<String> = fs::read_to_string(&resourcepack_state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let managed_names: std::collections::HashSet<&str> = previously_managed
        .iter()
        .map(|s| s.as_str())
        .chain(resourcepack_manifest.iter().map(|p| p.name.as_str()))
        .collect();

    let resourcepack_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let resourcepack_backup_dir =
        Path::new(inst_mc_dir).join(format!("resourcepacks_backup_{}", resourcepack_timestamp));
    let mut resourcepack_backup_created = false;
    let mut removed_resourcepacks: Vec<String> = Vec::new();

    println!("Backing up outdated and removed resourcepacks...");
    for (name, local_hash) in &local_resourcepack_hashes {
        if !managed_names.contains(name.as_str()) {
            continue;
        }

        let up_to_date = resourcepack_manifest
            .iter()
            .any(|p| &p.name == name && &p.hash == local_hash);

        if !up_to_date {
            if !resourcepack_backup_created {
                fs::create_dir_all(&resourcepack_backup_dir)?;
                resourcepack_backup_created = true;
            }
            let src = resourcepacks_dir.join(name);
            let dst = resourcepack_backup_dir.join(name);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            rename(&src, &dst)?;
            removed_resourcepacks.push(name.clone());
        }
    }

    println!("Downloading new and updated resourcepacks...");
    download_missing_parallel(
        client,
        &resourcepacks_dir,
        &resourcepack_manifest,
        |entry| local_resourcepack_hashes.get(&entry.name) == Some(&entry.hash),
        |entry| entry.name.clone(),
        |entry| format!("{}/resourcepacks/{}.zip", CDN_URL, entry.hash),
    )?;
    // Every managed pack is force-added (not just newly downloaded ones) so a player-disabled
    // pack gets re-enabled on every sync, not only when its content happens to change.
    let added_resourcepacks: Vec<String> =
        resourcepack_manifest.iter().map(|e| e.name.clone()).collect();

    // Optional: the pack maintainer's declared load order for managed resourcepacks (from the
    // source instance's own options.txt). Absent/404 just means no order data was published —
    // skip reordering and leave whatever order the player already has.
    let order: Vec<String> = client
        .get(format!("{}/{}/resourcepacks-order.json", CDN_URL, prefix))
        .send()
        .ok()
        .filter(|res| res.status().is_success())
        .and_then(|res| res.json().ok())
        .unwrap_or_default();

    // Merge (not overwrite) the enable/disable change into options.txt's resourcePacks list
    // so a player's own entries and anything other mods register there survive untouched.
    wolfpacker::config_patch::merge_resource_pack_entries(
        &Path::new(inst_mc_dir).join("options.txt"),
        &added_resourcepacks,
        &removed_resourcepacks,
        &order,
    )?;

    let managed_state: Vec<&str> = resourcepack_manifest.iter().map(|p| p.name.as_str()).collect();
    fs::write(&resourcepack_state_path, serde_json::to_string(&managed_state)?)?;

    Ok(())
}

fn sync_ftbquests(
    client: &reqwest::blocking::Client,
    prefix: &str,
    inst_mc_dir: &str,
) -> anyhow::Result<()> {
    println!("Fetching FTB Quests manifest...");
    let ftbquests_manifest_res = client
        .get(format!("{}/{}/ftbquests-manifest.json", CDN_URL, prefix))
        .send()?;

    if !ftbquests_manifest_res.status().is_success() {
        println!("No FTB Quests manifest published. Skipping FTB Quests sync.");
        return Ok(());
    }

    let ftbquests_manifest: Vec<FtbQuestEntry> = ftbquests_manifest_res.json()?;

    let ftbquests_dir = Path::new(inst_mc_dir).join("config").join("ftbquests");
    fs::create_dir_all(&ftbquests_dir)?;

    println!("Hashing local FTB Quests files...");
    let local_ftbquests_hashes: HashMap<String, String> =
        hash_dir_parallel(&ftbquests_dir, |rel| Some((rel, ())))?
            .into_iter()
            .map(|(k, (hash, ()))| (k, hash))
            .collect();

    let ftbquests_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let ftbquests_backup_dir =
        Path::new(inst_mc_dir).join(format!("ftbquests_backup_{}", ftbquests_timestamp));
    let mut ftbquests_backup_created = false;

    println!("Backing up outdated and removed FTB Quests files...");
    for (name, local_hash) in &local_ftbquests_hashes {
        let up_to_date = ftbquests_manifest
            .iter()
            .any(|q| &q.name == name && &q.hash == local_hash);

        if !up_to_date {
            if !ftbquests_backup_created {
                fs::create_dir_all(&ftbquests_backup_dir)?;
                ftbquests_backup_created = true;
            }
            let src = ftbquests_dir.join(name);
            let dst = ftbquests_backup_dir.join(name);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            rename(&src, &dst)?;
        }
    }

    println!("Downloading new and updated FTB Quests files...");
    download_missing_parallel(
        client,
        &ftbquests_dir,
        &ftbquests_manifest,
        |entry| local_ftbquests_hashes.get(&entry.name) == Some(&entry.hash),
        |entry| entry.name.clone(),
        |entry| format!("{}/ftbquests/{}", CDN_URL, entry.hash),
    )?;

    Ok(())
}

fn sync_config_patch(
    client: &reqwest::blocking::Client,
    prefix: &str,
    inst_mc_dir: &str,
) -> anyhow::Result<()> {
    println!("Fetching config patch...");
    match client.get(format!("{}/{}/config-patch.json", CDN_URL, prefix)).send() {
        Ok(res) if res.status().is_success() => match res.json::<wolfpacker::config_patch::ConfigMap>() {
            Ok(patch) => {
                let current_values = wolfpacker::config_patch::read_matching(Path::new(inst_mc_dir), &patch);
                let effective_diff = wolfpacker::config_patch::diff(&patch, &current_values);

                if effective_diff.is_empty() {
                    println!("Config already up to date.");
                } else {
                    println!("Applying config patch...");
                    if let Err(e) = wolfpacker::config_patch::apply_all(Path::new(inst_mc_dir), &patch) {
                        println!("Warning: failed to apply config patch: {}", e);
                    }
                }
            }
            Err(e) => println!("Warning: malformed config patch, skipping: {}", e),
        },
        Ok(_) => println!("No config patch published. Skipping config sync."),
        Err(e) => println!("Warning: failed to fetch config patch: {}", e),
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let profile = parse_arg("--profile", "all");
    let modpack = parse_arg("--modpack", "wfp");
    let server = parse_flag("--server");
    let prefix = format!("{}/{}", modpack, if server { "server" } else { "client" });
    println!("Using profile: {}", profile);
    println!("Modpack: {} ({})", modpack, if server { "server" } else { "client" });

    // Outside PrismLauncher (e.g. a Pterodactyl server egg) INST_DIR/INST_MC_DIR aren't set;
    // fall back to the working directory, matching the flat volumes/<uuid> layout.
    let cwd = env::current_dir()?.to_string_lossy().to_string();
    let inst_dir = env::var("INST_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cwd.clone());
    let inst_mc_dir = env::var("INST_MC_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd);

    let version_path = Path::new(&inst_dir).join("version");
    let profile_path = Path::new(&inst_dir).join("profile");

    println!("Checking local version...");
    let local_version: u32 = match fs::read_to_string(&version_path) {
        Ok(v) => v.trim().parse().unwrap_or(0),
        Err(_) => 0,
    };
    println!("Local version: {}", local_version);

    let local_profile = fs::read_to_string(&profile_path).ok().map(|p| p.trim().to_string());
    println!("Local profile: {:?}", local_profile);

    println!("Checking remote version...");
    let client = reqwest::blocking::Client::builder().build()?;

    let res = client.get(format!("{}/{}/version", CDN_URL, prefix)).send()?;
    let remote_version_str = res.text()?;
    let remote_version: u32 = remote_version_str.trim().parse().unwrap_or(0);
    println!("Remote version: {}", remote_version);

    // A version match alone doesn't mean nothing to do — switching --profile (e.g. all ->
    // minimal) needs mods excluded from the new profile removed even if the remote manifest
    // itself hasn't changed. Only skip when both the version AND the profile used for the
    // last successful sync match.
    let profile_unchanged = local_profile.as_deref() == Some(profile.as_str());
    if local_version >= remote_version && local_version != 0 && profile_unchanged {
        println!("You are on the latest version and profile. Skipping update.");
        return Ok(());
    }

    println!("Checking NeoForge version...");
    let neoforge_res = client
        .get(format!("{}/{}/neoforge-version", CDN_URL, prefix))
        .send()?;

    if neoforge_res.status().is_success() {
        let remote_neoforge_version = neoforge_res.text()?.trim().to_string();

        if server {
            if check_neoforge_version_file(&inst_dir, &remote_neoforge_version)? {
                println!(
                    "NeoForge loader version changed to {} — reinstall it manually (e.g. via the Pterodactyl egg); wolfpacker does not install the server loader.",
                    remote_neoforge_version
                );
            } else {
                println!("NeoForge is already up to date.");
            }
        } else if update_neoforge_version(&inst_dir, &remote_neoforge_version)? {
            println!("Updated NeoForge to version {}", remote_neoforge_version);
        } else {
            println!("NeoForge is already up to date.");
        }
    } else {
        println!("No NeoForge version published. Skipping NeoForge check.");
    }

    if !server {
        println!("Checking launch settings (memory, JVM args)...");
        let launch_res = client.get(format!("{}/{}/launch-settings.json", CDN_URL, prefix)).send()?;
        if launch_res.status().is_success() {
            let remote_launch_settings: BTreeMap<String, String> = launch_res.json().unwrap_or_default();
            if update_launch_settings(&inst_dir, &remote_launch_settings)? {
                println!("Updated launch settings to {:?} (takes effect next launch)", remote_launch_settings);
            } else {
                println!("Launch settings are already up to date.");
            }
        } else {
            println!("No launch settings published. Skipping launch settings check.");
        }
    }

    // The four sync sections below touch disjoint directories/state files (mods/,
    // resourcepacks/ + options.txt, config/ftbquests/, config/*) and each does its own
    // network fetch, hashing and downloading — run them concurrently instead of serially.
    // Each section additionally parallelizes its own hashing/downloads across cores via rayon.
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let mods_handle = scope.spawn(|| sync_mods(&client, &prefix, &profile, &inst_mc_dir));
        let rp_handle = scope.spawn(|| sync_resourcepacks(&client, &prefix, &inst_mc_dir));
        let ftb_handle = scope.spawn(|| sync_ftbquests(&client, &prefix, &inst_mc_dir));
        let cfg_handle = scope.spawn(|| sync_config_patch(&client, &prefix, &inst_mc_dir));

        mods_handle.join().map_err(|_| anyhow::anyhow!("mod sync thread panicked"))??;
        rp_handle.join().map_err(|_| anyhow::anyhow!("resourcepack sync thread panicked"))??;
        ftb_handle.join().map_err(|_| anyhow::anyhow!("FTB Quests sync thread panicked"))??;
        cfg_handle.join().map_err(|_| anyhow::anyhow!("config patch sync thread panicked"))??;

        Ok(())
    })?;

    println!("Updating local version file...");
    fs::write(&version_path, remote_version.to_string())?;
    fs::write(&profile_path, &profile)?;

    println!("Successfully updated modpack to version {}!", remote_version);

    Ok(())
}
