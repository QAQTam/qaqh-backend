use std::collections::{HashMap, VecDeque};

use qaqh_types::{Message, ToolDef};

const MAX_CALIBRATORS: usize = 32;
const MAX_SAMPLES: usize = 16;
const MIN_SAMPLE_TOKENS: u64 = 32;
const MIN_ACCEPTED_RATIO: f64 = 0.5;
const MAX_ACCEPTED_RATIO: f64 = 2.5;
const MAX_API_CONTEXT_KEYS: usize = 64;
const MIN_CALIBRATED_SAMPLES: usize = 6;
const MIN_CALIBRATED_SCALE: f64 = 0.85;
const COLD_START_MARGIN_PERCENT: u64 = 10;
const CALIBRATED_MARGIN_PERCENT: u64 = 5;
const MIN_COLD_START_MARGIN: u64 = 256;
const MIN_CALIBRATED_MARGIN: u64 = 128;

/// Conservative fixed per-image token charge used when accounting a request.
///
/// Provider-side image accounting is pixel based: a 1 MiB-class screenshot
/// costs a few thousand tokens (Anthropic ≈ `w*h/750`, OpenAI high detail
/// ≈ `85 + 170·tiles`), while the very same base64 payload counted as prose
/// costs 250k-300k tokens under the character heuristic
/// (BUG-2026-09-16-05 / D-14). Charging this fixed upper bound per image keeps
/// the local estimate on the same order of magnitude as the endpoint without
/// pretending to know the pixel dimensions.
pub(crate) const IMAGE_TOKEN_BUDGET: u64 = 4_096;

/// Placeholder that replaces an inline base64 payload before serialization.
///
/// It keeps the byte length (so the prepared-request key still distinguishes
/// differently sized images) but never the bytes themselves.
fn image_payload_placeholder(byte_len: usize) -> String {
    format!("<inline image payload: {byte_len} bytes>")
}

/// Replace inline image payloads with [`image_payload_placeholder`] and return
/// how many images were replaced.
///
/// Only the two structural positions that carry image bytes are rewritten —
/// [`qaqh_types::ContentBlock::Image::data`] and
/// [`qaqh_types::ToolResult::images`]`[].data`. Base64-looking text inside
/// ordinary string content is deliberately left untouched.
pub(crate) fn redact_image_payloads(messages: &mut [Message]) -> u64 {
    let mut images = 0u64;
    for message in messages.iter_mut() {
        for block in message.content.iter_mut() {
            match block {
                qaqh_types::ContentBlock::Image { data, .. } => {
                    let placeholder = image_payload_placeholder(data.len());
                    *data = placeholder;
                    images += 1;
                }
                qaqh_types::ContentBlock::ToolResult { result, .. } => {
                    for image in result.images.iter_mut() {
                        let placeholder = image_payload_placeholder(image.data.len());
                        image.data = placeholder;
                        images += 1;
                    }
                }
                _ => {}
            }
        }
    }
    images
}

/// Token charge for `images` redacted inline image payloads.
pub(crate) fn image_token_charge(images: u64) -> u64 {
    images.saturating_mul(IMAGE_TOKEN_BUDGET)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestTokenEstimate {
    pub raw_tokens: u64,
    pub predicted_tokens: u64,
    pub upper_bound_tokens: u64,
    pub sample_count: usize,
    /// Exact provider-reported input tokens for this unchanged prepared request.
    /// When present this is the authoritative context size; the heuristic upper
    /// bound remains telemetry only and must not retrigger compaction.
    pub api_context_tokens: Option<u64>,
}

#[derive(Debug, Default)]
struct CalibrationState {
    ratios: VecDeque<f64>,
    positive_residuals: VecDeque<u64>,
    api_context_by_request: HashMap<String, u64>,
    api_context_order: VecDeque<String>,
}

impl CalibrationState {
    fn scale(&self) -> f64 {
        let observed = percentile_f64(&self.ratios, 0.8).unwrap_or(1.0);
        if self.ratios.len() < MIN_CALIBRATED_SAMPLES {
            observed.max(1.0)
        } else {
            observed.max(MIN_CALIBRATED_SCALE)
        }
    }

    fn estimate(&self, raw_tokens: u64) -> RequestTokenEstimate {
        let predicted_tokens = ((raw_tokens as f64) * self.scale()).ceil() as u64;
        let sample_count = self.ratios.len();
        let margin = if sample_count < MIN_CALIBRATED_SAMPLES {
            raw_tokens
                .saturating_mul(COLD_START_MARGIN_PERCENT)
                .div_ceil(100)
                .max(MIN_COLD_START_MARGIN)
        } else {
            let proportional = raw_tokens
                .saturating_mul(CALIBRATED_MARGIN_PERCENT)
                .div_ceil(100);
            percentile_u64(&self.positive_residuals, 0.95)
                .unwrap_or(0)
                .max(proportional)
                .max(MIN_CALIBRATED_MARGIN)
        };
        RequestTokenEstimate {
            raw_tokens,
            predicted_tokens,
            upper_bound_tokens: predicted_tokens.saturating_add(margin),
            sample_count,
            api_context_tokens: None,
        }
    }

    fn observe(&mut self, raw_tokens: u64, observed_tokens: u64) -> bool {
        if raw_tokens < MIN_SAMPLE_TOKENS || observed_tokens == 0 {
            return false;
        }
        let ratio = observed_tokens as f64 / raw_tokens as f64;
        if !(MIN_ACCEPTED_RATIO..=MAX_ACCEPTED_RATIO).contains(&ratio) {
            return false;
        }

        let prior_prediction = ((raw_tokens as f64) * self.scale()).ceil() as u64;
        push_bounded(
            &mut self.positive_residuals,
            observed_tokens.saturating_sub(prior_prediction),
        );
        push_bounded(&mut self.ratios, ratio);
        true
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionTokenCalibrator {
    states: HashMap<String, CalibrationState>,
}

impl SessionTokenCalibrator {
    pub fn estimate(
        &self,
        fingerprint: &str,
        request_key: &str,
        raw_tokens: u64,
    ) -> RequestTokenEstimate {
        let mut estimate = self.states.get(fingerprint).map_or_else(
            || CalibrationState::default().estimate(raw_tokens),
            |state| state.estimate(raw_tokens),
        );
        estimate.api_context_tokens = self
            .states
            .get(fingerprint)
            .and_then(|state| state.api_context_by_request.get(request_key))
            .copied();
        estimate
    }

    pub fn observe(
        &mut self,
        fingerprint: &str,
        request_key: &str,
        raw_tokens: u64,
        observed_tokens: u64,
    ) -> bool {
        if !self.states.contains_key(fingerprint)
            && self.states.len() >= MAX_CALIBRATORS
            && let Some(expired) = self.states.keys().next().cloned()
        {
            self.states.remove(&expired);
        }
        let state = self.states.entry(fingerprint.to_string()).or_default();
        let accepted = state.observe(raw_tokens, observed_tokens);
        // Exact request bindings are trusted only when the usage sample is
        // plausible for the locally serialized shape. Zero/extreme readings
        // may be provider telemetry for a different internal request and must
        // not become the auto-compaction source.
        if accepted {
            if !state.api_context_by_request.contains_key(request_key) {
                if state.api_context_order.len() >= MAX_API_CONTEXT_KEYS
                    && let Some(expired) = state.api_context_order.pop_front()
                {
                    state.api_context_by_request.remove(&expired);
                }
                state.api_context_order.push_back(request_key.to_string());
            }
            state
                .api_context_by_request
                .insert(request_key.to_string(), observed_tokens);
        }
        accepted
    }
}

/// Estimate the complete semantic request surface that is about to be sent.
///
/// Provider-reported usage later calibrates future estimates and becomes the
/// authoritative source when the exact same prepared request shape is seen again.
pub(crate) fn prepared_request_metrics(
    messages: &[Message],
    tools: Option<&[ToolDef]>,
    session_id: Option<&str>,
) -> (u64, String, u64) {
    use std::hash::{Hash, Hasher};

    // Account a redacted copy: inline image bytes are billed by the endpoint
    // per pixel, never per base64 character (BUG-2026-09-16-05 / D-14).
    let mut accounted = messages.to_vec();
    if let Some(session_id) = session_id.filter(|id| !id.is_empty()) {
        qaqh_memwatch::global().record_phase(
            "context.estimate.copy",
            Some(session_id),
            None,
            None,
            None,
        );
    }
    let images = redact_image_payloads(&mut accounted);
    let serialized = serde_json::to_string(&(&accounted, tools)).unwrap_or_default();
    let serialized_bytes = serialized.len() as u64;
    if let Some(session_id) = session_id.filter(|id| !id.is_empty()) {
        qaqh_memwatch::global().update_estimate_json_bytes(session_id, serialized_bytes);
        qaqh_memwatch::global().record_phase(
            "context.estimate.serialized",
            Some(session_id),
            None,
            None,
            Some(serialized_bytes),
        );
    }
    let raw_tokens = u64::from(qaqh_types::count_tokens(&serialized))
        .max(1)
        .saturating_add(image_token_charge(images));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serialized.hash(&mut hasher);
    if let Some(session_id) = session_id.filter(|id| !id.is_empty()) {
        qaqh_memwatch::global().record_phase(
            "context.estimate.complete",
            Some(session_id),
            None,
            None,
            Some(serialized_bytes),
        );
    }
    (
        raw_tokens,
        format!("{:016x}", hasher.finish()),
        serialized_bytes,
    )
}

fn push_bounded<T>(values: &mut VecDeque<T>, value: T) {
    if values.len() >= MAX_SAMPLES {
        values.pop_front();
    }
    values.push_back(value);
}

fn percentile_f64(values: &VecDeque<f64>, quantile: f64) -> Option<f64> {
    let mut sorted = values.iter().copied().collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    percentile_index(sorted.len(), quantile).map(|index| sorted[index])
}

fn percentile_u64(values: &VecDeque<u64>, quantile: f64) -> Option<u64> {
    let mut sorted = values.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    percentile_index(sorted.len(), quantile).map(|index| sorted[index])
}

fn percentile_index(len: usize, quantile: f64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some((((len - 1) as f64) * quantile.clamp(0.0, 1.0)).ceil() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_uses_a_conservative_upper_bound() {
        let calibrator = SessionTokenCalibrator::default();
        let estimate = calibrator.estimate("session/provider/model", "request-a", 1_000);
        assert_eq!(estimate.predicted_tokens, 1_000);
        assert_eq!(estimate.upper_bound_tokens, 1_256);
        assert_eq!(estimate.sample_count, 0);
        assert_eq!(estimate.api_context_tokens, None);
    }

    #[test]
    fn learns_a_provider_ratio_without_cross_fingerprint_leakage() {
        let mut calibrator = SessionTokenCalibrator::default();
        for raw in [1_000, 2_000, 3_000, 4_000] {
            assert!(calibrator.observe("provider-a", &format!("request-{raw}"), raw, raw * 6 / 5,));
        }

        let learned = calibrator.estimate("provider-a", "request-new", 5_000);
        assert_eq!(learned.predicted_tokens, 6_000);
        assert_eq!(learned.api_context_tokens, None);
        assert!(learned.upper_bound_tokens >= learned.predicted_tokens);

        let isolated = calibrator.estimate("provider-b", "request-new", 5_000);
        assert_eq!(isolated.predicted_tokens, 5_000);
        assert_eq!(isolated.sample_count, 0);
    }

    #[test]
    fn rejects_unusable_or_extreme_samples() {
        let mut calibrator = SessionTokenCalibrator::default();
        assert!(!calibrator.observe("provider", "zero", 1_000, 0));
        assert!(!calibrator.observe("provider", "extreme", 1_000, 10_000));
        assert_eq!(
            calibrator
                .estimate("provider", "unseen", 1_000)
                .sample_count,
            0
        );
        assert_eq!(
            calibrator
                .estimate("provider", "extreme", 1_000)
                .api_context_tokens,
            None,
            "rejected telemetry must not bind an exact prepared request"
        );
    }

    #[test]
    fn downward_adjustment_needs_enough_samples_and_stays_bounded() {
        let mut calibrator = SessionTokenCalibrator::default();
        for _ in 0..5 {
            assert!(calibrator.observe("provider", "same", 1_000, 600));
        }
        assert_eq!(
            calibrator
                .estimate("provider", "unseen", 1_000)
                .predicted_tokens,
            1_000
        );

        assert!(calibrator.observe("provider", "same", 1_000, 600));
        let learned = calibrator.estimate("provider", "unseen", 1_000);
        assert_eq!(learned.predicted_tokens, 850);
        assert!(learned.upper_bound_tokens >= 900);
    }

    #[test]
    fn exact_prepared_request_uses_provider_reported_context() {
        let mut calibrator = SessionTokenCalibrator::default();
        assert!(calibrator.observe("provider", "exact-shape", 1_000, 740));

        let same = calibrator.estimate("provider", "exact-shape", 1_000);
        assert_eq!(same.api_context_tokens, Some(740));
        assert!(same.upper_bound_tokens >= same.predicted_tokens);

        let changed = calibrator.estimate("provider", "different-shape", 1_000);
        assert_eq!(changed.api_context_tokens, None);
    }

    #[test]
    fn prepared_request_estimate_includes_tools() {
        let messages = vec![Message::user("hello")];
        let tool = ToolDef {
            call_type: "function".into(),
            function: qaqh_types::ToolFunction {
                name: "large_tool".into(),
                description: "A deliberately verbose tool description ".repeat(20),
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let (without_tools, without_key, _) = prepared_request_metrics(&messages, None, None);
        let (with_tools, with_key, _) = prepared_request_metrics(&messages, Some(&[tool]), None);
        assert!(with_tools > without_tools);
        assert_ne!(with_key, without_key);
    }

    fn image_message(bytes: usize) -> Message {
        let mut message = Message::user("look at this");
        message.content.push(qaqh_types::ContentBlock::image(
            "image/png",
            &"A".repeat(bytes),
        ));
        message
    }

    fn tool_image_message(bytes: usize) -> Message {
        let result = qaqh_types::ToolResult::ok("done").with_image("image/png", "A".repeat(bytes));
        Message::tool_result("call-1", result)
    }

    #[test]
    fn image_payload_bytes_are_not_counted_as_prose() {
        let small = image_message(1_024);
        let huge = image_message(1_048_576);
        let (small_tokens, _, _) =
            prepared_request_metrics(std::slice::from_ref(&small), None, None);
        let (huge_tokens, _, _) = prepared_request_metrics(std::slice::from_ref(&huge), None, None);
        assert!(
            huge_tokens <= small_tokens + 64,
            "image bytes must not grow the estimate linearly: small={small_tokens} huge={huge_tokens}"
        );
        // 1 MiB of inline base64 would be ~300k tokens as prose; the endpoint
        // charges a few thousand.
        assert!(
            huge_tokens < 10_000,
            "a 1 MiB screenshot must stay in the endpoint's order of magnitude: {huge_tokens}"
        );
    }

    #[test]
    fn tool_result_images_use_the_same_fixed_budget() {
        let small = tool_image_message(1_024);
        let huge = tool_image_message(1_048_576);
        let (small_tokens, _, _) =
            prepared_request_metrics(std::slice::from_ref(&small), None, None);
        let (huge_tokens, _, _) = prepared_request_metrics(std::slice::from_ref(&huge), None, None);
        assert!(
            huge_tokens <= small_tokens + 64,
            "ToolResult.images[].data must be redacted too: small={small_tokens} huge={huge_tokens}"
        );
    }

    #[test]
    fn every_image_is_charged_the_fixed_budget() {
        let one = image_message(4_096);
        let twenty: Vec<Message> = (0..20).map(|_| image_message(4_096)).collect();
        let (one_tokens, _, _) = prepared_request_metrics(std::slice::from_ref(&one), None, None);
        let (twenty_tokens, _, _) = prepared_request_metrics(&twenty, None, None);
        assert!(
            twenty_tokens >= one_tokens + 19 * IMAGE_TOKEN_BUDGET,
            "each image must be charged separately: one={one_tokens} twenty={twenty_tokens}"
        );
    }

    #[test]
    fn base64_looking_text_is_not_redacted() {
        let small = Message::user(&"A".repeat(1_000));
        let large = Message::user(&"A".repeat(100_000));
        let (small_tokens, _, _) = prepared_request_metrics(&[small], None, None);
        let (large_tokens, _, _) = prepared_request_metrics(&[large], None, None);
        assert!(
            large_tokens > small_tokens + 10_000,
            "ordinary text must keep its linear cost: small={small_tokens} large={large_tokens}"
        );
    }
}
