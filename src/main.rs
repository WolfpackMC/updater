use md5::{Digest, Md5};
use std::collections::HashMap;
use std::{env, fs, io};
use std::fs::{rename, File};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const CDN_URL: &str = "https://wolfpack-cdn.kalkafox.dev";
const S3_URL: &str = "https://wolfpackmc.s3.us-east-1.amazonaws.com";

#[derive(serde::Deserialize)]
struct ModEntry {
    name: String,
    hash: String,
    profiles: Vec<String>,
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

fn main() -> anyhow::Result<()> {
    let profile = parse_arg("--profile", "all");
    let modpack = parse_arg("--modpack", "wfp");
    let server = parse_flag("--server");
    let prefix = format!("{}/{}", modpack, if server { "server" } else { "client" });
    println!("Using profile: {}", profile);
    println!("Modpack: {} ({})", modpack, if server { "server" } else { "client" });

    let inst_name = env::var("INST_NAME").unwrap_or_default();
    let inst_id = env::var("INST_ID").unwrap_or_default();
    let inst_java = env::var("INST_JAVA").unwrap_or_default();
    let inst_java_args = env::var("INST_JAVA_ARGS").unwrap_or_default();

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

    println!("Checking local version...");
    let local_version: u32 = match fs::read_to_string(&version_path) {
        Ok(v) => v.trim().parse().unwrap_or(0),
        Err(_) => 0,
    };
    println!("Local version: {}", local_version);

    println!("Checking remote version...");
    let client = reqwest::blocking::Client::builder().build()?;

    let res = client.get(format!("{}/{}/version", S3_URL, prefix)).send()?;
    let remote_version_str = res.text()?;
    let remote_version: u32 = remote_version_str.trim().parse().unwrap_or(0);
    println!("Remote version: {}", remote_version);

    if local_version >= remote_version && local_version != 0 {
        println!("You are on the latest version. Skipping update.");
        return Ok(());
    }

    println!("Checking NeoForge version...");
    let neoforge_res = client
        .get(format!("{}/{}/neoforge-version", S3_URL, prefix))
        .send()?;

    if neoforge_res.status().is_success() {
        let remote_neoforge_version = neoforge_res.text()?.trim().to_string();

        if server {
            if check_neoforge_version_file(&inst_dir, &remote_neoforge_version)? {
                println!(
                    "NeoForge loader version changed to {} — reinstall it manually (e.g. via the Pterodactyl egg); mcupdater does not install the server loader.",
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

    println!("Fetching mod manifest...");
    let full_manifest: Vec<ModEntry> = client
        .get(format!("{}/{}/manifest.json", S3_URL, prefix))
        .send()?
        .json()?;

    let manifest: Vec<ModEntry> = full_manifest
        .into_iter()
        .filter(|m| m.profiles.iter().any(|p| p == &profile))
        .collect();

    let mods_dir = Path::new(&inst_mc_dir).join("mods");
    fs::create_dir_all(&mods_dir)?;

    println!("Hashing local mods...");
    // keyed by base name (Prism's ".disabled" suffix stripped), so a disabled mod still
    // matches its manifest entry instead of being treated as missing/stale.
    let mut local_hashes: HashMap<String, (String, bool)> = HashMap::new();
    for entry in walkdir::WalkDir::new(&mods_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
    {
        let rel = entry
            .path()
            .strip_prefix(&mods_dir)?
            .to_string_lossy()
            .replace('\\', "/");
        let disabled = rel.ends_with(".disabled");
        let base = rel.strip_suffix(".disabled").unwrap_or(&rel).to_string();
        local_hashes.insert(base, (file_md5(entry.path())?, disabled));
    }

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let backup_dir = Path::new(&inst_mc_dir).join(format!("mods_backup_{}", timestamp));
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
    for entry in &manifest {
        let local = local_hashes.get(&entry.name);
        let up_to_date = local.map(|(h, _)| h == &entry.hash).unwrap_or(false);

        if up_to_date {
            continue;
        }

        // preserve disabled state across an update instead of silently re-enabling the mod
        let disabled = local.map(|(_, d)| *d).unwrap_or(false);
        let on_disk_name = if disabled {
            format!("{}.disabled", entry.name)
        } else {
            entry.name.clone()
        };

        println!("Downloading {}...", entry.name);
        let target_path = mods_dir.join(&on_disk_name);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut response = client
            .get(format!("{}/mods/{}.jar", S3_URL, entry.hash))
            .send()?;
        let mut out_file = File::create(&target_path)?;
        io::copy(&mut response, &mut out_file)?;
    }

    println!("Updating local version file...");
    fs::write(&version_path, remote_version.to_string())?;

    println!("Successfully updated modpack to version {}!", remote_version);

    Ok(())
}