//! The miner's `--status-json` stream: one JSON object per stdout line.
//!
//! Parsing is deliberately lenient: a line that is not JSON, an unknown event
//! type or a field of an unexpected type never fails the stream. A missing or
//! mistyped field is simply `None`.

use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    pub version: Option<String>,
    pub backend: Option<String>,
    pub cpu: Option<String>,
    pub threads: Option<u64>,
    pub tpc: Option<u64>,
    pub pads: Option<u64>,
    pub prefetch: Option<bool>,
    pub kernel: Option<String>,
    pub pages: Option<String>,
}

impl Profile {
    /// Large pages are active ("2M" on Linux, "large" on Windows). `None`
    /// when the miner did not say.
    pub fn large_pages(&self) -> Option<bool> {
        let p = self.pages.as_deref()?.trim().to_ascii_lowercase();
        Some(p == "2m" || p == "large")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub ts: Option<u64>,
    pub hps: Option<f64>,
    pub height: Option<u64>,
    pub found: Option<u64>,
    pub accepted: Option<u64>,
    pub refused: Option<u64>,
    pub unknown: Option<u64>,
    pub uptime_s: Option<u64>,
    pub state: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Block {
    pub ts: Option<u64>,
    pub height: Option<u64>,
    pub result: Option<String>,
    pub hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Profile(Profile),
    Status(Status),
    Block(Block),
    Error(String),
}

/// What one stdout line is.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Event(Event),
    /// A JSON object that is not a known event (future type): ignored.
    OtherJson,
    /// Not JSON: shown in the log.
    Text,
}

pub fn classify(line: &str) -> Line {
    let t = line.trim().trim_start_matches('\u{feff}');
    if !t.starts_with('{') {
        return Line::Text;
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(o)) => event(&o).map(Line::Event).unwrap_or(Line::OtherJson),
        _ => Line::Text,
    }
}

/// Parse one stdout line. `None` for anything that is not a known event.
#[cfg(test)]
pub fn parse_line(line: &str) -> Option<Event> {
    match classify(line) {
        Line::Event(e) => Some(e),
        _ => None,
    }
}

fn event(o: &Map<String, Value>) -> Option<Event> {
    match o.get("type")?.as_str()? {
        "profile" => Some(Event::Profile(Profile {
            version: text(o, "version"),
            backend: text(o, "backend"),
            cpu: text(o, "cpu"),
            threads: uint(o, "threads"),
            tpc: uint(o, "tpc"),
            pads: uint(o, "pads"),
            prefetch: flag(o, "prefetch"),
            kernel: text(o, "kernel"),
            pages: text(o, "pages"),
        })),
        "status" => Some(Event::Status(Status {
            ts: uint(o, "ts"),
            hps: float(o, "hps"),
            height: uint(o, "height"),
            found: uint(o, "found"),
            accepted: uint(o, "accepted"),
            refused: uint(o, "refused"),
            unknown: uint(o, "unknown"),
            uptime_s: uint(o, "uptime_s"),
            state: text(o, "state"),
            message: text(o, "message"),
        })),
        "block" => Some(Event::Block(Block {
            ts: uint(o, "ts"),
            height: uint(o, "height"),
            result: text(o, "result"),
            hash: text(o, "hash"),
        })),
        "error" => Some(Event::Error(
            text(o, "message").unwrap_or_else(|| "unspecified error".to_string()),
        )),
        _ => None,
    }
}

fn text(o: &Map<String, Value>, k: &str) -> Option<String> {
    match o.get(k)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn float(o: &Map<String, Value>, k: &str) -> Option<f64> {
    let f = match o.get(k)? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    f.is_finite().then_some(f)
}

fn uint(o: &Map<String, Value>, k: &str) -> Option<u64> {
    let v = o.get(k)?;
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (f.is_finite() && f >= 0.0 && f < 1.8e19).then(|| f.round() as u64)
}

fn flag(o: &Map<String, Value>, k: &str) -> Option<bool> {
    match o.get(k)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_line() {
        let l = r#"{"type":"profile","version":"0.3.0","backend":"avx2","cpu":"AMD Ryzen 9 5950X","threads":16,"tpc":1,"pads":1,"prefetch":false,"kernel":"fast","pages":"large"}"#;
        let Some(Event::Profile(p)) = parse_line(l) else { panic!() };
        assert_eq!(p.version.as_deref(), Some("0.3.0"));
        assert_eq!(p.threads, Some(16));
        assert_eq!(p.tpc, Some(1));
        assert_eq!(p.prefetch, Some(false));
        assert_eq!(p.large_pages(), Some(true));
    }

    #[test]
    fn pages_kinds() {
        let pg = |s: &str| Profile { pages: Some(s.into()), ..Default::default() }.large_pages();
        assert_eq!(pg("2M"), Some(true));
        assert_eq!(pg("large"), Some(true));
        assert_eq!(pg("4K"), Some(false));
        assert_eq!(pg("normal"), Some(false));
        assert_eq!(Profile::default().large_pages(), None);
    }

    #[test]
    fn status_line_with_null_height_and_crlf() {
        let l = "{\"type\":\"status\",\"ts\":1790000000,\"hps\":16892.4,\"height\":null,\"found\":3,\"accepted\":2,\"refused\":0,\"unknown\":1,\"uptime_s\":125,\"state\":\"waiting\",\"message\":\"no template\"}\r\n";
        let Some(Event::Status(s)) = parse_line(l) else { panic!() };
        assert_eq!(s.height, None);
        assert_eq!(s.hps, Some(16892.4));
        assert_eq!((s.found, s.accepted, s.refused, s.unknown), (Some(3), Some(2), Some(0), Some(1)));
        assert_eq!(s.state.as_deref(), Some("waiting"));
    }

    #[test]
    fn block_and_error() {
        let b = parse_line(r#"{"type":"block","ts":1,"height":24900,"result":"accepted","hash":"00ab"}"#);
        assert_eq!(
            b,
            Some(Event::Block(Block {
                ts: Some(1),
                height: Some(24900),
                result: Some("accepted".into()),
                hash: Some("00ab".into())
            }))
        );
        let b = parse_line(r#"{"type":"block","ts":2,"height":24901,"result":"unknown","hash":null}"#);
        let Some(Event::Block(b)) = b else { panic!() };
        assert_eq!(b.hash, None);
        assert_eq!(
            parse_line(r#"{"type":"error","message":"pool refused the key"}"#),
            Some(Event::Error("pool refused the key".into()))
        );
    }

    #[test]
    fn lenient() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("mining at 16.9 kH/s"), None);
        assert_eq!(parse_line("{not json"), None);
        assert_eq!(parse_line(r#"{"type":"future","x":1}"#), None);
        assert_eq!(parse_line(r#"{"no_type":1}"#), None);
        assert_eq!(parse_line("[1,2]"), None);
        // Unknown fields ignored, mistyped fields become None, floats accepted.
        let Some(Event::Status(s)) =
            parse_line(r#"{"type":"status","extra":{"a":1},"hps":"12.5","found":2.0,"accepted":"x","height":-1}"#)
        else {
            panic!()
        };
        assert_eq!(s.hps, Some(12.5));
        assert_eq!(s.found, Some(2));
        assert_eq!(s.accepted, None);
        assert_eq!(s.height, None);
    }

    #[test]
    fn classify_lines() {
        assert_eq!(classify("plain text from the miner"), Line::Text);
        assert_eq!(classify("{truncated"), Line::Text);
        assert_eq!(classify(r#"{"type":"future","x":1}"#), Line::OtherJson);
        assert_eq!(classify(r#"{"x":1}"#), Line::OtherJson);
        assert!(matches!(classify(r#"{"type":"error","message":"m"}"#), Line::Event(Event::Error(_))));
    }
}
