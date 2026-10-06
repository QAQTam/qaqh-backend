use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ToolRisk;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolExecutionError, ToolHeader,
    ToolMeta, ToolProjection, TypedTool,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedAskMode {
    Single,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NormalizedAskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub allow_custom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NormalizedAsk {
    pub mode: NormalizedAskMode,
    pub questions: Vec<NormalizedAskQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserError {
    pub code: &'static str,
    pub message: String,
}

pub fn normalize_ask_user(args: &Value) -> Result<NormalizedAsk, AskUserError> {
    let raw_questions = match args.get("questions") {
        Some(Value::Array(questions)) => questions.clone(),
        Some(_) => {
            return Err(AskUserError {
                code: "invalid_questions",
                message: "questions must be an array".into(),
            });
        }
        None => vec![json!({
            "id": "q1",
            "question": args.get("question").and_then(Value::as_str).unwrap_or(""),
            "options": args.get("options").cloned().unwrap_or_else(|| json!([])),
            "allow_custom": args.get("allow_custom").and_then(Value::as_bool).unwrap_or(true),
        })],
    };

    if raw_questions.is_empty() {
        return Err(AskUserError {
            code: "empty_questions",
            message: "at least one question is required".into(),
        });
    }

    let mut ids = HashSet::new();
    let mut questions = Vec::with_capacity(raw_questions.len());
    for (index, raw) in raw_questions.iter().enumerate() {
        let question = raw
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if question.trim().is_empty() {
            return Err(AskUserError {
                code: "missing_question",
                message: format!("questions[{index}].question is required"),
            });
        }

        let id = match raw.get("id") {
            None => format!("q{}", index + 1),
            Some(Value::String(id)) if !id.trim().is_empty() => id.clone(),
            Some(_) => {
                return Err(AskUserError {
                    code: "invalid_question_id",
                    message: format!("questions[{index}].id must be a non-empty string"),
                });
            }
        };
        if !ids.insert(id.clone()) {
            return Err(AskUserError {
                code: "duplicate_question_id",
                message: format!("duplicate question id: {id}"),
            });
        }

        let options = match raw.get("options") {
            None => Vec::new(),
            Some(Value::Array(values)) => {
                let mut options = Vec::with_capacity(values.len());
                for (option_index, value) in values.iter().enumerate() {
                    let Some(option) = value.as_str() else {
                        return Err(AskUserError {
                            code: "invalid_option",
                            message: format!(
                                "question {id} option {option_index} must be a non-empty string"
                            ),
                        });
                    };
                    let option = option.trim();
                    if option.is_empty() {
                        return Err(AskUserError {
                            code: "invalid_option",
                            message: format!(
                                "question {id} option {option_index} must be a non-empty string"
                            ),
                        });
                    }
                    options.push(option.to_string());
                }
                options
            }
            Some(_) => {
                return Err(AskUserError {
                    code: "invalid_options",
                    message: format!("question {id} options must be an array"),
                });
            }
        };
        let mut unique_options = HashSet::new();
        if options
            .iter()
            .any(|option| !unique_options.insert(option.clone()))
        {
            return Err(AskUserError {
                code: "duplicate_option",
                message: format!("question {id} contains duplicate options"),
            });
        }

        let allow_custom = raw
            .get("allow_custom")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if options.is_empty() && !allow_custom {
            return Err(AskUserError {
                code: "unanswerable_question",
                message: format!("question {id} has no options and disallows custom answers"),
            });
        }

        questions.push(NormalizedAskQuestion {
            id,
            question,
            options,
            allow_custom,
        });
    }

    let mode = if questions.len() == 1 {
        NormalizedAskMode::Single
    } else {
        NormalizedAskMode::Batch
    };
    Ok(NormalizedAsk { mode, questions })
}

/// Ask user questions (Ringing interaction).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskArgs {
    /// Questions
    #[serde(default)]
    pub questions: Option<Value>,
    /// Single question (deprecated, use questions)
    #[serde(default)]
    pub question: Option<Value>,
    /// Choices (deprecated)
    #[serde(default)]
    pub options: Option<Value>,
    /// Allow custom (deprecated)
    #[serde(default)]
    pub allow_custom: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AskOutput {
    pub timeis: String,
    pub status: String,
    pub mode: NormalizedAskMode,
    pub questions: Vec<NormalizedAskQuestion>,
}

impl AskOutput {
    fn summary_text(&self) -> String {
        let count = self.questions.len();
        format!(
            "asked {count} question{}",
            if count == 1 { "" } else { "s" }
        )
    }
}

impl ToolProjection for AskOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string()),
        }]
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        ToolDisplay::new(
            ToolHeader::Other {
                label: "ask".to_string(),
            },
            ToolBody::None,
        )
        .with_summary(self.summary_text())
    }
}

pub struct AskTool;

impl TypedTool for AskTool {
    type Args = AskArgs;
    type Output = AskOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "ask",
            "Ask user questions (Ringing interaction).",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            std::time::Duration::ZERO,
        )
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        _ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let mut raw = serde_json::Map::new();
        if let Some(questions) = args.questions {
            raw.insert("questions".to_string(), questions);
        }
        if let Some(question) = args.question {
            raw.insert("question".to_string(), question);
        }
        if let Some(options) = args.options {
            raw.insert("options".to_string(), options);
        }
        if let Some(allow_custom) = args.allow_custom {
            raw.insert("allow_custom".to_string(), allow_custom);
        }
        let ask = normalize_ask_user(&Value::Object(raw)).map_err(|error| {
            crate::file_mutate::mutation_error(
                error.code,
                format!("ask: {}", error.message),
                Some("Fix the ask arguments and retry."),
                json!({}),
            )
        })?;
        Ok(AskOutput {
            timeis: crate::now_utc8(),
            status: "ok".to_string(),
            mode: ask.mode,
            questions: ask.questions,
        })
    }
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(AskTool);
}

/// Compatibility entry retained for existing in-process tests.
#[cfg(test)]
pub(super) fn exec_ask_user(args: &Value) -> crate::ToolResult {
    use crate::file_mutate::ambient_tool_context;
    use crate::tool_api::{ErasedTool, TypedToolAdapter};

    let ctx = ambient_tool_context("ask-compat", std::time::Duration::ZERO);
    TypedToolAdapter::new(AskTool)
        .execute(ctx, args.clone())
        .unwrap_or_else(|fatal| panic!("ask tool fatal: {}", fatal.message))
        .to_tool_result()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ask_registration_and_display_are_same_source() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        assert!(
            manager.builtins.contains_key("ask"),
            "ask must be on the typed execution surface"
        );
        let result = exec_ask_user(&serde_json::json!({
            "question": "Choose?",
            "options": ["A", "B"],
        }));
        assert!(result.is_success());
        let display = result.display().expect("typed display");
        assert_eq!(display.summary.as_deref(), Some("asked 1 question"));
        assert!(display.body.is_none());
    }

    #[test]
    fn old_format_single_question() {
        let args = serde_json::json!({
            "question": "Choose A or B?",
            "options": ["A", "B"],
            "allow_custom": false
        });
        let result = exec_ask_user(&args);
        // Parse the JSON output
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "ok");
        assert!(value.get("user_query").is_none());
        assert_eq!(value["mode"], "single");
        assert_eq!(value["questions"][0]["id"], "q1");
        assert_eq!(value["questions"][0]["question"], "Choose A or B?");
        assert_eq!(
            value["questions"][0]["options"].as_array().unwrap().len(),
            2
        );
        assert_eq!(value["questions"][0]["allow_custom"], false);
    }

    #[test]
    fn new_format_batch_questions() {
        let args = serde_json::json!({
            "questions": [
                { "id": "arch", "question": "Which architecture?", "options": ["A", "B", "C"] },
                { "id": "strat", "question": "Strategy?", "allow_custom": true }
            ]
        });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "ok");
        assert!(value.get("user_query").is_none());
        assert_eq!(value["mode"], "batch");
        assert_eq!(value["questions"].as_array().unwrap().len(), 2);
        assert_eq!(value["questions"][1]["id"], "strat");
        assert_eq!(value["questions"][1]["allow_custom"], true);
    }

    #[test]
    fn auto_id_generation() {
        let args = serde_json::json!({
            "questions": [
                { "question": "Q1?" },
                { "question": "Q2?" },
                { "question": "Q3?" }
            ]
        });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        let qs = value["questions"].as_array().unwrap();
        assert_eq!(qs[0]["id"], "q1");
        assert_eq!(qs[1]["id"], "q2");
        assert_eq!(qs[2]["id"], "q3");
    }

    #[test]
    fn empty_questions_error() {
        let args = serde_json::json!({ "questions": [] });
        let result = exec_ask_user(&args);
        let err = result.error.as_ref().expect("structured error");
        assert_eq!(err.code, "empty_questions");
    }

    #[test]
    fn missing_question_in_array_error() {
        let args = serde_json::json!({
            "questions": [
                { "id": "q1", "question": "Valid?" },
                { "id": "q2" }
            ]
        });
        let result = exec_ask_user(&args);
        let err = result.error.as_ref().expect("structured error");
        assert_eq!(err.code, "missing_question");
    }

    #[test]
    fn old_format_no_options() {
        let args = serde_json::json!({ "question": "What do you think?" });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "ok");
        let q = &value["questions"][0];
        assert_eq!(q["question"], "What do you think?");
        assert!(q["options"].as_array().unwrap().is_empty());
        assert_eq!(q["allow_custom"], true);
    }

    #[test]
    fn multi_question_input_derives_batch_without_mode() {
        let ask = normalize_ask_user(&serde_json::json!({
            "questions": [
                {
                    "id": "arch",
                    "question": "Architecture?",
                    "options": ["A", "B"],
                    "allow_custom": false
                },
                { "question": "Strategy?", "allow_custom": true }
            ]
        }))
        .unwrap();

        assert_eq!(ask.mode, NormalizedAskMode::Batch);
        assert_eq!(ask.questions[0].id, "arch");
        assert_eq!(ask.questions[1].id, "q2");
    }

    #[test]
    fn duplicate_question_ids_are_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "questions": [
                { "id": "same", "question": "First?", "allow_custom": true },
                { "id": "same", "question": "Second?", "allow_custom": true }
            ]
        }))
        .unwrap_err();

        assert_eq!(error.code, "duplicate_question_id");
    }

    #[test]
    fn duplicate_options_are_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "question": "Pick one",
            "options": ["A", "A"],
            "allow_custom": false
        }))
        .unwrap_err();

        assert_eq!(error.code, "duplicate_option");
    }

    #[test]
    fn unanswerable_question_is_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "question": "Blocked",
            "options": [],
            "allow_custom": false
        }))
        .unwrap_err();

        assert_eq!(error.code, "unanswerable_question");
    }

    #[test]
    fn blank_option_is_rejected_before_presenting_the_prompt() {
        let error = normalize_ask_user(&serde_json::json!({
            "question": "Blocked",
            "options": ["   "],
            "allow_custom": false
        }))
        .unwrap_err();

        assert_eq!(error.code, "invalid_option");
    }

    #[test]
    fn explicit_blank_question_id_is_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "questions": [
                { "id": "  ", "question": "Question?", "allow_custom": true }
            ]
        }))
        .unwrap_err();

        assert_eq!(error.code, "invalid_question_id");
    }
}
