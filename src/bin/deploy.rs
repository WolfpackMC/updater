use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use md5::{Digest, Md5};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const BUCKET: &str = "wolfpackmc";

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct ModEntry {
    name: String,
    hash: String,
    profiles: Vec<String>,
}

fn workdir() -> PathBuf {
    PathBuf::from("E:\\PrismLauncher\\instances\\server\\minecraft")
}

fn instance_dir() -> PathBuf {
    workdir().parent().unwrap().to_path_buf()
}

fn mmc_pack_path() -> PathBuf {
    instance_dir().join("mmc-pack.json")
}

fn profile_exclude_path() -> PathBuf {
    workdir().join("profile-exclude.json")
}

/// Substrings matched against mod filenames; a match excludes that mod from the "minimal" profile.
fn load_minimal_excludes() -> anyhow::Result<Vec<String>> {
    match fs::read_to_string(profile_exclude_path()) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(_) => Ok(Vec::new()),
    }
}

/* ---------------- MOD SCAN ---------------- */

fn collect_mods() -> Vec<(String, PathBuf)> {
    let mods_dir = workdir().join("mods");

    walkdir::WalkDir::new(&mods_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let rel_path = e.path().strip_prefix(&mods_dir).ok()?;
            let rel_str = rel_path.to_string_lossy().replace('\\', "/");

            if rel_str.contains(".connector") || rel_str.split('/').any(|p| p == "server") {
                return None;
            }

            Some((rel_str, e.into_path()))
        })
        .collect()
}

fn build_manifest(mods: &[(String, PathBuf)], minimal_excludes: &[String]) -> anyhow::Result<Vec<ModEntry>> {
    mods.par_iter()
        .map(|(name, path)| -> anyhow::Result<ModEntry> {
            let mut profiles = vec!["all".to_string()];
            if !minimal_excludes.iter().any(|ex| name.contains(ex.as_str())) {
                profiles.push("minimal".to_string());
            }

            Ok(ModEntry {
                name: name.clone(),
                hash: get_file_md5(path)?,
                profiles,
            })
        })
        .collect()
}

/* ---------------- HASH ---------------- */

fn get_file_md5(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
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

/* ---------------- S3 HELPERS ---------------- */

async fn get_object_text(client: &Client, key: &str) -> anyhow::Result<Option<String>> {
    match client
        .get_object()
        .bucket(BUCKET)
        .key(key)
        .send()
        .await
    {
        Ok(res) => {
            let bytes = res.body.collect().await?.into_bytes();
            Ok(Some(String::from_utf8(bytes.to_vec())?.trim().to_string()))
        }
        Err(SdkError::ServiceError(ctx)) if ctx.raw().status().as_u16() == 404 => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn put_object_text(client: &Client, key: &str, body: String, content_type: &str) -> anyhow::Result<()> {
    client
        .put_object()
        .bucket(BUCKET)
        .key(key)
        .body(ByteStream::from(body.into_bytes()))
        .acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
        .content_type(content_type)
        .send()
        .await?;
    Ok(())
}

async fn get_version(client: &Client) -> anyhow::Result<u32> {
    Ok(get_object_text(client, "version")
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

async fn get_remote_manifest(client: &Client) -> anyhow::Result<Option<Vec<ModEntry>>> {
    match get_object_text(client, "manifest.json").await? {
        // an old-schema or malformed manifest is treated as absent, forcing a full republish
        Some(text) => Ok(serde_json::from_str(&text).ok()),
        None => Ok(None),
    }
}

async fn upload_manifest(client: &Client, manifest: &[ModEntry]) -> anyhow::Result<()> {
    let body = serde_json::to_string(manifest)?;
    put_object_text(client, "manifest.json", body, "application/json").await
}

async fn mod_blob_exists(client: &Client, hash: &str) -> anyhow::Result<bool> {
    match client
        .head_object()
        .bucket(BUCKET)
        .key(format!("mods/{}.jar", hash))
        .send()
        .await
    {
        Ok(_) => Ok(true),
        Err(SdkError::ServiceError(ctx)) if ctx.raw().status().as_u16() == 404 => Ok(false),
        Err(err) => Err(err.into()),
    }
}

async fn upload_mod_blob(client: &Client, hash: &str, path: &Path) -> anyhow::Result<()> {
    let bytes = fs::read(path)?;
    client
        .put_object()
        .bucket(BUCKET)
        .key(format!("mods/{}.jar", hash))
        .body(ByteStream::from(bytes))
        .acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
        .content_type("application/java-archive")
        .send()
        .await?;
    Ok(())
}

async fn upload_version(client: &Client, version: u32) -> anyhow::Result<()> {
    put_object_text(client, "version", version.to_string(), "text/plain").await
}

/* ---------------- NEOFORGE ---------------- */

fn get_local_neoforge_version() -> anyhow::Result<String> {
    let contents = fs::read_to_string(mmc_pack_path())?;
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

async fn get_remote_neoforge_version(client: &Client) -> anyhow::Result<Option<String>> {
    get_object_text(client, "neoforge-version").await
}

async fn upload_neoforge_version(client: &Client, version: &str) -> anyhow::Result<()> {
    put_object_text(client, "neoforge-version", version.to_string(), "text/plain").await
}

/* ---------------- MAIN ---------------- */

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let credentials = Credentials::new(
        env::var("AWS_ACCESS_ID").unwrap_or_default(),
        env::var("AWS_ACCESS_SECRET").unwrap_or_default(),
        None,
        None,
        "static",
    );

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(credentials)
        .build();

    let client = Client::from_conf(config);

    println!("Scanning mods...");
    let mods = collect_mods();
    let minimal_excludes = load_minimal_excludes()?;

    println!("Hashing mods...");
    let mut manifest = build_manifest(&mods, &minimal_excludes)?;
    manifest.sort_by(|a, b| a.name.cmp(&b.name));

    println!("Fetching remote manifest...");
    let mut remote_manifest = get_remote_manifest(&client).await?.unwrap_or_default();
    remote_manifest.sort_by(|a, b| a.name.cmp(&b.name));

    let mods_changed = manifest != remote_manifest;

    println!("Checking NeoForge version...");
    let local_neoforge_version = get_local_neoforge_version()?;
    let remote_neoforge_version = get_remote_neoforge_version(&client).await?;

    println!("Local NeoForge: {}", local_neoforge_version);
    println!("Remote NeoForge: {:?}", remote_neoforge_version);

    let neoforge_changed = Some(local_neoforge_version.clone()) != remote_neoforge_version;

    if !mods_changed && !neoforge_changed {
        println!("No changes. Skipping upload.");
        return Ok(());
    }

    println!("Changes detected. Uploading...");

    if mods_changed {
        for (name, path) in &mods {
            let entry = manifest.iter().find(|m| &m.name == name).unwrap();
            if !mod_blob_exists(&client, &entry.hash).await? {
                println!("Uploading {}", name);
                upload_mod_blob(&client, &entry.hash, path).await?;
            }
        }
        upload_manifest(&client, &manifest).await?;
    }

    if neoforge_changed {
        upload_neoforge_version(&client, &local_neoforge_version).await?;
    }

    let current_version = get_version(&client).await?;
    let new_version = current_version + 1;

    upload_version(&client, new_version).await?;

    println!("Updated to version {}", new_version);

    Ok(())
}
