//! 捕获 gate 真正发出的 HTTP 请求体（wire 级测试装置）。
//!
//! 为什么需要：上下文级断言（`Vec<Message>`）看不到 gate 的投影与序列化，
//! 而前缀缓存吃的是**线上字节**。这里用一个本地假 provider 收请求体，让
//! `qaqh_gate::chat_stream` 走完整生产路径（投影 → SDK 归一 → 序列化 → HTTP）。
//!
//! 两个测试二进制（chat / responses 各一个）共享本模块；`SessionManager::init`
//! 是进程级 OnceLock，所以初始化只由调用方在各自进程里做一次。

#![allow(dead_code)] // 两个测试二进制各自只用到其中一部分

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use qaqh_config::Config;
use qaqh_gate::ProviderConfig;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::state::lifecycle::{create_session, init_session};
use qaqh_types::message::{ContentBlock, Message};

/// chat completions 的最小 SSE：一帧正文 + 终帧 + `[DONE]`。
///
/// 少了正文帧会被 gate 的空流重试路径当成失败重发，请求数就不止预期了。
pub const CHAT_SSE: &str = concat!(
    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Responses API 的最小 SSE：正文增量 + 终态（带 usage，含缓存命中字段）。
///
/// 终态 `response.output` 必须带 message 条目，否则 SDK 走「终态缺条目」的
/// 回填路径（`responses_sdk.rs` 的 gap 处理），会让事件数量依赖实现细节。
pub const RESPONSES_SSE: &str = concat!(
    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"model\":\"deepseek-chat\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"total_tokens\":11,\"input_tokens_details\":{\"cached_tokens\":8}}}}\n\n",
    "data: [DONE]\n\n",
);

/// 被测的 wire。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// OpenAI Chat Completions。
    Chat,
    /// OpenAI Responses。
    Responses,
}

/// 收请求体、按路径回固定 SSE 的本地 provider。
pub struct CaptureProvider {
    port: u16,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl CaptureProvider {
    pub fn start() -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind mock provider");
        let port = server
            .server_addr()
            .to_ip()
            .expect("mock provider must bind a TCP address")
            .port();
        let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        // 线程随测试进程退出，无需 join：端口是按需分配的，不会串扰其它测试。
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let responses = request.url().contains("/responses");
                let mut body = Vec::new();
                let _ = request.as_reader().read_to_end(&mut body);
                sink.lock().expect("body sink").push(body);
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                        .expect("static header");
                let payload = if responses { RESPONSES_SSE } else { CHAT_SSE };
                let _ =
                    request.respond(tiny_http::Response::from_string(payload).with_header(header));
            }
        });
        Self { port, bodies }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn request_count(&self) -> usize {
        self.bodies.lock().expect("body sink").len()
    }

    pub fn last_body(&self) -> Vec<u8> {
        self.bodies
            .lock()
            .expect("body sink")
            .last()
            .cloned()
            .expect("provider received a request")
    }
}

/// 每个测试二进制独占一个临时 data root（进程 pid + 标签）。
pub fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-restart-request-body-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp data root");
    root
}

/// 钉住 data root 并初始化 `SessionManager`（**每个测试进程只调用一次**）。
pub fn init_data_root(root: &Path) {
    unsafe { std::env::set_var("QAQH_DATA_DIR", root) };
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
}

/// 一个"生命周期"：等价于 daemon 重启后 resume 指定会话。
pub fn resume_agent(root: &Path, ws: &str, session_id: &str) -> AgentState {
    unsafe { std::env::set_var("QAQH_DATA_DIR", root) };
    let mut agent = AgentState::init("restart-request-body-test", Config::default());
    agent.ephemeral = false;
    qaqh_workspace::set_workspace(ws);
    assert!(
        init_session(&mut agent, Some(session_id)),
        "resume must succeed for seed {session_id}"
    );
    agent
}

/// 新建会话生命周期（首个生命周期，seed 由 create 路径分配）。
pub fn create_agent(root: &Path, ws: &str) -> AgentState {
    unsafe { std::env::set_var("QAQH_DATA_DIR", root) };
    let mut agent = AgentState::init("restart-request-body-test", Config::default());
    agent.ephemeral = false;
    qaqh_workspace::set_workspace(ws);
    create_session(&mut agent);
    assert!(
        !agent.session.session_id.is_empty(),
        "create session must assign a seed"
    );
    agent
}

/// 首轮：user → assistant(tool_use) → tool result。带真实工具环，`tools`
/// 字段与 tool_call 派生都在请求体里落地。
pub fn session_turn_1(agent: &mut AgentState) {
    agent.msg.push_user("hello, look up the deploy color");
    let _ = agent.msg.push_assistant(Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "lookup".into(),
            input: serde_json::json!({ "key": "deploy-color" }),
        }],
    });
    let _ = agent
        .msg
        .push_tool_result("call-1", "value(deploy-color) = azure-falcon-42", true);
}

/// 被测 provider。字段刻意取 DeepSeek 的形态：chat 用
/// `PromptCacheHitTokens` + 流式 usage，responses 用默认 compat。
pub fn provider_for(provider: &CaptureProvider, wire: Wire) -> ProviderConfig {
    match wire {
        Wire::Chat => ProviderConfig::chat(
            &provider.base_url(),
            "test-key",
            "deepseek-chat",
            Some(qaqh_types::UserSendMode::Body),
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        )
        .with_stream_usage(true),
        Wire::Responses => {
            ProviderConfig::responses(&provider.base_url(), "test-key", "deepseek-chat", None)
        }
    }
}

/// 把当前上下文真的发一次（生产 gate 路径），返回 provider 收到的请求体。
pub fn capture_body(
    agent: &mut AgentState,
    provider: &CaptureProvider,
    session_id: &str,
    wire: Wire,
) -> Vec<u8> {
    let config = provider_for(provider, wire);
    let before = provider.request_count();
    let messages = agent.build_context();
    let tools = agent.tool_defs.clone();
    qaqh_gate::chat_stream(
        &config,
        messages,
        Some(tools),
        16_384,
        Some("high".to_string()),
        Some(session_id.to_string()),
        None,
        &mut |_| {},
    )
    .expect("gate stream against the mock provider must succeed");
    assert_eq!(
        provider.request_count(),
        before + 1,
        "exactly one wire request per capture (retries would hide a broken body)"
    );
    provider.last_body()
}

pub fn body_json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("wire request body must be JSON")
}

/// chat 的 `messages` 数组。
pub fn messages(body: &serde_json::Value) -> &Vec<serde_json::Value> {
    body["messages"]
        .as_array()
        .expect("chat wire body must carry a messages array")
}

/// responses 的 `input` 数组。
pub fn input_items(body: &serde_json::Value) -> &Vec<serde_json::Value> {
    body["input"]
        .as_array()
        .expect("responses wire body must carry an input array")
}
