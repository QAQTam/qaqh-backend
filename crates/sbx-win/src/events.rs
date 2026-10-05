//! 裁决/生命周期事件流(spec §4.3 journal.jsonl 的核心侧形态)。
//! 事件是输出流(`EventSink`),不是文件契约——落文件是 sink 的实现选择。

use serde::Serialize;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// unix 毫秒(v0;正式版升 RFC3339)
    pub ts: u64,
    /// "plan" | "acl_apply" | "token" | "spawn" | "exit" | "check"
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl Event {
    pub fn new(event: &str) -> Self {
        Event {
            ts: now_millis(),
            event: event.to_string(),
            path: None,
            detail: None,
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub trait EventSink {
    fn emit(&self, event: &Event);
}

pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &Event) {}
}

/// 逐行 JSON sink(journal.jsonl 或 stderr)。
pub struct JsonlSink<W: Write + Send> {
    inner: Mutex<W>,
}

impl<W: Write + Send> JsonlSink<W> {
    pub fn new(inner: W) -> Self {
        JsonlSink {
            inner: Mutex::new(inner),
        }
    }
}

impl<W: Write + Send> EventSink for JsonlSink<W> {
    fn emit(&self, event: &Event) {
        if let Ok(mut w) = self.inner.lock() {
            if let Ok(line) = serde_json::to_string(event) {
                let _ = writeln!(w, "{line}");
            }
        }
    }
}
