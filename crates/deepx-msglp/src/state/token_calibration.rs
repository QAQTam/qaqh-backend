use std::collections::{HashMap, VecDeque};

use deepx_types::{Message, ToolDef};

const MAX_CALIBRATORS: usize = 32;
const MAX_SAMPLES: usize = 16;
const MIN_SAMPLE_TOKENS: u64 = 32;
const MIN_ACCEPTED_RATIO: f64 = 0.5;
const MAX_ACCEPTED_RATIO: f64 = 2.5;
const MIN_CALIBRATED_SAMPLES: usize = 6;
const MIN_CALIBRATED_SCALE: f64 = 0.85;
const COLD_START_MARGIN_PERCENT: u64 = 10;
const CALIBRATED_MARGIN_PERCENT: u64 = 5;
const MIN_COLD_START_MARGIN: u64 = 256;
const MIN_CALIBRATED_MARGIN: u64 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestTokenEstimate {
    pub raw_tokens: u64,
    pub predicted_tokens: u64,
    pub upper_bound_tokens: u64,
    pub sample_count: usize,
}

#[derive(Debug, Default)]
struct CalibrationState {
    ratios: VecDeque<f64>,
    positive_residuals: VecDeque<u64>,
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
    pub fn estimate(&self, fingerprint: &str, raw_tokens: u64) -> RequestTokenEstimate {
        self.states.get(fingerprint).map_or_else(
            || CalibrationState::default().estimate(raw_tokens),
            |state| state.estimate(raw_tokens),
        )
    }

    pub fn observe(&mut self, fingerprint: &str, raw_tokens: u64, observed_tokens: u64) -> bool {
        if !self.states.contains_key(fingerprint)
            && self.states.len() >= MAX_CALIBRATORS
            && let Some(expired) = self.states.keys().next().cloned()
        {
            self.states.remove(&expired);
        }
        self.states
            .entry(fingerprint.to_string())
            .or_default()
            .observe(raw_tokens, observed_tokens)
    }
}

/// Estimate the complete semantic request surface that is about to be sent.
///
/// Provider-reported usage later calibrates this value, but never replaces it
/// as the auto-compaction decision source.
pub(crate) fn estimate_prepared_request_tokens(
    messages: &[Message],
    tools: Option<&[ToolDef]>,
) -> u64 {
    let serialized = serde_json::to_string(&(messages, tools)).unwrap_or_default();
    u64::from(deepx_types::count_tokens(&serialized)).max(1)
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
        let estimate = calibrator.estimate("session/provider/model", 1_000);
        assert_eq!(estimate.predicted_tokens, 1_000);
        assert_eq!(estimate.upper_bound_tokens, 1_256);
        assert_eq!(estimate.sample_count, 0);
    }

    #[test]
    fn learns_a_provider_ratio_without_cross_fingerprint_leakage() {
        let mut calibrator = SessionTokenCalibrator::default();
        for raw in [1_000, 2_000, 3_000, 4_000] {
            assert!(calibrator.observe("provider-a", raw, raw * 6 / 5));
        }

        let learned = calibrator.estimate("provider-a", 5_000);
        assert_eq!(learned.predicted_tokens, 6_000);
        assert!(learned.upper_bound_tokens >= learned.predicted_tokens);

        let isolated = calibrator.estimate("provider-b", 5_000);
        assert_eq!(isolated.predicted_tokens, 5_000);
        assert_eq!(isolated.sample_count, 0);
    }

    #[test]
    fn rejects_unusable_or_extreme_samples() {
        let mut calibrator = SessionTokenCalibrator::default();
        assert!(!calibrator.observe("provider", 1_000, 0));
        assert!(!calibrator.observe("provider", 1_000, 10_000));
        assert_eq!(calibrator.estimate("provider", 1_000).sample_count, 0);
    }

    #[test]
    fn downward_adjustment_needs_enough_samples_and_stays_bounded() {
        let mut calibrator = SessionTokenCalibrator::default();
        for _ in 0..5 {
            assert!(calibrator.observe("provider", 1_000, 600));
        }
        assert_eq!(calibrator.estimate("provider", 1_000).predicted_tokens, 1_000);

        assert!(calibrator.observe("provider", 1_000, 600));
        let learned = calibrator.estimate("provider", 1_000);
        assert_eq!(learned.predicted_tokens, 850);
        assert!(learned.upper_bound_tokens >= 900);
    }

    #[test]
    fn prepared_request_estimate_includes_tools() {
        let messages = vec![Message::user("hello")];
        let tool = ToolDef {
            call_type: "function".into(),
            function: deepx_types::ToolFunction {
                name: "large_tool".into(),
                description: "A deliberately verbose tool description ".repeat(20),
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let without_tools = estimate_prepared_request_tokens(&messages, None);
        let with_tools = estimate_prepared_request_tokens(&messages, Some(&[tool]));
        assert!(with_tools > without_tools);
    }
}
