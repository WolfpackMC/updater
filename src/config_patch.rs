//! Shared key=val config patching, used by `deploy` (extract + diff pack-managed keys) and
//! `mcupdater` (apply them on top of a player's local config, overwriting only those keys).
//! Player-added keys and files not touched by the pack are never read or written.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(untagged)]
pub enum ConfigValue {
    String(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
}

impl fmt::Display for ConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigValue::String(s) => write!(f, "{}", s),
            ConfigValue::Integer(i) => write!(f, "{}", i),
            ConfigValue::Float(v) => write!(f, "{}", v),
            ConfigValue::Bool(b) => write!(f, "{}", b),
        }
    }
}

/// relative file path (posix-style, e.g. "config/mymod-common.toml") -> dotted key path -> value
pub type ConfigMap = BTreeMap<String, BTreeMap<String, ConfigValue>>;

/// Only scalar leaf values (string/int/float/bool) are tracked; arrays and tables-as-values
/// are skipped since there's no unambiguous single "key=val" to diff/patch for them.
pub fn extract_all(files: &[(String, PathBuf)]) -> ConfigMap {
    let mut map = ConfigMap::new();

    for (rel_path, path) in files {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let keys = match file_kind(rel_path) {
            Some(FileKind::Toml) => extract_toml(&text),
            Some(FileKind::Json) => extract_json(&text),
            Some(FileKind::Properties) => extract_properties(&text),
            Some(FileKind::Options) => extract_options(&text),
            None => continue,
        };

        if !keys.is_empty() {
            map.insert(rel_path.clone(), keys);
        }
    }

    map
}

enum FileKind {
    Toml,
    Json,
    Properties,
    Options,
}

/// Classifies a pack-managed config path by how it should be parsed/patched. `options.txt`
/// (Minecraft's own `key:value` file, sitting at the instance root rather than under `config/`)
/// is matched by name, not extension.
fn file_kind(rel_path: &str) -> Option<FileKind> {
    if Path::new(rel_path).file_name().and_then(|f| f.to_str()) == Some("options.txt") {
        return Some(FileKind::Options);
    }
    match Path::new(rel_path).extension().and_then(|e| e.to_str()) {
        Some("toml") => Some(FileKind::Toml),
        Some("json") => Some(FileKind::Json),
        Some("properties") => Some(FileKind::Properties),
        _ => None,
    }
}

/// Keys present (with a different or new value) in `current` but not matching `baseline`.
pub fn diff(current: &ConfigMap, baseline: &ConfigMap) -> ConfigMap {
    let mut changed = ConfigMap::new();

    for (file, keys) in current {
        for (key, value) in keys {
            let baseline_value = baseline.get(file).and_then(|m| m.get(key));
            if baseline_value != Some(value) {
                changed.entry(file.clone()).or_default().insert(key.clone(), value.clone());
            }
        }
    }

    changed
}

/// Overwrites entries in `base` with entries from `updates`, adding new files/keys as needed.
pub fn merge(base: &mut ConfigMap, updates: &ConfigMap) {
    for (file, keys) in updates {
        let entry = base.entry(file.clone()).or_default();
        for (key, value) in keys {
            entry.insert(key.clone(), value.clone());
        }
    }
}

/// Reads current on-disk values, but only for the files/keys named in `wanted` — used to see
/// what a patch is about to change before applying it. A key absent from the result means the
/// file or key doesn't exist locally yet.
pub fn read_matching(config_dir_root: &Path, wanted: &ConfigMap) -> ConfigMap {
    let mut out = ConfigMap::new();

    for (rel_path, keys) in wanted {
        let path = config_dir_root.join(rel_path);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let current_all = match file_kind(rel_path) {
            Some(FileKind::Toml) => extract_toml(&text),
            Some(FileKind::Json) => extract_json(&text),
            Some(FileKind::Properties) => extract_properties(&text),
            Some(FileKind::Options) => extract_options(&text),
            None => continue,
        };

        let mut matched = BTreeMap::new();
        for key in keys.keys() {
            if let Some(v) = current_all.get(key) {
                matched.insert(key.clone(), v.clone());
            }
        }

        if !matched.is_empty() {
            out.insert(rel_path.clone(), matched);
        }
    }

    out
}

const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// Prints `delta` (new values) against `old` (previous values, if any) as a colored +/- diff,
/// e.g.:
///   config/mymod-common.toml
///     server.maxPlayers
///       - 20
///       + 50
pub fn print_diff(delta: &ConfigMap, old: &ConfigMap) {
    for (file, keys) in delta {
        println!("{}", file);
        for (key, new_value) in keys {
            println!("  {}", key);
            if let Some(old_value) = old.get(file).and_then(|m| m.get(key)) {
                println!("    {}- {}{}", RED, old_value, RESET);
            }
            println!("    {}+ {}{}", GREEN, new_value, RESET);
        }
    }
}

/// Applies a patch under `config_dir_root` (the dir containing `config/...`, i.e. the instance
/// minecraft dir). Pack values always win over whatever the player has in that key.
pub fn apply_all(config_dir_root: &Path, patch: &ConfigMap) -> anyhow::Result<()> {
    for (rel_path, keys) in patch {
        let path = config_dir_root.join(rel_path);
        match file_kind(rel_path) {
            Some(FileKind::Toml) => apply_toml(&path, keys)?,
            Some(FileKind::Json) => apply_json(&path, keys)?,
            Some(FileKind::Properties) => apply_properties(&path, keys)?,
            Some(FileKind::Options) => apply_options(&path, keys)?,
            None => {}
        }
    }

    Ok(())
}

/* ---------------- TOML ---------------- */

fn extract_toml(text: &str) -> BTreeMap<String, ConfigValue> {
    let mut out = BTreeMap::new();
    if let Ok(value) = text.parse::<toml::Value>() {
        flatten_toml(&value, "", &mut out);
    }
    out
}

fn flatten_toml(value: &toml::Value, prefix: &str, out: &mut BTreeMap<String, ConfigValue>) {
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table {
                let path = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                flatten_toml(v, &path, out);
            }
        }
        toml::Value::String(s) if !prefix.is_empty() => {
            out.insert(prefix.to_string(), ConfigValue::String(s.clone()));
        }
        toml::Value::Integer(i) if !prefix.is_empty() => {
            out.insert(prefix.to_string(), ConfigValue::Integer(*i));
        }
        toml::Value::Float(f) if !prefix.is_empty() => {
            out.insert(prefix.to_string(), ConfigValue::Float(*f));
        }
        toml::Value::Boolean(b) if !prefix.is_empty() => {
            out.insert(prefix.to_string(), ConfigValue::Bool(*b));
        }
        // arrays/datetimes/root scalars: not a supported leaf, skip
        _ => {}
    }
}

fn apply_toml(path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut doc = text.parse::<toml_edit::DocumentMut>().unwrap_or_else(|_| toml_edit::DocumentMut::new());

    for (key_path, value) in keys {
        set_toml_key(doc.as_table_mut(), key_path, value);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, doc.to_string())?;
    Ok(())
}

fn set_toml_key(table: &mut toml_edit::Table, key_path: &str, value: &ConfigValue) {
    let parts: Vec<&str> = key_path.split('.').collect();
    let mut current = table;
    for part in &parts[..parts.len() - 1] {
        let entry = current.entry(part).or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        if entry.as_table().is_none() {
            *entry = toml_edit::Item::Table(toml_edit::Table::new());
        }
        current = entry.as_table_mut().unwrap();
    }

    let leaf = parts[parts.len() - 1];
    let toml_value: toml_edit::Value = match value {
        ConfigValue::String(s) => s.clone().into(),
        ConfigValue::Integer(i) => (*i).into(),
        ConfigValue::Float(f) => (*f).into(),
        ConfigValue::Bool(b) => (*b).into(),
    };
    current[leaf] = toml_edit::Item::Value(toml_value);
}

/* ---------------- JSON ---------------- */

fn extract_json(text: &str) -> BTreeMap<String, ConfigValue> {
    let mut out = BTreeMap::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        flatten_json(&value, "", &mut out);
    }
    out
}

fn flatten_json(value: &serde_json::Value, prefix: &str, out: &mut BTreeMap<String, ConfigValue>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                flatten_json(v, &path, out);
            }
        }
        serde_json::Value::String(s) if !prefix.is_empty() => {
            out.insert(prefix.to_string(), ConfigValue::String(s.clone()));
        }
        serde_json::Value::Bool(b) if !prefix.is_empty() => {
            out.insert(prefix.to_string(), ConfigValue::Bool(*b));
        }
        serde_json::Value::Number(n) if !prefix.is_empty() => {
            if let Some(i) = n.as_i64() {
                out.insert(prefix.to_string(), ConfigValue::Integer(i));
            } else if let Some(f) = n.as_f64() {
                out.insert(prefix.to_string(), ConfigValue::Float(f));
            }
        }
        // arrays/null/root scalars: not a supported leaf, skip
        _ => {}
    }
}

fn apply_json(path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let mut root: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}));

    for (key_path, value) in keys {
        set_json_key(&mut root, key_path, value);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

fn set_json_key(root: &mut serde_json::Value, key_path: &str, value: &ConfigValue) {
    let parts: Vec<&str> = key_path.split('.').collect();
    let mut current = root;
    for part in &parts[..parts.len() - 1] {
        if !current.is_object() {
            *current = serde_json::json!({});
        }
        current = current
            .as_object_mut()
            .unwrap()
            .entry(part.to_string())
            .or_insert_with(|| serde_json::json!({}));
    }

    if !current.is_object() {
        *current = serde_json::json!({});
    }

    let leaf = parts[parts.len() - 1];
    let json_value = match value {
        ConfigValue::String(s) => serde_json::Value::String(s.clone()),
        ConfigValue::Integer(i) => serde_json::json!(i),
        ConfigValue::Float(f) => serde_json::json!(f),
        ConfigValue::Bool(b) => serde_json::Value::Bool(*b),
    };
    current.as_object_mut().unwrap().insert(leaf.to_string(), json_value);
}

/* ---------------- PROPERTIES ---------------- */

fn extract_properties(text: &str) -> BTreeMap<String, ConfigValue> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            out.insert(k.trim().to_string(), ConfigValue::String(v.trim().to_string()));
        }
    }
    out
}

fn apply_properties(path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut remaining: BTreeMap<String, String> = keys
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();

    for line in lines.iter_mut() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with('!') || !trimmed.contains('=') {
            continue;
        }
        let key = trimmed.split('=').next().unwrap().trim().to_string();
        if let Some(new_value) = remaining.remove(&key) {
            *line = format!("{}={}", key, new_value);
        }
    }

    for (key, value) in remaining {
        lines.push(format!("{}={}", key, value));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

/* ---------------- OPTIONS.TXT ---------------- */
//
// Minecraft's own format: one `key:value` per line, no comments. Same overwrite-known-keys,
// leave-everything-else-untouched semantics as `properties` so a player's other settings
// (video/sound/keybinds/etc.) never get touched by a pack-managed key.

/// Unlike `config/*.toml|json|properties` (pack-curated content, safe to track wholesale),
/// `options.txt` is almost entirely the player's own client settings (sensitivity, sound,
/// keybinds, gui scale, ...). Only keys the pack has actual business managing are tracked here
/// — everything else must never end up in a diff/patch, or a maintainer's personal settings get
/// shipped to every player on the next deploy.
const TRACKED_OPTIONS_KEYS: &[&str] = &["resourcePacks"];

fn extract_options(text: &str) -> BTreeMap<String, ConfigValue> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            if TRACKED_OPTIONS_KEYS.contains(&key) {
                out.insert(key.to_string(), ConfigValue::String(v.trim().to_string()));
            }
        }
    }
    out
}

fn apply_options(path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut remaining: BTreeMap<String, String> = keys
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();

    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if !trimmed.contains(':') {
            continue;
        }
        let key = trimmed.split(':').next().unwrap().trim().to_string();
        if let Some(new_value) = remaining.remove(&key) {
            *line = format!("{}:{}", key, new_value);
        }
    }

    for (key, value) in remaining {
        lines.push(format!("{}:{}", key, value));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}
