use std::collections::BTreeSet;

use qaqh_session::session_fact_v2::{
    BoundedText, FactPayload, ListOutput, RecoveryAction, RecoveryToolCompletion, SessionFact,
    ValidationError,
};
use serde_json::Value;

const PAYLOAD_FIXTURES: &str = include_str!("fixtures/session_fact_v2/payloads.jsonl");
const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");

fn payload_lines() -> Vec<&'static str> {
    PAYLOAD_FIXTURES
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn payload_kind(value: &Value) -> &str {
    value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

mod session_fact_v2 {
    use super::*;

    mod envelope {
        use super::*;

        #[test]
        fn roundtrip() -> Result<(), Box<dyn std::error::Error>> {
            let line = ENVELOPE_FIXTURE.trim();
            let expected: Value = serde_json::from_str(line)?;
            let fact: SessionFact = serde_json::from_str(line)?;
            fact.validate()?;

            let serialized = serde_json::to_string(&fact)?;
            let actual: Value = serde_json::from_str(&serialized)?;
            assert_eq!(actual, expected);

            let reparsed: SessionFact = serde_json::from_str(&serialized)?;
            assert_eq!(reparsed, fact);
            Ok(())
        }
    }

    mod payload {
        use super::*;

        #[test]
        fn all_variants_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
            let lines = payload_lines();
            assert_eq!(lines.len(), 22);

            for line in lines {
                let expected: Value = serde_json::from_str(line)?;
                let payload: FactPayload = serde_json::from_str(line)?;
                payload.validate()?;

                let serialized = serde_json::to_value(&payload)?;
                assert_eq!(serialized, expected, "serialized payload changed");

                let reparsed: FactPayload = serde_json::from_value(serialized)?;
                assert_eq!(reparsed, payload);
            }
            Ok(())
        }
    }

    mod golden {
        use super::*;

        #[test]
        fn all_variants() -> Result<(), Box<dyn std::error::Error>> {
            let expected_kinds = [
                "session_created",
                "input_accepted",
                "turn_started",
                "model_round_started",
                "assistant_block_sealed",
                "tool_call_declared",
                "tool_intent",
                "tool_finished",
                "interaction_requested",
                "interaction_resolved",
                "interaction_expired",
                "turn_finished",
                "turn_interrupted",
                "session_recovered",
                "compaction_applied",
                "session_metadata_changed",
                "session_title_changed",
                "session_deleted",
                "workspace_resource_changed",
                "subagent_spawned",
                "subagent_finished",
                "inter_agent_communication",
            ];

            let mut actual_kinds = BTreeSet::new();
            for line in payload_lines() {
                let value: Value = serde_json::from_str(line)?;
                let kind = payload_kind(&value);
                assert!(!kind.is_empty(), "fixture is missing kind");
                actual_kinds.insert(kind.to_owned());

                let payload: FactPayload = serde_json::from_str(line)?;
                assert_eq!(serde_json::to_value(&payload)?, value);
            }

            assert_eq!(actual_kinds.len(), expected_kinds.len());
            for kind in expected_kinds {
                assert!(actual_kinds.contains(kind), "missing golden kind: {kind}");
            }

            let bounded = BoundedText {
                text: "hello".to_owned(),
                original_chars: 5,
                truncated: false,
            };
            let list = ListOutput {
                items: vec!["a".to_owned(), "b".to_owned()],
                total: 2,
                returned: 2,
                next_cursor: None,
                truncated: false,
            };
            assert_eq!(serde_json::to_value(&bounded)?["text"], "hello");
            assert_eq!(serde_json::to_value(&list)?["returned"], 2);
            assert!(serde_json::to_value(&list)?.get("next_cursor").is_none());

            let completion: RecoveryToolCompletion = serde_json::from_str(
                r#"{"call_id":"call_01J00000000000000000000000","terminal_status":"denied","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":false,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000007","recovery_input_fingerprint":"sha256:8888888888888888888888888888888888888888888888888888888888888888"},"finished_at_ms":1}"#,
            )?;
            let completion_value = serde_json::to_value(&completion)?;
            for field in [
                "execution_id",
                "output_ref",
                "error",
                "evidence_ref",
                "evidence_fact_seq",
                "evidence_event_id",
            ] {
                assert!(
                    completion_value.get(field).is_none(),
                    "None field was serialized: {field}"
                );
            }

            let action = RecoveryAction::ToolFinished { completion };
            let action_value = serde_json::to_value(&action)?;
            assert_eq!(action_value["kind"], "tool_finished");
            assert!(action_value.get("completion").is_some());
            assert!(action_value.get("data").is_none());
            Ok(())
        }
    }

    mod validation {
        use super::*;

        #[test]
        fn recovery_completion_rejects_backgrounded() -> Result<(), Box<dyn std::error::Error>> {
            let action: RecoveryAction = serde_json::from_str(
                r#"{"kind":"tool_finished","completion":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"backgrounded","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":false,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000007","recovery_input_fingerprint":"sha256:8888888888888888888888888888888888888888888888888888888888888888"},"finished_at_ms":1}}"#,
            )?;
            assert!(matches!(
                action.validate(),
                Err(ValidationError::InvalidField {
                    field: "terminal_status",
                    ..
                })
            ));
            Ok(())
        }

        #[test]
        fn reconciled_requires_evidence() -> Result<(), Box<dyn std::error::Error>> {
            let missing_evidence: FactPayload = serde_json::from_str(
                r#"{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"succeeded","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":true,"finished_at_ms":1}}"#,
            )?;
            assert!(matches!(
                missing_evidence.validate(),
                Err(ValidationError::InvalidField {
                    field: "reconciled",
                    ..
                })
            ));

            let partial_pair: FactPayload = serde_json::from_str(
                r#"{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"succeeded","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":true,"evidence_fact_seq":1,"finished_at_ms":1}}"#,
            )?;
            assert!(matches!(
                partial_pair.validate(),
                Err(ValidationError::InvalidField {
                    field: "evidence_fact_seq",
                    ..
                })
            ));

            let evidence_ref: FactPayload = serde_json::from_str(
                r#"{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"succeeded","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":true,"evidence_ref":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","finished_at_ms":1}}"#,
            )?;
            evidence_ref.validate()?;

            let evidence_pair: FactPayload = serde_json::from_str(
                r#"{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"succeeded","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":true,"evidence_fact_seq":1,"evidence_event_id":"01J00000000000000000000007","finished_at_ms":1}}"#,
            )?;
            evidence_pair.validate()?;

            let recovery_missing_evidence: RecoveryAction = serde_json::from_str(
                r#"{"kind":"tool_finished","completion":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"succeeded","metrics":{"started_at_ms":1,"finished_at_ms":1,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":true,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000007","recovery_input_fingerprint":"sha256:8888888888888888888888888888888888888888888888888888888888888888"},"finished_at_ms":1}}"#,
            )?;
            assert!(matches!(
                recovery_missing_evidence.validate(),
                Err(ValidationError::InvalidField {
                    field: "reconciled",
                    ..
                })
            ));
            Ok(())
        }

        #[test]
        fn envelope_invariants() -> Result<(), Box<dyn std::error::Error>> {
            let fact: SessionFact = serde_json::from_str(ENVELOPE_FIXTURE.trim())?;
            fact.validate()?;

            let mut invalid_schema = fact.clone();
            invalid_schema.schema.version = 3;
            assert!(matches!(
                invalid_schema.validate(),
                Err(ValidationError::InvalidSchema { field: "version" })
            ));

            let mut invalid_sequence = fact.clone();
            invalid_sequence.fact_seq = 0;
            assert!(matches!(
                invalid_sequence.validate(),
                Err(ValidationError::InvalidFactSeq { fact_seq: 0 })
            ));

            let mut invalid_event_id = fact.clone();
            invalid_event_id.event_id.0 = "not-a-ulid".to_owned();
            assert!(matches!(
                invalid_event_id.validate(),
                Err(ValidationError::InvalidId {
                    field: "event_id",
                    ..
                })
            ));

            let unknown_kind =
                serde_json::from_str::<FactPayload>(r#"{"kind":"future_kind","data":{}}"#);
            assert!(unknown_kind.is_err());

            let unknown_field = serde_json::from_str::<FactPayload>(
                r#"{"kind":"session_created","data":{"created_at_ms":1,"cwd":"/workspace","model":"m","schema_caps":[],"future_field":true}}"#,
            )?;
            unknown_field.validate()?;

            let mut missing_inline_or_ref = fact;
            missing_inline_or_ref.payload = serde_json::from_str(
                r#"{"kind":"input_accepted","data":{"input_id":"input_01J00000000000000000000000","input_kind":"user_text","input_purpose":"trigger_turn","attachments":[],"actor":{"kind":"user","id":"local"}}}"#,
            )?;
            assert!(matches!(
                missing_inline_or_ref.validate(),
                Err(ValidationError::InvalidField {
                    field: "content_ref",
                    ..
                })
            ));
            Ok(())
        }
    }
}
