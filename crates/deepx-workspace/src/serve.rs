//! `deepx-workspace serve` — HTTP tool service.
//!
//! Runs the tool registry as a loopback HTTP service so the daemon (or any
//! host) can execute tools out-of-process without embedding the tool runtime.
//!
//! Protocol v1 (transport-neutral, Bearer-authenticated):
//!
//! ```text
//! GET  /health          -> 200 {"ok":true,"tools":N,"version":"..."}
//! GET  /tools           -> 200 [{"name","description","parameters"}, ...]
//! POST /execute         -> 200 {"success":bool,"content":"..."}
//!                         400 invalid body / 401 bad token / 404 unknown tool
//!                         500 execution failure
//!   body: {"session_id","workspace","name","args",
//!          "action"?, "call_id"?, "timeout_secs"?}
//! auth:  Authorization: Bearer <token> on every endpoint
//! ```
//!
//! Execution model:
//! - The tool runtime keeps **process-global** session/workspace state
//!   (`runtime::set_context`, `workspace::set_process_workspace`), so tool
//!   calls are serialized on one dedicated worker thread via a bounded queue.
//! - Control endpoints (`/health`, `/tools`) stay responsive on the HTTP
//!   thread pool and never block behind a running tool.
//! - The service trusts its caller (daemon) for authorization: the host is
//!   responsible for permission decisions before dispatch. `permission_level`
//!   is fixed at the CLI level (4) inside this process.
//! - Tool output is bounded by the tool handlers themselves (exec truncation,
//!   apply_patch summaries); the response never streams unbounded body text.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Bounded queue capacity for pending executions (backpressure beyond this
/// rejects with 429 instead of unbounded buffering).
const EXECUTE_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Clone, Serialize)]
struct HealthResponse {
    ok: bool,
    tools: usize,
    version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct ToolInfo {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ExecuteRequest {
    session_id: String,
    workspace: String,
    name: String,
    #[serde(default)]
    action: String,
    #[serde(default = "default_args")]
    args: serde_json::Value,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn default_args() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

#[derive(Debug, Serialize)]
struct ExecuteResponse {
    success: bool,
    content: String,
}

/// Job handed to the serial execution worker.
struct ExecuteJob {
    request: ExecuteRequest,
    respond: mpsc::Sender<ExecuteResponse>,
}

fn authorized(request: &tiny_http::Request, token: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|candidate| candidate == token)
}

fn json_response(
    status: tiny_http::StatusCode,
    payload: &impl Serialize,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let len = body.len();
    let headers = vec![
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
    ];
    tiny_http::Response::new(
        status,
        headers,
        std::io::Cursor::new(body),
        Some(len),
        None,
    )
}

fn text_response(
    status: tiny_http::StatusCode,
    text: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = text.as_bytes().to_vec();
    let len = body.len();
    let headers = vec![
        tiny_http::Header::from_bytes(
            &b"Content-Type"[..],
            &b"text/plain; charset=utf-8"[..],
        )
        .unwrap(),
    ];
    tiny_http::Response::new(
        status,
        headers,
        std::io::Cursor::new(body),
        Some(len),
        None,
    )
}

fn handle_execute(
    mut request: tiny_http::Request,
    _token: &str,
    tx: &mpsc::SyncSender<ExecuteJob>,
    next_id: &AtomicU64,
) {
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() {
        let _ = request.respond(text_response(tiny_http::StatusCode(400), "read body failed"));
        return;
    }
    let mut parsed: ExecuteRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(error) => {
            let _ = request.respond(text_response(
                tiny_http::StatusCode(400),
                &format!("invalid execute body: {error}"),
            ));
            return;
        }
    };
    if parsed.session_id.is_empty() || parsed.workspace.is_empty() || parsed.name.is_empty() {
        let _ = request.respond(text_response(
            tiny_http::StatusCode(400),
            "session_id, workspace, and name are required",
        ));
        return;
    }
    if crate::runtime::all_tools().iter().all(|def| def.function.name != parsed.name) {
        let _ = request.respond(text_response(
            tiny_http::StatusCode(404),
            &format!("unknown tool: {}", parsed.name),
        ));
        return;
    }
    if parsed.call_id.is_empty() {
        parsed.call_id = format!("serve_{}", next_id.fetch_add(1, Ordering::SeqCst));
    }
    let (respond, received) = mpsc::channel();
    match tx.send(ExecuteJob {
        request: parsed,
        respond,
    }) {
        Ok(()) => {
            // Wait for the serial worker. No timeout: tool duration is the
            // contract; control endpoints are served on other threads.
            let response = received.recv().unwrap_or(ExecuteResponse {
                success: false,
                content: "[ERROR] workspace execution worker unavailable".into(),
            });
            let _ = request.respond(json_response(tiny_http::StatusCode(200), &response));
        }
        Err(_) => {
            let _ = request.respond(text_response(
                tiny_http::StatusCode(429),
                "execution queue full; retry later",
            ));
        }
    }
}

/// Run the tool service until the process is terminated.
///
/// Binds `host:port` (port 0 picks an ephemeral port), prints the ready line
/// `DEEPX_WORKSPACE_READY <host>:<port>` to stdout for the spawning daemon,
/// then serves requests.
pub fn serve(host: &str, port: u16, token: &str) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    let server = tiny_http::Server::http(addr.as_str()).map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = server.server_addr().to_string();

    crate::runtime::init_tools("workspace-serve", &[], vec![]);
    crate::runtime::set_context("workspace-serve", 4);
    let default_workspace = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    crate::workspace::set_process_workspace(&default_workspace);

    // Serial execution worker: tool runtime state is process-global.
    let (tx, rx) = mpsc::sync_channel::<ExecuteJob>(EXECUTE_QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("workspace-executor".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                crate::runtime::set_context(&job.request.session_id, 4);
                crate::workspace::set_process_workspace(&job.request.workspace);
                let args = serde_json::to_string(&job.request.args).unwrap_or_else(|_| "{}".into());
                // 工具 panic 防护（Q: 工具 panic 后 daemon 怎么拿到结果？）：
                // catch_unwind 捕获 handler panic → 返回结构化错误结果，
                // executor 线程存活，serve 继续服务。panic 消息回传 LLM，
                // 不静默丢弃也不杀死服务。
                let name = job.request.name.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::execution::execute_with_context(
                        &job.request.name,
                        &job.request.action,
                        &args,
                        &job.request.call_id,
                        None,
                    )
                }))
                .unwrap_or_else(|payload| {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    crate::execution::ToolExecResult {
                        content: format!("[ERROR] tool '{name}' panicked: {message}"),
                        success: false,
                        meta: crate::ToolExecMeta {
                            name: name.clone(),
                            elapsed_ms: 0,
                            output_size: 0,
                            success: false,
                            args_summary: String::new(),
                        },
                        code_delta: None,
                        skill_effects: Vec::new(),
                    }
                });
                let _ = job.respond.send(ExecuteResponse {
                    success: result.success,
                    content: result.content,
                });
            }
            // channel 断开（唯一 sender 是 HTTP 线程持有的 Arc<tx>，只有
            // 在 serve 关闭路径才会全断）→ 异常退出，让 supervisor 重启。
            // 正常 shutdown 由外部 taskkill 完成，不会走到这里。
            log::error!("[workspace] executor thread ended unexpectedly; exiting for supervisor restart");
            std::process::exit(70);
        })
        .map_err(|e| format!("spawn executor thread: {e}"))?;

    // Ready line: the daemon reads this from stdout to learn the port.
    println!("DEEPX_WORKSPACE_READY {bound}");
    let _ = std::io::stdout().flush();
    log::info!("deepx-workspace serve listening on {bound}");

    let next_id = Arc::new(AtomicU64::new(1));
    let tx = Arc::new(tx);
    for request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let token = token.to_string();
        let tx = tx.clone();
        let next_id = next_id.clone();
        std::thread::Builder::new()
            .name("workspace-http".into())
            .spawn(move || {
                if !authorized(&request, &token) {
                    let _ = request.respond(text_response(tiny_http::StatusCode(401), "unauthorized"));
                    return;
                }
                let path = url.split('?').next().unwrap_or("");
                match (method.as_str(), path) {
                    ("GET", "/health") => {
                        let tools = crate::runtime::all_tools().len();
                        let _ = request.respond(json_response(
                            tiny_http::StatusCode(200),
                            &HealthResponse {
                                ok: true,
                                tools,
                                version: env!("CARGO_PKG_VERSION"),
                            },
                        ));
                    }
                    ("GET", "/tools") => {
                        let tools: Vec<ToolInfo> = crate::runtime::all_tools()
                            .into_iter()
                            .map(|def| ToolInfo {
                                name: def.function.name.clone(),
                                description: def.function.description.clone(),
                                parameters: def.function.parameters.clone(),
                            })
                            .collect();
                        let _ = request.respond(json_response(tiny_http::StatusCode(200), &tools));
                    }
                    ("POST", "/execute") => {
                        handle_execute(request, &token, &tx, &next_id);
                    }
                    _ => {
                        let _ = request.respond(text_response(tiny_http::StatusCode(404), "not found"));
                    }
                }
            })
            .map_err(|e| log::warn!("spawn http thread: {e}"))
            .ok();
    }
    Ok(())
}
