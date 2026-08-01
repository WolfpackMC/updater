use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use md5::{Digest, Md5};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

const BUCKET: &str = "wolfpackmc";

fn workdir() -> PathBuf {
    PathBuf::from("E:\\PrismLauncher\\instances\\server\\minecraft")
}

fn instance_dir() -> PathBuf {
    workdir().parent().unwrap().to_path_buf()
}

fn output_zip_path() -> PathBuf {
    workdir().join("WFP.zip")
}

fn mmc_pack_path() -> PathBuf {
    instance_dir().join("mmc-pack.json")
}

/* ---------------- ZIP ---------------- */

fn zip_mods() -> anyhow::Result<()> {
    let mods_dir = workdir().join("mods");
    let zip_file = fs::File::create(output_zip_path())?;
    let mut writer = zip::ZipWriter::new(zip_file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for entry in walkdir::WalkDir::new(&mods_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel_path = path.strip_prefix(&mods_dir)?;

        if rel_path.as_os_str().is_empty() {
            continue;
        }

        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        if rel_str.contains(".connector") || rel_str.split('/').any(|p| p == "server") {
            continue;
        }

        if path.is_dir() {
            writer.add_directory(format!("{}/", rel_str), options)?;
        } else {
            writer.start_file(rel_str, options)?;
            let mut file = fs::File::open(path)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            std::io::Write::write_all(&mut writer, &buffer)?;
        }
    }

    writer.finish()?;
    Ok(())
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

async fn get_remote_hash(client: &Client) -> anyhow::Result<Option<String>> {
    get_object_text(client, "WFP.hash").await
}

async fn get_version(client: &Client) -> anyhow::Result<u32> {
    Ok(get_object_text(client, "version")
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

async fn upload_zip(client: &Client) -> anyhow::Result<()> {
    let bytes = fs::read(output_zip_path())?;
    client
        .put_object()
        .bucket(BUCKET)
        .key("WFP.zip")
        .body(ByteStream::from(bytes))
        .acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
        .content_type("application/zip")
        .send()
        .await?;
    Ok(())
}

async fn upload_hash(client: &Client, hash: &str) -> anyhow::Result<()> {
    put_object_text(client, "WFP.hash", hash.to_string(), "text/plain").await
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

    println!("Zipping...");
    zip_mods()?;

    println!("Hashing...");
    let local_hash = get_file_md5(&output_zip_path())?;

    println!("Fetching remote hash...");
    let remote_hash = get_remote_hash(&client).await?;

    println!("Local: {}", local_hash);
    println!("Remote: {:?}", remote_hash);

    println!("Checking NeoForge version...");
    let local_neoforge_version = get_local_neoforge_version()?;
    let remote_neoforge_version = get_remote_neoforge_version(&client).await?;

    println!("Local NeoForge: {}", local_neoforge_version);
    println!("Remote NeoForge: {:?}", remote_neoforge_version);

    let mods_changed = Some(local_hash.clone()) != remote_hash;
    let neoforge_changed = Some(local_neoforge_version.clone()) != remote_neoforge_version;

    if !mods_changed && !neoforge_changed {
        println!("No changes. Skipping upload.");
        return Ok(());
    }

    println!("Changes detected. Uploading...");

    if mods_changed {
        upload_zip(&client).await?;
        upload_hash(&client, &local_hash).await?;
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
