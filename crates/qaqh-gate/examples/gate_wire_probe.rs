//! 一次性探针（debug）：把 gate 的真实出站请求打到本机 provider，逐帧打印
//! `StreamEvent`，用于定位「上游工具调用无响应」这类 wire 级问题。
//!
//! ```sh
//! QAQH_GATE_PROBE_URL=http://127.0.0.1:18899/v1 \
//! QAQH_GATE_PROBE_WIRE=responses \
//! cargo run -p qaqh-gate --example gate_wire_probe
//! ```
//!
//! 配合 `scripts/wire-capture-proxy.mjs` 使用：把 `QAQH_WIRE_TARGET` 指向真实
//! provider，`QAQH_GATE_PROBE_URL` 指向抓包代理，即可同时拿到「我们发了什么」
//! 与「上游回了什么」。

use qaqh_gate::{ProviderConfig, StreamEvent, chat_stream};
use qaqh_types::{ContentBlock, Message, ToolDef, ToolFunction};
use serde_json::json;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn tools() -> Vec<ToolDef> {
    let mk = |name: &str, desc: &str, params: serde_json::Value| ToolDef {
        call_type: "function".into(),
        function: ToolFunction {
            name: name.into(),
            description: desc.into(),
            parameters: params,
        },
    };
    vec![
        mk(
            "exec",
            "Run a shell command and return stdout/stderr.",
            json!({"type": "object", "properties": {
                "command": {"type": "string", "description": "shell command"},
                "cwd": {"type": ["string", "null"]},
                "timeout_secs": {"type": "integer", "minimum": 1}
            }, "required": ["command"]}),
        ),
        mk(
            "read",
            "Read files with line ranges.",
            json!({"type": "object", "properties": {
                "requests": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "start_line": {"type": "integer"}},
                    "required": ["path"]
                }}
            }}),
        ),
        mk(
            "list_dir",
            "List a directory. Reply with exactly one tool call and no prose.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "depth": {"type": "integer", "default": 1}
            }, "required": ["path"]}),
        ),
    ]
}

fn describe(ev: &StreamEvent) -> String {
    match ev {
        StreamEvent::ContentDelta(t) => {
            format!("ContentDelta {:?}", t.chars().take(40).collect::<String>())
        }
        StreamEvent::ReasoningDelta { kind, text } => {
            format!("ReasoningDelta kind={kind:?} len={}", text.len())
        }
        StreamEvent::ToolCallProgress {
            index,
            id,
            name,
            args_chunk,
        } => {
            format!("ToolCallProgress index={index} id={id:?} name={name:?} args={args_chunk:?}")
        }
        StreamEvent::WebSearchStatus(s) => format!("WebSearchStatus {s}"),
        StreamEvent::UsageUpdate(u) => format!(
            "UsageUpdate prompt={} completion={}",
            u.prompt_tokens, u.completion_tokens
        ),
        StreamEvent::Error { kind, message } => format!("Error kind={kind:?} message={message:?}"),
        StreamEvent::Retrying {
            attempt,
            max_retries,
            delay_secs,
            error,
        } => {
            format!("Retrying attempt={attempt}/{max_retries} delay={delay_secs}s error={error:?}")
        }
        StreamEvent::Done {
            raw_message,
            usage,
            stop_reason,
        } => {
            let blocks: Vec<String> = raw_message
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => format!("text(len={})", text.len()),
                    ContentBlock::Reasoning { reasoning, kind } => {
                        format!("reasoning(len={},kind={kind:?})", reasoning.len())
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        format!("tool_use(id={id:?},name={name:?},input={input})")
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        format!("tool_result({tool_use_id:?})")
                    }
                    ContentBlock::ResponseOutputItem { item } => {
                        format!("roi(type={:?})", item.get("type").and_then(|v| v.as_str()))
                    }
                    other => format!("{other:?}").chars().take(60).collect(),
                })
                .collect();
            format!(
                "Done stop={stop_reason:?} usage={} blocks=[{}]",
                usage.is_some(),
                blocks.join(", ")
            )
        }
    }
}

fn main() {
    let url = env_or("QAQH_GATE_PROBE_URL", "http://127.0.0.1:8787/v1");
    let wire = env_or("QAQH_GATE_PROBE_WIRE", "responses");
    let model = env_or("QAQH_GATE_PROBE_MODEL", "qfmodel");
    let effort = env_or("QAQH_GATE_PROBE_EFFORT", "high");
    let prompt = env_or(
        "QAQH_GATE_PROBE_PROMPT",
        "Call list_dir with path \"/tmp\" right now. Do not write any prose before or after the call.",
    );

    let mut provider = match wire.as_str() {
        "anthropic" => ProviderConfig::anthropic(&url, "probe-key", &model, None),
        _ => ProviderConfig::responses(&url, "probe-key", &model, None),
    };
    provider.retry = Some(qaqh_types::RetrySpec {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
        idle_timeout_secs: 0,
    });

    let messages = vec![
        Message::system(
            "You are QAQ-Harness, an AI coding agent. You act on the workspace through typed tools. Prefer tool calls over narration.",
        ),
        Message::user(&prompt),
    ];

    println!("wire={wire} url={url} model={model} effort={effort}");
    let mut frames = 0usize;
    let result = chat_stream(
        &provider,
        messages,
        Some(tools()),
        4096,
        Some(effort),
        None,
        None,
        &mut |ev| {
            frames += 1;
            println!("  · {}", describe(&ev));
        },
    );
    println!(
        "result={:?} frames={frames}",
        result.as_ref().map(|_| "Ok").map_err(|e| e.to_string()),
    );
}
