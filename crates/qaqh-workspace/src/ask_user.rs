use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ToolRisk;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolExecutionError, ToolHeader,
    ToolMeta, ToolProjection, TypedTool,
};

pub const ASK_MAX_INPUT_BYTES: usize = 16 * 1024;
pub const ASK_MAX_QUESTIONS: usize = 10;
pub const ASK_MAX_OPTIONS_PER_QUESTION: usize = 10;
pub const ASK_MAX_QUESTION_CHARS: usize = 1_000;
pub const ASK_MAX_OPTION_CHARS: usize = 200;

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

/// Parse the strict freeform `ASK v1` grammar; malformed prose is never
/// guessed at or repaired.
pub fn parse_ask_input(input: &str) -> Result<NormalizedAsk, AskUserError> {
    let invalid = |line: usize, reason: String| AskUserError {
        code: "ask_format_invalid",
        message: format!("line {line}: {reason}"),
    };
    if input.len() > ASK_MAX_INPUT_BYTES {
        return Err(AskUserError {
            code: "ask_input_too_large",
            message: format!("input exceeds {ASK_MAX_INPUT_BYTES} bytes"),
        });
    }
    let mut lines = input.lines().enumerate();
    let Some((header_line, header)) = lines.next() else {
        return Err(invalid(1, "expected `ASK v1` header".into()));
    };
    if header.trim() != "ASK v1" {
        return Err(invalid(header_line + 1, "expected `ASK v1` header".into()));
    }

    let mut questions = Vec::new();
    let mut question: Option<String> = None;
    let mut options = Vec::new();
    let mut options_seen = false;
    let mut allow_custom = true;
    let mut allow_custom_seen = false;
    let mut unique_options = HashSet::new();

    let finish = |questions: &mut Vec<NormalizedAskQuestion>,
                  question: &mut Option<String>,
                  options: &mut Vec<String>,
                  options_seen: &mut bool,
                  allow_custom: &mut bool,
                  allow_custom_seen: &mut bool,
                  unique_options: &mut HashSet<String>,
                  line: usize|
     -> Result<(), AskUserError> {
        let Some(question) = question.take() else {
            return Ok(());
        };
        if *options_seen && options.is_empty() {
            return Err(invalid(line, "`Options:` requires at least one option".into()));
        }
        if options.is_empty() && !*allow_custom {
            return Err(invalid(
                line,
                "`Allow custom: no` requires at least one option".into(),
            ));
        }
        questions.push(NormalizedAskQuestion {
            id: format!("q{}", questions.len() + 1),
            question,
            options: std::mem::take(options),
            allow_custom: *allow_custom,
        });
        *options_seen = false;
        *allow_custom = true;
        *allow_custom_seen = false;
        unique_options.clear();
        if questions.len() > ASK_MAX_QUESTIONS {
            return Err(AskUserError {
                code: "ask_too_many_questions",
                message: format!("maximum is {ASK_MAX_QUESTIONS} questions"),
            });
        }
        Ok(())
    };

    for (line_index, raw) in lines {
        let line_number = line_index + 1;
        let line = raw.trim();
        if line.is_empty() {
            finish(
                &mut questions, &mut question, &mut options, &mut options_seen,
                &mut allow_custom, &mut allow_custom_seen, &mut unique_options, line_number,
            )?;
            continue;
        }
        if let Some(text) = line.strip_prefix("Question: ") {
            if question.is_some() {
                return Err(invalid(line_number, "separate question blocks with a blank line".into()));
            }
            let text = text.trim();
            if text.is_empty() {
                return Err(invalid(line_number, "question text is required".into()));
            }
            if text.chars().count() > ASK_MAX_QUESTION_CHARS {
                return Err(invalid(line_number, format!("question exceeds {ASK_MAX_QUESTION_CHARS} characters")));
            }
            question = Some(text.to_owned());
            continue;
        }
        if question.is_none() {
            return Err(invalid(line_number, "expected `Question: ...`".into()));
        }
        if line == "Options:" {
            if options_seen || allow_custom_seen {
                return Err(invalid(line_number, "`Options:` must appear once before `Allow custom:`".into()));
            }
            options_seen = true;
            continue;
        }
        if let Some(text) = line.strip_prefix("- ") {
            if !options_seen || allow_custom_seen {
                return Err(invalid(line_number, "options must follow `Options:` and precede `Allow custom:`".into()));
            }
            let text = text.trim();
            if text.is_empty() {
                return Err(invalid(line_number, "option text is required".into()));
            }
            if text.chars().count() > ASK_MAX_OPTION_CHARS {
                return Err(invalid(line_number, format!("option exceeds {ASK_MAX_OPTION_CHARS} characters")));
            }
            if !unique_options.insert(text.to_owned()) {
                return Err(invalid(line_number, format!("duplicate option `{text}`")));
            }
            options.push(text.to_owned());
            if options.len() > ASK_MAX_OPTIONS_PER_QUESTION {
                return Err(invalid(line_number, format!("maximum is {ASK_MAX_OPTIONS_PER_QUESTION} options")));
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("Allow custom: ") {
            if allow_custom_seen {
                return Err(invalid(line_number, "`Allow custom:` may appear once".into()));
            }
            allow_custom = match value {
                "yes" => true,
                "no" => false,
                _ => return Err(invalid(line_number, "expected exactly `Allow custom: yes` or `Allow custom: no`".into())),
            };
            allow_custom_seen = true;
            continue;
        }
        return Err(invalid(line_number, "unknown line; use `Question:`, `Options:`, `- option`, or `Allow custom:`".into()));
    }
    let final_line = input.lines().count().max(1) + 1;
    finish(
        &mut questions, &mut question, &mut options, &mut options_seen,
        &mut allow_custom, &mut allow_custom_seen, &mut unique_options, final_line,
    )?;
    if questions.is_empty() {
        return Err(invalid(final_line, "at least one question is required".into()));
    }
    let mode = if questions.len() == 1 {
        NormalizedAskMode::Single
    } else {
        NormalizedAskMode::Batch
    };
    Ok(NormalizedAsk { mode, questions })
}

/// Extract the model function's single text field before parsing.
pub fn normalize_ask_user(args: &Value) -> Result<NormalizedAsk, AskUserError> {
    let Some(object) = args.as_object() else {
        return Err(AskUserError {
            code: "ask_input_missing",
            message: "expected an object with one string field `input`".into(),
        });
    };
    if object.len() != 1 {
        return Err(AskUserError {
            code: "ask_input_fields_invalid",
            message: "expected exactly one field: `input`".into(),
        });
    }
    let Some(input) = object.get("input").and_then(Value::as_str) else {
        return Err(AskUserError {
            code: "ask_input_missing",
            message: "`input` must be an `ASK v1` string".into(),
        });
    };
    parse_ask_input(input)
}

/// Ask user questions (Ringing interaction).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskArgs {
    /// Complete `ASK v1` document. Load the `qaqh-ask` skill for its grammar.
    pub input: String,
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

pub fn ask_model_input_representation(
) -> crate::tool_api::NativeTextRepresentation {
    crate::tool_api::NativeTextRepresentation::text_tool(
        "input",
        serde_json::json!({}),
        "Ask the user using one complete `ASK v1` document. Each question starts with `Question: `. Separate question blocks with a blank line. Optional `Options:` is followed by `- option` lines; optional `Allow custom: yes|no` follows. Omission means yes. Load the `qaqh-ask` skill for limits and examples. Invalid format is returned to the model for retry and never shown to the user.",
        false,
        true,
    )
}

impl TypedTool for AskTool {
    type Args = AskArgs;
    type Output = AskOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "ask",
            "Ask the user using a complete `ASK v1` text document in `input`. Malformed documents are never shown to the user; the model receives a retryable format error. Load `qaqh-ask` for the grammar.",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            std::time::Duration::ZERO,
        )
        .with_native_text(ask_model_input_representation())
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        _ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let ask = parse_ask_input(&args.input).map_err(|error| {
            ToolExecutionError::Recoverable(
                crate::tool_api::ToolError::invalid_arguments(format!(
                    "ASK_FORMAT_ERROR: {}",
                    error.message
                ))
                .with_retryable(true)
                .with_hint("This ask was not shown to the user. Re-output the complete `ASK v1` document; do not ask the user to fix the format."),
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

    const SINGLE: &str = "ASK v1\nQuestion: Choose A or B?\nOptions:\n- A\n- B\nAllow custom: no";
    const BATCH: &str = "ASK v1\nQuestion: Which architecture?\nOptions:\n- A\n- B\nAllow custom: no\n\nQuestion: What constraints matter?";

    #[test]
    fn typed_ask_registration_and_display_are_same_source() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        assert!(
            manager.builtins.contains_key("ask"),
            "ask must be on the typed execution surface"
        );
        let result = exec_ask_user(&serde_json::json!({
            "input": "ASK v1\nQuestion: Choose?\nOptions:\n- A\n- B\nAllow custom: no"
        }));
        assert!(result.is_success());
        let display = result.display().expect("typed display");
        assert_eq!(display.summary.as_deref(), Some("asked 1 question"));
        assert!(display.body.is_none());
    }

    #[test]
    fn ask_v1_single_question_parses_into_existing_interaction_shape() {
        let ask = parse_ask_input(SINGLE).expect("ask document");
        assert_eq!(ask.mode, NormalizedAskMode::Single);
        assert_eq!(
            ask.questions,
            vec![NormalizedAskQuestion {
                id: "q1".into(),
                question: "Choose A or B?".into(),
                options: vec!["A".into(), "B".into()],
                allow_custom: false,
            }]
        );
    }

    #[test]
    fn ask_v1_batch_generates_stable_ids_and_defaults_custom_to_yes() {
        let ask = parse_ask_input(BATCH).expect("ask document");
        assert_eq!(ask.mode, NormalizedAskMode::Batch);
        assert_eq!(ask.questions.len(), 2);
        assert_eq!(ask.questions[0].id, "q1");
        assert_eq!(ask.questions[1].id, "q2");
        assert!(!ask.questions[0].allow_custom);
        assert!(ask.questions[1].allow_custom);
    }

    #[test]
    fn ask_function_accepts_only_one_string_input_field() {
        let normalized = normalize_ask_user(&serde_json::json!({"input": SINGLE})).unwrap();
        assert_eq!(normalized, parse_ask_input(SINGLE).unwrap());
        assert_eq!(
            normalize_ask_user(&serde_json::json!({"input": SINGLE, "extra": true}))
                .unwrap_err()
                .code,
            "ask_input_fields_invalid"
        );
    }

    #[test]
    fn malformed_format_is_retryable_and_does_not_become_an_interaction() {
        let result = exec_ask_user(&serde_json::json!({"input": "Question: no header"}));
        let error = result.error.as_ref().expect("format error");
        assert_eq!(error.code, "invalid_arguments");
        assert!(error.retryable);
        assert!(error.message.contains("ASK_FORMAT_ERROR"));
        assert!(error.hint.as_deref().unwrap_or_default().contains("not shown"));
    }

    #[test]
    fn malformed_documents_fail_closed() {
        for (text, expected) in [
            ("", "ask_format_invalid"),
            ("ASK v2\nQuestion: Q?", "ask_format_invalid"),
            ("ASK v1\nQuestion: Q?\nOptions:", "ask_format_invalid"),
            ("ASK v1\nQuestion: Q?\nAllow custom: no", "ask_format_invalid"),
            ("ASK v1\nQuestion: Q?\nOptions:\n- A\n- A", "ask_format_invalid"),
            ("ASK v1\nQuestion: Q?\nUnknown: value", "ask_format_invalid"),
            ("ASK v1\nQuestion: Q?\nQuestion: Q2?", "ask_format_invalid"),
        ] {
            assert_eq!(parse_ask_input(text).unwrap_err().code, expected, "{text:?}");
        }
    }

    #[test]
    fn function_schema_is_one_required_string() {
        let schema = crate::tool_api::schema::schema_of::<AskArgs>();
        assert_eq!(schema["required"], serde_json::json!(["input"]));
        assert_eq!(schema["properties"]["input"]["type"], "string");
    }
}
