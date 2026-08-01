//! AgentFS bridge — progressive enhancement for `memory` and tool recording.
//!
//! Two modes, selected at compile time:
//! - `feature = "agentfs"` → uses the real `agentfs_sdk` crate (async)
//! - default → local JSON-file storage (no extra deps, zero compile cost)
//!
//! All public helpers (`try_kv_set`, `try_kv_get`, `try_record_tool`) are
//! best-effort — failures are logged but never propagated.

use std::sync::{LazyLock, Mutex};

// ── Mode selection ──────────────────────────────────────────────────────

#[cfg(feature = "agentfs")]
mod imp {
    use tokio::runtime::Runtime;

    static RT: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().unwrap());

    pub struct Inner {
        agent: agentfs_sdk::AgentFS,
    }

    impl Inner {
        pub fn init(session_id: &str) -> Option<Self> {
            match RT.block_on(agentfs_sdk::AgentFS::open(
                agentfs_sdk::AgentFSOptions::with_id(session_id),
            )) {
                Ok(agent) => Some(Self { agent }),
                Err(e) => {
                    log::warn!("[agentfs] SDK init failed: {e}");
                    None
                }
            }
        }

        pub fn kv_set(&self, key: &str, value: &str) {
            let v = serde_json::Value::String(value.to_string());
            if let Err(e) = RT.block_on(self.agent.kv.set(key, &v)) {
                log::warn!("[agentfs] kv_set({key}) failed: {e}");
            }
        }

        pub fn kv_get(&self, key: &str) -> Option<String> {
            let val: Option<serde_json::Value> =
                RT.block_on(self.agent.kv.get(key)).ok().flatten()?;
            match val {
                serde_json::Value::String(s) => Some(s),
                other => Some(other.to_string()),
            }
        }

        pub fn kv_delete(&self, key: &str) {
            if let Err(e) = RT.block_on(self.agent.kv.delete(key)) {
                log::warn!("[agentfs] kv_delete({key}) failed: {e}");
            }
        }

        pub fn record_tool(
            &self,
            name: &str,
            action: &str,
            params_json: &str,
            result: &str,
            elapsed_ms: u64,
        ) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let started = now - (elapsed_ms as f64 / 1000.0);
            let params: serde_json::Value = serde_json::from_str(params_json)
                .unwrap_or(serde_json::Value::String(params_json.to_string()));
            let res: serde_json::Value = serde_json::Value::String(result.to_string());
            if let Err(e) = RT.block_on(self.agent.tools.record(name, started, now, &params, &res))
            {
                log::warn!("[agentfs] record_tool({name}/{action}) failed: {e}");
            }
        }
    }
}

#[cfg(not(feature = "agentfs"))]
mod imp {
    use std::fs;
    use std::path::PathBuf;

    fn data_dir() -> PathBuf {
        deepx_types::platform::data_dir().join("agentfs")
    }

    fn ensure_dir(p: &PathBuf) {
        let _ = fs::create_dir_all(p);
    }

    pub struct Inner {
        session_id: String,
    }

    impl Inner {
        pub fn init(session_id: &str) -> Option<Self> {
            let dir = data_dir().join("kv");
            ensure_dir(&dir);
            let dir = data_dir().join("tools");
            ensure_dir(&dir);
            Some(Self {
                session_id: session_id.to_string(),
            })
        }

        pub fn kv_set(&self, key: &str, value: &str) {
            let dir = data_dir().join("kv").join(&self.session_id);
            ensure_dir(&dir);
            let key = key.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
            let path = dir.join(format!("{key}.json"));
            let _ = fs::write(&path, value);
        }

        pub fn kv_get(&self, key: &str) -> Option<String> {
            let key = key.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
            let path = data_dir()
                .join("kv")
                .join(&self.session_id)
                .join(format!("{key}.json"));
            fs::read_to_string(&path).ok()
        }

        pub fn kv_delete(&self, key: &str) {
            let key = key.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
            let path = data_dir()
                .join("kv")
                .join(&self.session_id)
                .join(format!("{key}.json"));
            let _ = fs::remove_file(&path);
        }

        pub fn record_tool(
            &self,
            name: &str,
            action: &str,
            params_json: &str,
            result: &str,
            elapsed_ms: u64,
        ) {
            let dir = data_dir().join("tools");
            ensure_dir(&dir);
            let path = dir.join(format!("{}.jsonl", &self.session_id));
            let record = serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "name": name,
                "action": action,
                "params": params_json,
                "result": result,
                "elapsed_ms": elapsed_ms,
            });
            let line = serde_json::to_string(&record).unwrap_or_default();
            if let Err(e) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
            {
                log::error!("[agentfs] record_tool write failed: {e}");
            }
        }
    }
}

// ── Global bridge ───────────────────────────────────────────────────────

static BRIDGE: LazyLock<Mutex<Option<imp::Inner>>> = LazyLock::new(|| Mutex::new(None));

/// Initialise the global bridge for the given session id.
pub fn init_bridge(session_id: &str) {
    let inner = imp::Inner::init(session_id);
    if inner.is_some() {
        log::info!("[agentfs] bridge initialised for session {session_id}");
    }
    *BRIDGE.lock().unwrap() = inner;
}

// ── Public helpers ──────────────────────────────────────────────────────

pub fn try_kv_set(key: &str, value: &str) {
    if let Some(ref inner) = *BRIDGE.lock().unwrap() {
        inner.kv_set(key, value);
    }
}

pub fn try_kv_get(key: &str) -> Option<String> {
    let bridge = BRIDGE.lock().unwrap();
    bridge.as_ref()?.kv_get(key)
}

pub fn try_kv_delete(key: &str) {
    if let Some(ref inner) = *BRIDGE.lock().unwrap() {
        inner.kv_delete(key);
    }
}

pub fn try_record_tool(name: &str, action: &str, params_json: &str, result: &str, elapsed_ms: u64) {
    if let Some(ref inner) = *BRIDGE.lock().unwrap() {
        inner.record_tool(name, action, params_json, result, elapsed_ms);
    }
}
