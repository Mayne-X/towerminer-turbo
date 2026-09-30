//! Test double for `towerminer.exe --status-json`: speaks the JSON contract
//! with made-up numbers so the GUI can be exercised without mining.
//! Never shipped. Copy it next to the GUI as `towerminer.exe`.
//!
//! Environment:
//!   FAKE_MODE       mining (default) | error (error event then exit 1 at the
//!                   3rd report) | crash (exit 101 at the 3rd report, no event)
//!                   | waiting (no template: height null, state waiting)
//!   FAKE_PERIOD_MS  milliseconds between reports (default 1000)
//!   FAKE_PAGES      value of "pages" in the profile (default "normal")

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn out(line: &str) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{line}");
    let _ = o.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = std::env::var("FAKE_MODE").unwrap_or_else(|_| "mining".into());
    let period = std::env::var("FAKE_PERIOD_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(1000u64);
    let pages = std::env::var("FAKE_PAGES").unwrap_or_else(|_| "normal".into());
    let key = std::env::var("TOWERMINER_KEY").ok();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let threads: u64 = arg("--threads").and_then(|s| s.parse().ok()).unwrap_or(1);

    eprintln!("fake towerminer (test double) pid={} mode={mode}", std::process::id());
    eprintln!("argv: {}", args.join(" "));
    match &key {
        Some(k) => eprintln!(
            "key: set via TOWERMINER_KEY ({} chars), present in argv: {}",
            k.len(),
            args.iter().any(|a| !k.is_empty() && a.contains(k.as_str()))
        ),
        None => eprintln!("key: not set"),
    }
    if !args.iter().any(|a| a == "--status-json") {
        eprintln!("error: --status-json missing");
        std::process::exit(2);
    }

    out(&format!(
        r#"{{"type":"profile","version":"0.3.0","backend":"avx2 (fake)","cpu":"Fake CPU 16-Core Processor","threads":{threads},"tpc":1,"pads":1,"prefetch":false,"kernel":"fast","pages":"{pages}","future_field":[1,2]}}"#
    ));
    out("this stdout line is not JSON");
    out(r#"{"type":"future_event","x":1}"#);

    let start = std::time::Instant::now();
    let mut height = 24_950u64;
    let (mut found, mut acc, mut refu, mut unk) = (0u64, 0u64, 0u64, 0u64);
    for tick in 1u64.. {
        std::thread::sleep(Duration::from_millis(period));
        let hps = 1050.0 * threads as f64 + (tick % 7) as f64 * 13.7;
        if tick % 4 == 0 {
            height += 1;
        }
        if mode == "mining" && tick % 3 == 0 {
            found += 1;
            let (res, hash) = match (tick / 3) % 3 {
                1 => {
                    acc += 1;
                    ("accepted", format!("\"{:064x}\"", 0xabcdef_u64 * tick))
                }
                2 => {
                    refu += 1;
                    ("refused", format!("\"{:064x}\"", 0x123456_u64 * tick))
                }
                _ => {
                    unk += 1;
                    ("unknown", "null".to_string())
                }
            };
            out(&format!(
                r#"{{"type":"block","ts":{},"height":{height},"result":"{res}","hash":{hash}}}"#,
                now()
            ));
            eprintln!("\x1b[32mblock {height} found -> {res}\x1b[0m");
        }
        if (mode == "error" || mode == "crash") && tick == 3 {
            if mode == "error" {
                out(r#"{"type":"error","message":"the pool refused the mining key (HTTP 401 Unauthorized)"}"#);
                eprintln!("\x1b[31mFATAL: the pool refused the mining key (HTTP 401 Unauthorized)\x1b[0m");
                std::process::exit(1);
            }
            eprintln!("thread 'worker-3' panicked at src/walk.rs:42:9: simulated crash");
            std::process::exit(101);
        }
        let (state, h, msg) = if mode == "waiting" {
            ("waiting", "null".to_string(), "cannot reach the node: connection refused")
        } else {
            ("mining", height.to_string(), "")
        };
        let hps = if mode == "waiting" { 0.0 } else { hps };
        out(&format!(
            r#"{{"type":"status","ts":{},"hps":{hps:.1},"height":{h},"found":{found},"accepted":{acc},"refused":{refu},"unknown":{unk},"uptime_s":{},"state":"{state}","message":"{msg}"}}"#,
            now(),
            start.elapsed().as_secs()
        ));
        eprintln!("\x1b[36m[{:>4}s]\x1b[0m {hps:.0} H/s height {h} found {found} ({acc}/{refu}/{unk})", start.elapsed().as_secs());
    }
}
