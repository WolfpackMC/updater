use std::{env, fs, io};
use std::fs::{rename, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const CDN_URL: &str = "https://wolfpack-cdn.kalkafox.dev";
const S3_URL: &str = "https://wolfpackmc.s3.us-east-1.amazonaws.com";

const PROGRESS_INTERVAL: u64 = 10 * 1024 * 1024;

fn move_dir_fallback(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());

        if path.is_dir() {
            move_dir_fallback(&path, &target)?;
        } else {
            fs::copy(&path, &target)?;
        }
    }

    fs::remove_dir_all(src)?;
    Ok(())
}

fn update_neoforge_version(inst_dir: &str, remote_version: &str) -> anyhow::Result<bool> {
    let mmc_pack_path = Path::new(inst_dir).join("mmc-pack.json");
    let contents = fs::read_to_string(&mmc_pack_path)?;
    let mut mmc_pack: serde_json::Value = serde_json::from_str(&contents)?;

    let components = mmc_pack
        .get_mut("components")
        .and_then(|c| c.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("mmc-pack.json missing components array"))?;

    let neoforge = components
        .iter_mut()
        .find(|c| c.get("uid").and_then(|u| u.as_str()) == Some("net.neoforged"))
        .ok_or_else(|| anyhow::anyhow!("NeoForge component not found in mmc-pack.json"))?;

    let local_version = neoforge.get("version").and_then(|v| v.as_str()).unwrap_or("");

    if local_version == remote_version {
        return Ok(false);
    }

    neoforge["version"] = serde_json::Value::String(remote_version.to_string());
    neoforge["cachedVersion"] = serde_json::Value::String(remote_version.to_string());

    fs::write(&mmc_pack_path, serde_json::to_string_pretty(&mmc_pack)?)?;

    Ok(true)
}

fn files_are_identical(path1: &Path, path2: &Path) -> io::Result<bool> {
    let meta1 = fs::metadata(path1)?;
    let meta2 = fs::metadata(path2)?;

    if meta1.len() != meta2.len() {
        return Ok(false);
    }

    let mut f1 = File::open(path1)?;
    let mut f2 = File::open(path2)?;

    let mut buffer1 = [0; 8192];
    let mut buffer2 = [0; 8192];

    loop {
        let n1 = f1.read(&mut buffer1)?;
        let n2 = f2.read(&mut buffer2)?;

        if n1 != n2 || buffer1[..n1] != buffer2[..n2] {
            return Ok(false);
        }
        if n1 == 0 {
            break;
        }
    }

    Ok(true)
}

fn main() -> anyhow::Result<()> {
    let inst_name = env::var("INST_NAME").unwrap_or_default();
    let inst_id = env::var("INST_ID").unwrap_or_default();
    let inst_dir = env::var("INST_DIR").unwrap_or_default();
    let inst_mc_dir = env::var("INST_MC_DIR").unwrap_or_default();
    let inst_java = env::var("INST_JAVA").unwrap_or_default();
    let inst_java_args = env::var("INST_JAVA_ARGS").unwrap_or_default();

    let version_path = Path::new(&inst_dir).join("version");

    println!("Checking local version...");
    let local_version: u32 = match fs::read_to_string(&version_path) {
        Ok(v) => v.trim().parse().unwrap_or(0),
        Err(_) => 0,
    };
    println!("Local version: {}", local_version);

    println!("Checking remote version...");
    let client = reqwest::blocking::Client::builder().build()?;

    let res = client.get(format!("{}/version", S3_URL)).send()?;
    let remote_version_str = res.text()?;
    let remote_version: u32 = remote_version_str.trim().parse().unwrap_or(0);
    println!("Remote version: {}", remote_version);

    if local_version >= remote_version && local_version != 0 {
        println!("You are on the latest version. Skipping update.");
        return Ok(());
    }

    println!("Checking NeoForge version...");
    let neoforge_res = client.get(format!("{}/neoforge-version", S3_URL)).send()?;

    if neoforge_res.status().is_success() {
        let remote_neoforge_version = neoforge_res.text()?.trim().to_string();

        if update_neoforge_version(&inst_dir, &remote_neoforge_version)? {
            println!("Updated NeoForge to version {}", remote_neoforge_version);
        } else {
            println!("NeoForge is already up to date.");
        }
    } else {
        println!("No NeoForge version published. Skipping NeoForge check.");
    }

    let zip_path = Path::new(&inst_dir).join("WFP.zip");
    println!("Starting download of WFP.zip...");

    let mut response = client.get(format!("{}/WFP.zip", S3_URL)).send()?;

    let total_size = response.content_length().unwrap_or(0);

    let mut out_file = File::create(&zip_path)?;

    let mut downloaded: u64 = 0;
    let mut last_reported: u64 = 0;
    let mut buffer = [0; 8192];

    let start_time = Instant::now();
    let mut last_time = start_time;
    let mut last_bytes = 0u64;

    loop {
        let bytes_read = response.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        out_file.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;

        let now = Instant::now();
        let elapsed = now.duration_since(last_time).as_secs_f64();

        if downloaded - last_reported >= PROGRESS_INTERVAL || downloaded == total_size {
            let percentage = if total_size > 0 {
                (downloaded as f64 / total_size as f64) * 100.0
            } else {
                0.0
            };

            let bytes_since_last = downloaded - last_bytes;
            let speed = bytes_since_last as f64 / elapsed; // bytes/sec

            println!(
                "Progress: {:.2} MB / {:.2} MB ({:.1}%) | Speed: {:.2} MB/s",
                downloaded as f64 / 1_048_576.0,
                total_size as f64 / 1_048_576.0,
                percentage,
                speed / 1_048_576.0
            );

            last_reported = downloaded;
            last_bytes = downloaded;
            last_time = now;
        }
    }

    println!("Download complete!");

    // Setup a staging directory to extract the remote zip into
    let mods_staging = Path::new(&inst_mc_dir).join(".mods_staging");
    if mods_staging.exists() {
        fs::remove_dir_all(&mods_staging)?;
    }
    fs::create_dir_all(&mods_staging)?;

    println!("Extracting jar files into staging folder...");
    let zip_file = File::open(&zip_path)?;
    let mut archive = zip::ZipArchive::new(zip_file)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;

        let outpath = match file.enclosed_name() {
            Some(path) => path.to_owned(),
            None => continue,
        };

        let extract_path = mods_staging.join(&outpath);

        if (*file.name()).ends_with('/') {
            fs::create_dir_all(&extract_path)?;
        } else {
            if let Some(p) = extract_path.parent() {
                if !p.exists() {
                    fs::create_dir_all(p)?;
                }
            }
            let mut outfile = File::create(&extract_path)?;
            io::copy(&mut file, &mut outfile)?;
        }
    }

    let mods_dir = Path::new(&inst_mc_dir).join("mods");
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let backup_dir = Path::new(&inst_mc_dir).join(format!("mods_backup_{}", timestamp));
    let mut backup_created = false;

    println!("Comparing existing mods with the remote update...");

    if mods_dir.exists() {
        if let Ok(entries) = fs::read_dir(&mods_dir) {
            for entry in entries.flatten() {
                let current_mod_path = entry.path();

                if current_mod_path.is_file() {
                    let file_name = current_mod_path.file_name().unwrap();
                    let staging_mod_path = mods_staging.join(file_name);

                    let mut should_backup = true;

                    if staging_mod_path.exists() {
                        if let Ok(true) = files_are_identical(&current_mod_path, &staging_mod_path) {
                            should_backup = false;
                        }
                    }

                    if should_backup {
                        if !backup_created {
                            fs::create_dir_all(&backup_dir)?;
                            backup_created = true;
                        }
                        let backup_file_path = backup_dir.join(file_name);
                        rename(&current_mod_path, &backup_file_path)?;
                    } else {
                        let _ = fs::remove_file(&staging_mod_path);
                    }
                }
            }
        }
    } else {
        fs::create_dir_all(&mods_dir)?;
    }

    println!("Applying new mods...");
    if let Ok(entries) = fs::read_dir(&mods_staging) {
        for entry in entries.flatten() {
            let staging_path = entry.path();
            let target_path = mods_dir.join(entry.file_name());

            if staging_path.is_dir() {
                move_dir_fallback(&staging_path, &target_path)?;
            } else {
                rename(&staging_path, &target_path)?;
            }
        }
    }

    println!("Cleaning up temporary files...");
    fs::remove_dir_all(&mods_staging)?;
    fs::remove_file(&zip_path)?;

    println!("Updating local version file...");
    fs::write(&version_path, remote_version.to_string())?;

    println!("Successfully updated modpack to version {}!", remote_version);

    Ok(())
}