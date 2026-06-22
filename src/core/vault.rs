//! Vault client — file-based persistence on the gdrive vault root. Same pattern
//! as the sibling sleeves: atomic writes (temp file + rename) and a short read
//! cache. Subfolders: state / cards / reflections / meta.
//!
//! This sleeve's durable state: NAV history (CAGR glide + drawdown), per-name
//! high-water marks (drawdown-deploy rule), the phased-build progress record,
//! daily action cards, and the daily/quarterly digests.

use chrono::Local;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const SUBFOLDERS: [&str; 4] = ["state", "cards", "reflections", "meta"];

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

fn now_iso() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
}

pub struct VaultClient {
    root: PathBuf,
    cache: Mutex<HashMap<String, (String, f64)>>,
    cache_ttl_seconds: f64,
}

impl VaultClient {
    pub fn new(vault_path: Option<PathBuf>) -> Self {
        let root = vault_path
            .unwrap_or_else(|| home().join("gdrive").join("vault").join("aschenbrenner-unlimited"));
        let _ = std::fs::create_dir_all(&root);
        VaultClient {
            root,
            cache: Mutex::new(HashMap::new()),
            cache_ttl_seconds: 300.0,
        }
    }

    #[allow(dead_code)]
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    fn path_for_sub(&self, subfolder: &str) -> PathBuf {
        self.root.join(subfolder)
    }

    pub fn read_vault_file(&self, path: &Path) -> Option<String> {
        if !path.exists() {
            return None;
        }
        let key = path.to_string_lossy().replace('\\', "/");
        let now = Local::now().timestamp_millis() as f64 / 1000.0;
        {
            let cache = self.cache.lock().unwrap();
            if let Some((content, ts)) = cache.get(&key) {
                if now - ts < self.cache_ttl_seconds {
                    return Some(content.clone());
                }
            }
        }
        let content = std::fs::read_to_string(path).ok()?;
        self.cache.lock().unwrap().insert(key, (content.clone(), now));
        Some(content)
    }

    pub fn write_vault_file(&self, content: &str, target: &str, subfolder: &str) -> std::io::Result<PathBuf> {
        let base = self.path_for_sub(subfolder);
        std::fs::create_dir_all(&base)?;
        let path = base.join(target);
        let tmp = base.join(format!(".{}.{}.tmp", target, std::process::id()));
        std::fs::write(&tmp, content)?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&tmp, &path)?;
        self.cache.lock().unwrap().remove(&path.to_string_lossy().to_string());
        Ok(path)
    }

    /// Files in `subfolder` whose name contains `contains` and ends with `ext`.
    pub fn list_pattern(&self, subfolder: &str, contains: &str, ext: &str) -> Vec<PathBuf> {
        let dir = self.path_for_sub(subfolder);
        let mut out = vec![];
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if name.contains(contains) && name.ends_with(ext) {
                    out.push(e.path());
                }
            }
        }
        out.sort();
        out
    }

    pub fn read(&self, subfolder: &str, filename: &str) -> Option<String> {
        self.read_vault_file(&self.path_for_sub(subfolder).join(filename))
    }

    pub fn write(&self, subfolder: &str, filename: &str, content: &str) -> std::io::Result<PathBuf> {
        self.write_vault_file(content, filename, subfolder)
    }

    /// Read a JSON document, or `Value::Null` when absent/corrupt.
    pub fn read_json(&self, subfolder: &str, filename: &str) -> Value {
        self.read(subfolder, filename)
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    }

    /// Write a JSON document (pretty).
    pub fn write_json(&self, subfolder: &str, filename: &str, v: &Value) -> std::io::Result<PathBuf> {
        self.write(subfolder, filename, &serde_json::to_string_pretty(v).unwrap())
    }

    // ── per-name high-water marks (drawdown-deploy rule) ────────────────────

    /// Map of `ticker -> highest price seen` (state/high-water.json).
    pub fn high_water(&self) -> HashMap<String, f64> {
        let v = self.read_json("state", "high-water.json");
        let mut out = HashMap::new();
        if let Some(obj) = v.get("marks").and_then(|m| m.as_object()) {
            for (k, val) in obj {
                if let Some(px) = val.as_f64() {
                    out.insert(k.clone(), px);
                }
            }
        }
        out
    }

    /// Persist the high-water map (ratcheting up only is the caller's job).
    pub fn save_high_water(&self, marks: &HashMap<String, f64>) {
        let obj: serde_json::Map<String, Value> =
            marks.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let _ = self.write_json("state", "high-water.json", &json!({"marks": obj, "saved_at": now_iso()}));
    }

    // ── NAV history (CAGR glide + portfolio-level drawdown) ──────────────────

    /// Append (or overwrite) today's NAV point, capped at `max_points`.
    pub fn record_nav(&self, date_str: &str, nav: f64, max_points: usize) {
        let mut points: std::collections::BTreeMap<String, f64> = Default::default();
        if let Some(arr) = self.read_json("state", "nav-history.json").get("points").and_then(|p| p.as_array()) {
            for p in arr {
                if let (Some(d), Some(v)) = (
                    p.get("date").and_then(|v| v.as_str()),
                    p.get("nav").and_then(|v| v.as_f64()),
                ) {
                    points.insert(d.to_string(), v);
                }
            }
        }
        points.insert(date_str.to_string(), (nav * 100.0).round() / 100.0);
        let mut ordered: Vec<_> = points.into_iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        let n = ordered.len();
        if n > max_points {
            ordered = ordered.split_off(n - max_points);
        }
        let pts: Vec<Value> = ordered.into_iter().map(|(d, v)| json!({"date": d, "nav": v})).collect();
        let _ = self.write_json("state", "nav-history.json", &json!({"points": pts}));
    }

    /// Write a per-run meta record.
    #[allow(dead_code)]
    pub fn meta_save_run(&self, date_str: &str, window: &str, payload: Value) -> std::io::Result<PathBuf> {
        let data = json!({"date": date_str, "window": window, "saved_at": now_iso(), "run": payload});
        let name = format!("run-{}-{}.json", date_str, window);
        self.write_json("meta", &name, &data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_vault() -> VaultClient {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "asch-vault-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        VaultClient::new(Some(dir))
    }

    #[test]
    fn write_then_read_roundtrips() {
        let v = tmp_vault();
        v.write("meta", "hello.txt", "world").unwrap();
        assert_eq!(v.read("meta", "hello.txt").as_deref(), Some("world"));
    }

    #[test]
    fn high_water_roundtrips() {
        let v = tmp_vault();
        let mut m = HashMap::new();
        m.insert("GEV".to_string(), 1049.0);
        m.insert("CEG".to_string(), 267.0);
        v.save_high_water(&m);
        let got = v.high_water();
        assert_eq!(got.get("GEV").copied(), Some(1049.0));
        assert_eq!(got.get("CEG").copied(), Some(267.0));
    }

    #[test]
    fn nav_history_dedupes_by_date_and_caps() {
        let v = tmp_vault();
        v.record_nav("2026-06-19", 40_000.0, 3);
        v.record_nav("2026-06-19", 40_500.0, 3); // same day → overwrite
        v.record_nav("2026-06-20", 41_000.0, 3);
        let pts = v.read_json("state", "nav-history.json");
        let arr = pts["points"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["nav"].as_f64().unwrap(), 40_500.0);
    }
}
