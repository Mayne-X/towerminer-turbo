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
    /// Sent as --worker-name when not empty.
    pub worker_name: String,
    /// 0 = all logical CPUs.
    pub threads: usize,
    pub policy: Policy,
}

impl Config {
    /// Command line of `towerminer.exe` (the key goes in the environment).
    pub fn miner_args(&self, threads: usize) -> Vec<String> {
        let mut args = vec!["--rpc".to_string(), self.rpc.clone()];
        if !self.coinbase.is_empty() {
            args.push("--coinbase".into());
            args.push(self.coinbase.clone());
        }
        if !self.worker_name.is_empty() {
            args.push("--worker-name".into());
            args.push(self.worker_name.clone());
        }
        args.extend([
            "--threads".to_string(),
            threads.to_string(),
            "--policy".to_string(),
            self.policy.arg().to_string(),
            "--status-json".to_string(),
        ]);
        args
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rpc: DEFAULT_RPC.to_string(),
            remember_key: false,
            key: String::new(),
            coinbase: String::new(),
            worker_name: String::new(),
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
        // Settings saved by 0.3.0 before the field existed: no worker name.
        assert_eq!(c.worker_name, "");
    }

    #[test]
    fn miner_args_pass_worker_name_only_when_set() {
        let c = Config { rpc: "http://x:1".into(), ..Default::default() };
        let a = c.miner_args(4);
        assert_eq!(
            a,
            ["--rpc", "http://x:1", "--threads", "4", "--policy", "hashrate", "--status-json"].map(String::from)
        );
        assert!(!a.iter().any(|s| s == "--worker-name"));
        let c = Config { rpc: "http://x:1".into(), worker_name: "rig-1".into(), coinbase: "j1abc".into(), ..Default::default() };
        let a = c.miner_args(2);
        let i = a.iter().position(|s| s == "--worker-name").expect("--worker-name");
        assert_eq!(a[i + 1], "rig-1");
        assert!(a.windows(2).any(|w| w[0] == "--coinbase" && w[1] == "j1abc"));
    }
}
