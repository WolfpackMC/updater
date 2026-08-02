//! Shared key=val config patching, used by `deploy` (extract + diff pack-managed keys) and
//! `mcupdater` (apply them on top of a player's local config, overwriting only those keys).
//! Player-added keys and files not touched by the pack are never read or written.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

impl ConfigValue {
    fn as_property_string(&self) -> String {
        match self {
            ConfigValue::String(s) => s.clone(),
            ConfigValue::Integer(i) => i.to_string(),
            ConfigValue::Float(f) => f.to_string(),
            ConfigValue::Bool(b) => b.to_string(),
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
        let ext = Path::new(rel_path).extension().and_then(|e| e.to_str()).unwrap_or("");
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let keys = match ext {
            "toml" => extract_toml(&text),
            "json" => extract_json(&text),
            "properties" => extract_properties(&text),
            _ => continue,
        };

        if !keys.is_empty() {
            map.insert(rel_path.clone(), keys);
        }
    }

    map
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

/// Applies a patch under `config_dir_root` (the dir containing `config/...`, i.e. the instance
/// minecraft dir). Pack values always win over whatever the player has in that key.
pub fn apply_all(config_dir_root: &Path, patch: &ConfigMap) -> anyhow::Result<()> {
    for (rel_path, keys) in patch {
        let path = config_dir_root.join(rel_path);
        match path.extension().and_then(|e| e.to_str()) {
            Some("toml") => apply_toml(&path, keys)?,
            Some("json") => apply_json(&path, keys)?,
            Some("properties") => apply_properties(&path, keys)?,
            _ => {}
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
        current = current
            .entry(part)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .expect("pack-managed config key path collides with a non-table value");
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
        .map(|(k, v)| (k.clone(), v.as_property_string()))
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
