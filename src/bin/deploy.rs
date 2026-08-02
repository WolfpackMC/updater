use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use mcupdater::config_patch::{self, ConfigMap};
use md5::{Digest, Md5};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::Sha512;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const BUCKET: &str = "wolfpackmc";
const CDN_URL: &str = "https://wolfpack-cdn.kalkafox.dev";
/// Mutable keys (version pointers, manifests, the mrpack) get a short TTL so CloudFront
/// doesn't serve stale state for long after a publish. Content-addressed keys
/// (mods/{hash}.jar) are cached far longer since their content never changes once uploaded.
const MUTABLE_CACHE_CONTROL: &str = "public, max-age=60";
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct ModEntry {
    name: String,
    hash: String,
    profiles: Vec<String>,
}

struct ModpackPaths {
    workdir: PathBuf,
    instance_dir: PathBuf,
}

/// Client and, optionally, dedicated server mod source dirs for a modpack id.
/// Add new modpacks here as they're onboarded.
fn modpack_workdir(modpack: &str, server: bool) -> anyhow::Result<PathBuf> {
    match (modpack, server) {
        ("wfp", false) => Ok(PathBuf::from(r"E:\PrismLauncher\instances\server\minecraft")),
        ("wfp", true) => Ok(PathBuf::from(r"E:\PrismLauncher\instances\wfpserver\minecraft")),
        _ => anyhow::bail!(
            "Unknown modpack/variant combo: modpack={}, server={}",
            modpack,
            server
        ),
    }
}

fn modpack_paths(modpack: &str, server: bool) -> anyhow::Result<ModpackPaths> {
    let workdir = modpack_workdir(modpack, server)?;
    let instance_dir = workdir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workdir {} has no parent", workdir.display()))?
        .to_path_buf();
    Ok(ModpackPaths {
        workdir,
        instance_dir,
    })
}

fn mmc_pack_path(paths: &ModpackPaths) -> PathBuf {
    paths.instance_dir.join("mmc-pack.json")
}

fn profile_exclude_path(paths: &ModpackPaths) -> PathBuf {
    paths.workdir.join("profile-exclude.json")
}

/// Local (maintainer-machine-only) snapshot of every pack-managed config key's value as of the
/// last deploy, used to detect which keys changed this run. Not uploaded anywhere — the
/// cumulative result of every such diff is what gets published as `config-patch.json`.
fn config_baseline_path(modpack: &str, server: bool) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("config-baseline")
        .join(format!("{}-{}.json", modpack, if server { "server" } else { "client" }))
}

/// Substrings matched against mod filenames; a match excludes that mod from the "minimal" profile.
fn load_minimal_excludes(paths: &ModpackPaths) -> anyhow::Result<Vec<String>> {
    match fs::read_to_string(profile_exclude_path(paths)) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(_) => Ok(Vec::new()),
    }
}

/* ---------------- MOD SCAN ---------------- */

fn collect_mods(paths: &ModpackPaths, server: bool) -> Vec<(String, PathBuf)> {
    let mods_dir = paths.workdir.join("mods");

    walkdir::WalkDir::new(&mods_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let rel_path = e.path().strip_prefix(&mods_dir).ok()?;
            let rel_str = rel_path.to_string_lossy().replace('\\', "/");

            if rel_str.contains(".connector")
                || rel_str.split('/').any(|p| p == "server")
                || rel_str.ends_with(".zip")
                || (server && rel_str.contains(".client"))
            {
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

fn get_file_sha1_sha512(path: &Path) -> anyhow::Result<(String, String)> {
    let bytes = fs::read(path)?;

    let mut sha1_hasher = Sha1::new();
    sha1_hasher.update(&bytes);
    let sha1 = format!("{:x}", sha1_hasher.finalize());

    let mut sha512_hasher = Sha512::new();
    sha512_hasher.update(&bytes);
    let sha512 = format!("{:x}", sha512_hasher.finalize());

    Ok((sha1, sha512))
}

/* ---------------- MRPACK ---------------- */

#[derive(Serialize)]
struct MrpackHashes {
    sha1: String,
    sha512: String,
}

#[derive(Serialize)]
struct MrpackEnv {
    client: String,
    server: String,
}

#[derive(Serialize)]
struct MrpackFile {
    path: String,
    hashes: MrpackHashes,
    env: MrpackEnv,
    downloads: Vec<String>,
    #[serde(rename = "fileSize")]
    file_size: u64,
}

#[derive(Serialize)]
struct MrpackDependencies {
    minecraft: String,
    neoforge: String,
}

#[derive(Serialize)]
struct MrpackIndex {
    #[serde(rename = "formatVersion")]
    format_version: u32,
    game: String,
    #[serde(rename = "versionId")]
    version_id: String,
    name: String,
    summary: String,
    files: Vec<MrpackFile>,
    dependencies: MrpackDependencies,
}

#[derive(Serialize, Deserialize)]
struct PackEntry {
    id: String,
    name: String,
    description: String,
    #[serde(rename = "mrpackUrl")]
    mrpack_url: String,
}

#[derive(Serialize, Deserialize, Default)]
struct PacksManifest {
    packs: Vec<PackEntry>,
}

/// Top-level entries under the instance's minecraft dir that are safe to redistribute.
/// Everything else (saves, screenshots, logs, per-player caches, stray zips, mod-specific
/// data dirs, etc.) is drift that shouldn't ship to other players. Deliberately an allowlist,
/// not a denylist: new junk should have to be explicitly opted in, not explicitly excluded.
const ALLOWED_OVERRIDE_TOP_LEVEL: &[&str] = &["config", "icon.png", "options.txt", "servers.dat"];

fn collect_overrides(paths: &ModpackPaths, server: bool) -> Vec<(String, PathBuf)> {
    walkdir::WalkDir::new(&paths.workdir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let rel_path = e.path().strip_prefix(&paths.workdir).ok()?;
            let rel_str = rel_path.to_string_lossy().replace('\\', "/");
            let top = rel_str.split('/').next().unwrap_or(&rel_str);

            if !ALLOWED_OVERRIDE_TOP_LEVEL.contains(&top)
                || rel_str.contains(".connector")
                || rel_str.split('/').any(|p| p == "server")
                || (server && rel_str.contains(".client"))
            {
                return None;
            }

            Some((rel_str, e.into_path()))
        })
        .collect()
}

fn get_local_minecraft_version(paths: &ModpackPaths) -> anyhow::Result<String> {
    let contents = fs::read_to_string(mmc_pack_path(paths))?;
    let mmc_pack: serde_json::Value = serde_json::from_str(&contents)?;

    let version = mmc_pack
        .get("components")
        .and_then(|c| c.as_array())
        .and_then(|components| {
            components
                .iter()
                .find(|c| c.get("uid").and_then(|u| u.as_str()) == Some("net.minecraft"))
        })
        .and_then(|c| c.get("version"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Minecraft component not found in mmc-pack.json"))?;

    Ok(version.to_string())
}

/// Self-hosted equivalent of a Modrinth mrpack export: every mod is listed in `files[]`
/// pointing at the same content-addressed S3 blobs `upload_mod_blob` already publishes.
/// Unlike a Modrinth-CDN export, nothing needs to fall back to raw-embedding in
/// `overrides/mods/` — our own bucket resolves every mod, so the pack stays lean (a few MB
/// of config, not hundreds of MB of embedded jars).
fn build_mrpack(
    mods: &[(String, PathBuf)],
    manifest: &[ModEntry],
    overrides: &[(String, PathBuf)],
    minecraft_version: &str,
    neoforge_version: &str,
    version: u32,
    out_path: &Path,
) -> anyhow::Result<()> {
    let mut files = Vec::with_capacity(mods.len());
    for (name, path) in mods {
        let entry = manifest
            .iter()
            .find(|m| &m.name == name)
            .ok_or_else(|| anyhow::anyhow!("mod {} missing from manifest", name))?;
        let (sha1, sha512) = get_file_sha1_sha512(path)?;
        let file_size = fs::metadata(path)?.len();
        files.push(MrpackFile {
            path: format!("mods/{}", name),
            hashes: MrpackHashes { sha1, sha512 },
            env: MrpackEnv { client: "required".to_string(), server: "required".to_string() },
            downloads: vec![format!("{}/mods/{}.jar", CDN_URL, entry.hash)],
            file_size,
        });
    }

    let index = MrpackIndex {
        format_version: 1,
        game: "minecraft".to_string(),
        version_id: version.to_string(),
        name: "Wolfpack".to_string(),
        summary: "The Wolfpack NeoForge modpack.".to_string(),
        files,
        dependencies: MrpackDependencies {
            minecraft: minecraft_version.to_string(),
            neoforge: neoforge_version.to_string(),
        },
    };

    let file = fs::File::create(out_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("modrinth.index.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&index)?.as_bytes())?;

    for (rel_path, path) in overrides {
        zip.start_file(format!("overrides/{}", rel_path), options)?;
        zip.write_all(&fs::read(path)?)?;
    }

    zip.finish()?;
    Ok(())
}

async fn upload_mrpack(client: &Client, modpack: &str, path: &Path) -> anyhow::Result<String> {
    let key = format!("{}/{}.mrpack", modpack, modpack.to_uppercase());
    let bytes = fs::read(path)?;
    client
        .put_object()
        .bucket(BUCKET)
        .key(&key)
        .body(ByteStream::from(bytes))
        .acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
        .content_type("application/x-modrinth-modpack+zip")
        .cache_control(MUTABLE_CACHE_CONTROL)
        .send()
        .await?;
    Ok(format!("{}/{}", CDN_URL, key))
}

async fn update_packs_manifest(client: &Client, modpack: &str, mrpack_url: &str) -> anyhow::Result<()> {
    let mut manifest: PacksManifest = match get_object_text(client, "packs.json").await? {
        Some(text) => serde_json::from_str(&text).unwrap_or_default(),
        None => PacksManifest::default(),
    };

    if let Some(existing) = manifest.packs.iter_mut().find(|p| p.id == modpack) {
        existing.mrpack_url = mrpack_url.to_string();
    } else {
        manifest.packs.push(PackEntry {
            id: modpack.to_string(),
            name: modpack.to_uppercase(),
            description: format!("The {} modpack.", modpack.to_uppercase()),
            mrpack_url: mrpack_url.to_string(),
        });
    }

    let body = serde_json::to_string_pretty(&manifest)?;
    put_object_text(client, "packs.json", body, "application/json").await
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
        .cache_control(MUTABLE_CACHE_CONTROL)
        .send()
        .await?;
    Ok(())
}

async fn get_version(client: &Client, prefix: &str) -> anyhow::Result<u32> {
    Ok(get_object_text(client, &format!("{}/version", prefix))
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

async fn get_remote_manifest(client: &Client, prefix: &str) -> anyhow::Result<Option<Vec<ModEntry>>> {
    match get_object_text(client, &format!("{}/manifest.json", prefix)).await? {
        // an old-schema or malformed manifest is treated as absent, forcing a full republish
        Some(text) => Ok(serde_json::from_str(&text).ok()),
        None => Ok(None),
    }
}

async fn upload_manifest(client: &Client, prefix: &str, manifest: &[ModEntry]) -> anyhow::Result<()> {
    let body = serde_json::to_string(manifest)?;
    put_object_text(client, &format!("{}/manifest.json", prefix), body, "application/json").await
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
        .cache_control(IMMUTABLE_CACHE_CONTROL)
        .send()
        .await?;
    Ok(())
}

async fn upload_version(client: &Client, prefix: &str, version: u32) -> anyhow::Result<()> {
    put_object_text(client, &format!("{}/version", prefix), version.to_string(), "text/plain").await
}

/* ---------------- NEOFORGE ---------------- */

fn get_local_neoforge_version(paths: &ModpackPaths) -> anyhow::Result<String> {
    let contents = fs::read_to_string(mmc_pack_path(paths))?;
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

async fn get_remote_neoforge_version(client: &Client, prefix: &str) -> anyhow::Result<Option<String>> {
    get_object_text(client, &format!("{}/neoforge-version", prefix)).await
}

async fn upload_neoforge_version(client: &Client, prefix: &str, version: &str) -> anyhow::Result<()> {
    put_object_text(client, &format!("{}/neoforge-version", prefix), version.to_string(), "text/plain").await
}

/* ---------------- ARGS ---------------- */

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

/* ---------------- MAIN ---------------- */

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let modpack = parse_arg("--modpack", "wfp");
    let server = parse_flag("--server");
    let force = parse_flag("--force");
    let paths = modpack_paths(&modpack, server)?;
    let prefix = format!("{}/{}", modpack, if server { "server" } else { "client" });
    println!("Modpack: {} ({})", modpack, if server { "server" } else { "client" });

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
    let mods = collect_mods(&paths, server);
    let minimal_excludes = load_minimal_excludes(&paths)?;

    println!("Hashing mods...");
    let mut manifest = build_manifest(&mods, &minimal_excludes)?;
    manifest.sort_by(|a, b| a.name.cmp(&b.name));

    println!("Fetching remote manifest...");
    let mut remote_manifest = get_remote_manifest(&client, &prefix).await?.unwrap_or_default();
    remote_manifest.sort_by(|a, b| a.name.cmp(&b.name));

    let mods_changed = manifest != remote_manifest || force;

    println!("Checking NeoForge version...");
    let local_neoforge_version = get_local_neoforge_version(&paths)?;
    let remote_neoforge_version = get_remote_neoforge_version(&client, &prefix).await?;

    println!("Local NeoForge: {}", local_neoforge_version);
    println!("Remote NeoForge: {:?}", remote_neoforge_version);

    let neoforge_changed = Some(local_neoforge_version.clone()) != remote_neoforge_version || force;

    println!("Scanning config...");
    let overrides = collect_overrides(&paths, server);
    let config_files: Vec<(String, PathBuf)> = overrides
        .iter()
        .filter(|(rel, _)| rel.starts_with("config/"))
        .filter(|(rel, _)| {
            matches!(
                Path::new(rel).extension().and_then(|e| e.to_str()),
                Some("toml") | Some("json") | Some("properties")
            )
        })
        .cloned()
        .collect();
    let current_config = config_patch::extract_all(&config_files);

    let baseline_path = config_baseline_path(&modpack, server);
    let baseline_config: ConfigMap = fs::read_to_string(&baseline_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let config_delta = config_patch::diff(&current_config, &baseline_config);
    let config_changed = !config_delta.is_empty() || force;

    if !mods_changed && !neoforge_changed && !config_changed {
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
        upload_manifest(&client, &prefix, &manifest).await?;
    }

    if neoforge_changed {
        upload_neoforge_version(&client, &prefix, &local_neoforge_version).await?;
    }

    if config_changed {
        println!("Updating config patch...");
        let mut remote_patch: ConfigMap = get_object_text(&client, &format!("{}/config-patch.json", prefix))
            .await?
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        config_patch::merge(&mut remote_patch, &config_delta);

        let body = serde_json::to_string(&remote_patch)?;
        put_object_text(&client, &format!("{}/config-patch.json", prefix), body, "application/json").await?;

        if let Some(parent) = baseline_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&baseline_path, serde_json::to_string(&current_config)?)?;
    }

    let current_version = get_version(&client, &prefix).await?;
    let new_version = current_version + 1;

    upload_version(&client, &prefix, new_version).await?;

    println!("Updated to version {}", new_version);

    // The .mrpack is only meaningful as a from-scratch client install; server publishes
    // don't have a corresponding "new instance" flow to feed.
    if !server {
        println!("Building mrpack...");
        let local_minecraft_version = get_local_minecraft_version(&paths)?;
        let mrpack_path = env::temp_dir().join(format!("{}.mrpack", modpack));

        build_mrpack(
            &mods,
            &manifest,
            &overrides,
            &local_minecraft_version,
            &local_neoforge_version,
            new_version,
            &mrpack_path,
        )?;

        println!("Uploading mrpack...");
        let mrpack_url = upload_mrpack(&client, &modpack, &mrpack_path).await?;
        fs::remove_file(&mrpack_path).ok();

        println!("Updating packs.json...");
        update_packs_manifest(&client, &modpack, &mrpack_url).await?;

        println!("mrpack published: {}", mrpack_url);
    }

    Ok(())
}
