//! Settings stored in `%APPDATA%\towerminer-gui\config.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_RPC: &str = "http://127.0.0.1:9701";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Hashrate,
    Efficiency,
}

impl Policy {
    pub fn arg(self) -> &'static str {
        match self {
            Policy::Hashrate => "hashrate",
            Policy::Efficiency => "efficiency",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub rpc: String,
    pub remember_key: bool,
    /// Written only when `remember_key` is set.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    pub coinbase: String,
    /// 0 = all logical CPUs.
    pub threads: usize,
    pub policy: Policy,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rpc: DEFAULT_RPC.to_string(),
            remember_key: false,
            key: String::new(),
            coinbase: String::new(),
            threads: 0,
            policy: Policy::Hashrate,
        }
    }
}

/// `%APPDATA%\towerminer-gui\config.json`; next to the program when APPDATA
/// is not set.
pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok()?.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("towerminer-gui").join("config.json")
}

pub fn load() -> Config {
    let mut c: Config = std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if !c.remember_key {
        c.key.clear();
    }
    if c.rpc.trim().is_empty() {
        c.rpc = DEFAULT_RPC.to_string();
    }
    c
}

/// Write the settings; the key only when "Remember key" is set.
pub fn save(c: &Config) -> std::io::Result<()> {
    let mut c = c.clone();
    if !c.remember_key {
        c.key.clear();
    }
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&c)?)?;
    std::fs::rename(&tmp, &p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_not_serialized_when_empty_and_defaults_fill_gaps() {
        let c = Config { key: String::new(), ..Default::default() };
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("\"key\""));
        let c: Config = serde_json::from_str(r#"{"rpc":"http://x:1","policy":"efficiency","future":1}"#).unwrap();
        assert_eq!(c.rpc, "http://x:1");
        assert_eq!(c.policy, Policy::Efficiency);
        assert_eq!(c.threads, 0);
        assert!(!c.remember_key);
    }
}
