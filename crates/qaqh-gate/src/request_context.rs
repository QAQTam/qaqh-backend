/// Explicit task identity for one model request; the runtime owns its lifecycle.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub session_id: String,
    pub request_set_id: String,
    pub round_num: u32,
    pub turn_started_at: Option<i64>,
}

impl RequestContext {
    pub(crate) fn metadata(&self) -> serde_json::Value {
        let mut metadata = serde_json::json!({
            "session_id": self.session_id,
            "request_set_id": self.request_set_id,
            "round_num": self.round_num.to_string(),
        });
        if let Some(started_at) = self.turn_started_at {
            metadata["turn_started_at"] = serde_json::json!(started_at.to_string());
        }
        metadata
    }
}
