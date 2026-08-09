//! Shared key=val config patching, used by `deploy` (extract + diff pack-managed keys) and
//! `wolfpacker` (apply them on top of a player's local config, overwriting only those keys).
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

/// True if two values should be treated as unchanged. Floats compare with a relative epsilon
/// since some mods (e.g. attributefix, particular) re-emit their own defaults with 1-ULP text
/// drift on every launch, which would otherwise flag as a spurious config change on every deploy.
fn values_equal(a: &ConfigValue, b: &ConfigValue) -> bool {
    match (a, b) {
        (ConfigValue::Float(x), ConfigValue::Float(y)) => {
            (x - y).abs() <= f64::EPSILON * x.abs().max(y.abs()).max(1.0)
        }
        _ => a == b,
    }
}

/// Keys present (with a different or new value) in `current` but not matching `baseline`.
pub fn diff(current: &ConfigMap, baseline: &ConfigMap) -> ConfigMap {
    let mut changed = ConfigMap::new();

    for (file, keys) in current {
        for (key, value) in keys {
            let baseline_value = baseline.get(file).and_then(|m| m.get(key));
            let unchanged = baseline_value.is_some_and(|bv| values_equal(bv, value));
            if !unchanged {
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

/// (rel_path, dotted key) pairs that are seeded once and then left alone forever after, instead
/// of the default "pack always wins" behavior — for settings that are a personal hardware/
/// performance preference (e.g. Distant Horizons' CPU thread usage) rather than pack content the
/// maintainer should keep re-forcing on every sync. Same semantics as `SEEDED_OPTIONS_PREFIX`
/// below: applied unconditionally the first time (even overwriting a value the player already
/// set), then skipped forever once the player's local value no longer matches what was last seeded.
const SEEDED_TOML_KEYS: &[(&str, &str)] = &[
    ("config/DistantHorizons.toml", "common.multiThreading.numberOfThreads"),
    ("config/DistantHorizons.toml", "common.multiThreading.threadRunTimeRatio"),
    ("config/DistantHorizons.toml", "common.multiThreading.threadPriority"),
];

/// Applies a patch under `config_dir_root` (the dir containing `config/...`, i.e. the instance
/// minecraft dir). Pack values always win over whatever the player has in that key, except for
/// `SEEDED_TOML_KEYS` entries, which are seed-once (see its docs).
pub fn apply_all(config_dir_root: &Path, patch: &ConfigMap) -> anyhow::Result<()> {
    for (rel_path, keys) in patch {
        let path = config_dir_root.join(rel_path);
        match file_kind(rel_path) {
            Some(FileKind::Toml) => apply_toml(rel_path, &path, keys)?,
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

fn apply_toml(rel_path: &str, path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut doc = text.parse::<toml_edit::DocumentMut>().unwrap_or_else(|_| toml_edit::DocumentMut::new());
    let current_all = extract_toml(&text);

    let seed_state_path = path.with_file_name(format!(
        ".{}.wolfpacker-seed-state.json",
        path.file_name().and_then(|f| f.to_str()).unwrap_or("config")
    ));
    let mut seed_state: BTreeMap<String, String> = fs::read_to_string(&seed_state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut seed_state_changed = false;

    for (key_path, value) in keys {
        if SEEDED_TOML_KEYS.contains(&(rel_path, key_path.as_str())) {
            // Diverged = local value exists and no longer matches what we last seeded, meaning
            // the player changed it since. Absent from either side just means "never touched by
            // us or the player" — safe to seed.
            let diverged = match (current_all.get(key_path), seed_state.get(key_path)) {
                (Some(local), Some(last_seeded)) => &local.to_string() != last_seeded,
                _ => false,
            };
            if diverged {
                continue;
            }
            seed_state.insert(key_path.clone(), value.to_string());
            seed_state_changed = true;
        }
        set_toml_key(doc.as_table_mut(), key_path, value);
    }

    if seed_state_changed {
        fs::write(&seed_state_path, serde_json::to_string(&seed_state)?)?;
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

pub fn extract_properties(text: &str) -> BTreeMap<String, ConfigValue> {
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

/// Overwrites known `key=value` lines in `text` with `keys`, appending any not already present.
/// Lines that aren't a plain `key=value` pair (comments, `[Section]` headers, blanks) pass
/// through untouched — this is what lets it double as an instance.cfg patcher (see
/// `wolfpacker::main`'s memory-allocation sync), since PrismLauncher's INI-style `[General]`
/// header contains no `=` and is never touched.
pub fn patch_properties_text(text: &str, keys: &BTreeMap<String, ConfigValue>) -> String {
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

    lines.join("\n") + "\n"
}

fn apply_properties(path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let patched = patch_properties_text(&text, keys);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, patched)?;
    Ok(())
}

/* ---------------- OPTIONS.TXT ---------------- */
//
// Minecraft's own format: one `key:value` per line, no comments. Same overwrite-known-keys,
// leave-everything-else-untouched semantics as `properties` so a player's other settings
// (video/sound/keybinds/etc.) never get touched by a pack-managed key.

/// Unlike `config/*.toml|json|properties` (pack-curated content, safe to track wholesale),
/// `options.txt` is almost entirely the player's own client settings (sensitivity, sound,
/// keybinds, gui scale, ...). Only keys the pack has actual business managing — and where
/// overwrite-on-change is actually correct — are tracked here; everything else must never end up
/// in a diff/patch, or a maintainer's personal settings get shipped to every player on the next
/// deploy. `resourcePacks` is deliberately NOT here: it's a list a player (or other mods) can add
/// their own entries to, and this mechanism always overwrites a tracked key wholesale — correct
/// for a scalar like a forced key, wrong for a list, where it'd nuke unrelated entries. See
/// `merge_resource_pack_entries` for the list-merge this instead uses.
const TRACKED_OPTIONS_KEYS: &[&str] = &[];

/// Keybind lines (`key_key.foo:...`, `key_gui.bar:...`) are also trackable, but with different
/// semantics than `TRACKED_OPTIONS_KEYS`: instead of "pack always wins", it's "seed the pack's
/// value once, then leave it alone forever once a player's local value diverges from what was
/// last seeded" — see `apply_options`'s use of the per-key seed-state file. This still risks a
/// one-time rebind for a player who happened to already customize a key before the pack ever
/// tracked it, but protects every customization after that.
const SEEDED_OPTIONS_PREFIX: &str = "key_";

fn extract_options(text: &str) -> BTreeMap<String, ConfigValue> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            if TRACKED_OPTIONS_KEYS.contains(&key) || key.starts_with(SEEDED_OPTIONS_PREFIX) {
                out.insert(key.to_string(), ConfigValue::String(v.trim().to_string()));
            }
        }
    }
    out
}

fn apply_options(path: &Path, keys: &BTreeMap<String, ConfigValue>) -> anyhow::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();

    let current: BTreeMap<String, String> = lines
        .iter()
        .filter_map(|l| {
            let t = l.trim();
            t.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();

    let seed_state_path = path.with_file_name(".wolfpacker-keybind-state.json");
    let mut seed_state: BTreeMap<String, String> = fs::read_to_string(&seed_state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut seed_state_changed = false;

    let mut remaining: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in keys {
        let new_value = value.to_string();
        if key.starts_with(SEEDED_OPTIONS_PREFIX) {
            // Diverged = local value exists and no longer matches what we last seeded, meaning
            // the player rebound it since. Absent from either side just means "never touched by
            // us or the player" — safe (and, per product decision, intended) to seed.
            let diverged = match (current.get(key), seed_state.get(key)) {
                (Some(local), Some(last_seeded)) => local != last_seeded,
                _ => false,
            };
            if diverged {
                continue;
            }
            seed_state.insert(key.clone(), new_value.clone());
            seed_state_changed = true;
        }
        remaining.insert(key.clone(), new_value);
    }

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

    if seed_state_changed {
        fs::write(&seed_state_path, serde_json::to_string(&seed_state)?)?;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

/// Adds/removes exactly the given `file/<name>` entries in options.txt's `resourcePacks` list,
/// leaving every other entry (a player's own packs, vanilla, ids other mods register, ...)
/// untouched — unlike the generic key=val patching above, this is a merge, not an overwrite,
/// since `resourcePacks` is a set the pack only partially owns.
///
/// `order` is the pack maintainer's declared load order for the *managed* entries (from the
/// source instance's own options.txt), as bare names — e.g. `["Base.zip", "Overlay.zip"]`. Any
/// name in `order` that isn't currently present locally (not downloaded/enabled) is ignored.
/// Managed entries are repositioned as a contiguous block to match `order`, anchored at the
/// index of the first managed entry that was already present; every non-managed entry keeps its
/// original relative position untouched. An empty `order` skips reordering entirely, so callers
/// that have no order data (e.g. the source manifest doesn't publish one) leave existing order as-is.
pub fn merge_resource_pack_entries(
    options_path: &Path,
    add: &[String],
    remove: &[String],
    order: &[String],
) -> anyhow::Result<()> {
    if add.is_empty() && remove.is_empty() && order.is_empty() {
        return Ok(());
    }

    let text = fs::read_to_string(options_path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();

    let mut packs: Vec<String> = lines
        .iter()
        .find_map(|l| l.trim().strip_prefix("resourcePacks:"))
        .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
        .unwrap_or_default();

    for name in remove {
        packs.retain(|p| p != &format!("file/{}", name));
    }
    for name in add {
        let entry = format!("file/{}", name);
        if !packs.contains(&entry) {
            packs.push(entry);
        }
    }

    if !order.is_empty() {
        let order_set: std::collections::HashSet<&str> = order.iter().map(|s| s.as_str()).collect();
        let is_managed = |p: &str| p.strip_prefix("file/").map(|n| order_set.contains(n)).unwrap_or(false);

        let anchor = packs.iter().position(|p| is_managed(p)).map(|idx| {
            packs[..idx].iter().filter(|p| !is_managed(p)).count()
        });

        let present_managed: std::collections::HashSet<&str> = packs
            .iter()
            .filter_map(|p| p.strip_prefix("file/"))
            .filter(|n| order_set.contains(n))
            .collect();
        let reordered_managed: Vec<String> = order
            .iter()
            .filter(|n| present_managed.contains(n.as_str()))
            .map(|n| format!("file/{}", n))
            .collect();

        let mut new_packs: Vec<String> = packs.iter().filter(|p| !is_managed(p)).cloned().collect();
        let insert_at = anchor.unwrap_or(new_packs.len());
        for (i, entry) in reordered_managed.into_iter().enumerate() {
            new_packs.insert((insert_at + i).min(new_packs.len()), entry);
        }
        packs = new_packs;
    }

    let new_line = format!("resourcePacks:{}", serde_json::to_string(&packs)?);
    let mut replaced = false;
    for line in lines.iter_mut() {
        if line.trim().starts_with("resourcePacks:") {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(new_line);
    }

    if let Some(parent) = options_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(options_path, lines.join("\n") + "\n")?;
    Ok(())
}
