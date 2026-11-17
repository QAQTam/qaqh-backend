//! 用户 shell 终端 ws 数据面的端到端（ADR-0028 §D3 裁定=候选 B，切片 4 的验收）。
//!
//! 为什么必须真起 `axum::serve` + 真 ws 客户端：切片 4 的判据是「`shell.open` → ws
//! 订阅（回放+实时裸字节）→ 打字/resize → close」这条链**真通**。`oneshot` 伪造不出
//! 升级之后的双向字节通道，所以数据面用真连接；而**握手前**的拒绝（非升级请求、越权、
//! 借别人的 lease、未知 shell_id）用 `oneshot` 反而更准（不必真升级就能拿到状态码）。
//!
//! shell 是真的（Windows `powershell.exe` / Unix `$SHELL`，经 ConPTY/pty），与
//! `qaqh-runtime` 的 `shell::tests` 同口径：比对文本前先剥 CSI/OSC，因为 ConPTY 会把
//! 回显切成 `…\x1b[37m` + 正文 + `\x1b[25h` 这类片段，序列自己的参数字节也是字母数字
//! —— 不剥就会把明明在场的回显判成丢失（切片 2 踩过，实测串记在那边）。

use super::build_router;
use super::test_state;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use qaqh_ringing::{ShellStreamControl, ShellStreamEvent};
use qaqh_ringing_runtime::Scope;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::util::ServiceExt;

const ADMIN: &str = "test-token";
const CS_HEADER: &str = "x-qaqh-client-session-id";
const MARKER: &str = "qaqhwsprobe";

/// 剥掉 ANSI 控制序列（CSI 到终结字节、OSC 到 BEL），只留可打印文本。
fn plain(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        i += 1; // ESC
        let Some(&kind) = bytes.get(i) else { break };
        if kind == b'[' || kind == b']' {
            let csi = kind == b'[';
            i += 1;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if csi {
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                } else if byte == 0x07 {
                    break;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

/// 只留字母数字：回显/输出比对不受换行、提示符与残留参数位影响。
fn visible(bytes: &[u8]) -> String {
    plain(bytes)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json body: {error} / {bytes:?}"))
}

/// 建一条 lease 并返回 `(client_session_id, 全响应)`。admin 走自报 instance，
/// device 的 owner 由 token 反推（`v2.rs::handle_open_v2`），所以 device 那一路
/// 传进来的 instance 只是诊断字段。
async fn open_lease(app: &Router, token: &str, instance: &str) -> (String, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/ringing/v2/clients/open")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "schema": qaqh_ringing::RINGING_SCHEMA,
                "version": qaqh_ringing::RINGING_V2_VERSION,
                "client_instance_id": instance,
                "command_contract_revision": qaqh_ringing::COMMAND_CONTRACT_REVISION,
                "session_contract_revision": qaqh_ringing::SESSION_CONTRACT_REVISION,
            })
            .to_string(),
        ))
        .expect("open request");
    let response = app.clone().oneshot(request).await.expect("open oneshot");
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "clients/open 必须成功: {body}");
    (
        body["client_session_id"].as_str().expect("cs").to_owned(),
        body,
    )
}

/// 走服务面 RPC（`POST /ringing/v2/service/{method}`）。
async fn service_call(
    app: &Router,
    token: &str,
    client_session_id: &str,
    method: &str,
    params: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/ringing/v2/service/{method}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(CS_HEADER, client_session_id)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(params.to_string()))
        .expect("service request");
    let response = app.clone().oneshot(request).await.expect("service oneshot");
    let status = response.status();
    (status, body_json(response).await)
}

/// 真 ws 客户端的升级请求。
///
/// `Sec-WebSocket-Key` 用 RFC 6455 §1.3 的示例值：tungstenite 0.29 的
/// `IntoClientRequest for Request<()>` **不**替调用方补 key/version，缺了就直接
/// `InvalidHeader("sec-websocket-key")`。服务端算出的 accept 是按这条 key 校验的，
/// 所以固定值可重复用（两条连接各来一次也没问题）。
fn ws_request(addr: std::net::SocketAddr, uri: &str, token: &str, lease: &str) -> Request<()> {
    tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://{addr}{uri}"))
        // tungstenite 0.29 对调用方自建的 `Request` 要求显式 `host`（缺了直接
        // `InvalidHeader("host")`），不替你补。
        .header(header::HOST, addr.to_string())
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(CS_HEADER, lease)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .expect("ws request")
}

/// `oneshot` 探针用的升级请求头（不真连，只看握手前的判定）。
fn upgrade_headers(uri: &str, token: &str, client_session_id: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(CS_HEADER, client_session_id)
        .header(header::CONNECTION, "keep-alive, Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .expect("upgrade request")
}

/// `connect_async` 回来的流类型（明文回环，没有 TLS，但类型就是 MaybeTls）。
type WsClient = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// 收下行裸字节，直到剥 ANSI 之后的文本满足条件（或超时）。
async fn collect_until(
    socket: &mut WsClient,
    seen: &mut Vec<u8>,
    budget: Duration,
    ready: impl Fn(&str) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + budget;
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap_or_else(|_| panic!("{what}: 下行超时"));
        match message {
            Some(Ok(WsMessage::Binary(bytes))) => seen.extend(bytes),
            Some(Ok(other)) => panic!("{what}: 这一阶段只该收裸字节,收到 {other:?}"),
            Some(Err(error)) => panic!("{what}: ws 帧错误 {error}"),
            None => panic!("{what}: 下行流提前结束"),
        }
        let text = visible(seen);
        if ready(&text) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: {}s 内没等到期望内容，累积剥 ANSI 后: {text:?}",
            budget.as_secs()
        );
    }
}

/// 全链路：open → ws 订阅 → 打字 → resize → 第二个观看者 + 断联不 kill → close。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_stream_open_type_resize_second_viewer_then_close() {
    let state = test_state();
    let app = build_router(state.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_app = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, server_app).await.expect("serve");
    });

    let (lease, _) = open_lease(&app, ADMIN, "ws-roundtrip-instance").await;
    let (status, view) = service_call(
        &app,
        ADMIN,
        &lease,
        "shell.open",
        json!({ "cols": 110, "rows": 30 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "shell.open: {view}");
    let shell_id = view["id"].as_str().expect("shell id").to_owned();
    assert_eq!(view["alive"], json!(true), "刚开的 shell 必须是活的: {view}");
    assert_eq!(
        (view["cols"].as_u64(), view["rows"].as_u64()),
        (Some(110), Some(30)),
        "open 的尺寸必须来自请求（D6：不是 exec 面那套硬编码 80×24）: {view}"
    );

    let uri = qaqh_ringing::shell_stream_path(&shell_id);
    let (mut socket, handshake) =
        tokio_tungstenite::connect_async(ws_request(addr, &uri, ADMIN, &lease))
            .await
            .expect("ws handshake");
    assert_eq!(
        handshake.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "握手必须升级到 101"
    );

    // ① 打字：期望在**裸字节**下行里同时看到回显的那行命令和执行输出。
    let stdin = ShellStreamControl::Stdin {
        bytes: STANDARD.encode(format!("echo {MARKER}\r")),
    };
    socket
        .send(WsMessage::text(
            serde_json::to_string(&stdin).expect("control frame"),
        ))
        .await
        .expect("send stdin");
    let mut seen: Vec<u8> = Vec::new();
    collect_until(
        &mut socket,
        &mut seen,
        Duration::from_secs(25),
        |text| text.contains(&format!("echo{MARKER}")) && text.matches(MARKER).count() >= 2,
        "① 打字回显",
    )
    .await;
    // 面纪律：shell 的裸字节流里不许混进任何会话帧文本（D3 硬约束第 4 条）。
    let raw = String::from_utf8_lossy(&seen).to_lowercase();
    assert!(
        !raw.contains("session_event") && !raw.contains("\"seq\""),
        "shell 流里混进了会话帧: {raw}"
    );

    // ② resize：UI 测量值走控制帧；内核真值由 RPC 面回报（这条流上没有 ack 帧）。
    let resize = ShellStreamControl::Resize { cols: 132, rows: 44 };
    socket
        .send(WsMessage::text(
            serde_json::to_string(&resize).expect("control frame"),
        ))
        .await
        .expect("send resize");
    let resize_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, listed) = service_call(&app, ADMIN, &lease, "shell.list", json!({})).await;
        let shell = &listed["shells"][0];
        if (shell["cols"].as_u64(), shell["rows"].as_u64()) == (Some(132), Some(44)) {
            break;
        }
        assert!(
            Instant::now() < resize_deadline,
            "10s 内内核几何没到 132×44: {listed}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ③ 第二个观看者：attach 即拿到**有界回放**（含刚打过的 marker），不需要客户端有游标。
    let (mut second, _) = tokio_tungstenite::connect_async(ws_request(addr, &uri, ADMIN, &lease))
        .await
        .expect("second ws handshake");
    let mut replayed: Vec<u8> = Vec::new();
    collect_until(
        &mut second,
        &mut replayed,
        Duration::from_secs(20),
        |text| text.contains(MARKER),
        "③ 第二个观看者的回放",
    )
    .await;

    // ④ 断联不 kill：第二个观看者走人不伤会话（与 ExecStopReason 无 client 档同律）。
    second.send(WsMessage::Close(None)).await.expect("close second");
    drop(second);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, listed) = service_call(&app, ADMIN, &lease, "shell.list", json!({})).await;
    assert_eq!(
        listed["shells"].as_array().expect("shells").len(),
        1,
        "断联不得回收 shell: {listed}"
    );
    assert_eq!(listed["shells"][0]["alive"], json!(true), "断联不得杀会话");

    // ⑤ close：RPC 面收割，观看者从这条流上拿到 `closed` 控制帧 + 连接结束。
    let (status, closed) = service_call(
        &app,
        ADMIN,
        &lease,
        "shell.close",
        json!({ "shell_id": shell_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "shell.close: {closed}");
    assert_eq!(
        closed["reason"],
        json!("closed"),
        "显式 close 的原因必须是 closed: {closed}"
    );

    let mut saw_closed = None;
    let mut tail = String::new();
    // 墙钟上限，不是迭代上限：一次 `next()` 超时是 5s，按次数乘出来会变成十几分钟的
    // 静默挂起（这条流上任何一帧都不该让客户端等这么久）。
    let closed_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < closed_deadline {
        let Some(message) = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap_or(None)
        else {
            break;
        };
        match message.expect("ws 帧") {
            WsMessage::Text(text) => match serde_json::from_str::<ShellStreamEvent>(text.as_str())
                .expect("closed 帧形状")
            {
                ShellStreamEvent::Closed { reason } => {
                    saw_closed = Some(reason);
                    break;
                }
            },
            WsMessage::Binary(_) => continue,
            WsMessage::Close(_) => break,
            other => tail.push_str(&format!("{other:?} ")),
        }
    }
    assert_eq!(
        saw_closed.as_deref(),
        Some("closed"),
        "close 之后必须收到 closed{{reason:\"closed\"}}；尾巴={tail}"
    );
    // 事实落定之后这条流就该结束：要么收到 close，要么流自己断。
    let ended = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .map(|message| message.is_none() || matches!(message, Some(Ok(WsMessage::Close(_)))))
        .unwrap_or(false);
    assert!(ended, "closed 帧之后这条流必须结束");

    let (_, listed) = service_call(&app, ADMIN, &lease, "shell.list", json!({})).await;
    assert!(
        listed["shells"].as_array().expect("shells").is_empty(),
        "close 后必须从表里摘掉: {listed}"
    );

    server.abort();
}

/// 握手前的拒绝全部走 HTTP，不升级：非升级请求 / view 档 / 借别人的 lease / 未知 shell_id。
///
/// 「借别人的 lease」这一条是切片 4 顺手补上的闸：RPC 面与 ws 面共用
/// `service_api::shell_owner`，那里多了 `caller_lease_bound_to_identity` 一步
/// —— 没有它，有效 token + 猜到的 `client_session_id` 就能把 shell 挂到**别人**
/// 的 owner 名下，还能操作别人已开的壳（`authz.rs` 顶部明令禁止的那类冒充）。
#[tokio::test]
async fn shell_stream_handshake_gate_reuses_the_single_identity_model() {
    let state = test_state();
    let app = build_router(state.clone());

    let (interact_id, interact_token) = state
        .devices
        .lock()
        .expect("devices")
        .issue("ws-gate-interact", "node", Scope::Interact);
    let (_view_id, view_token) = state
        .devices
        .lock()
        .expect("devices")
        .issue("ws-gate-view", "node", Scope::View);
    let (thief_id, thief_token) = state
        .devices
        .lock()
        .expect("devices")
        .issue("ws-gate-thief", "node", Scope::Interact);

    let (interact_lease, _) = open_lease(&app, &interact_token, &interact_id).await;
    let (view_lease, _) = open_lease(&app, &view_token, "view-device").await;
    let (thief_lease, _) = open_lease(&app, &thief_token, &thief_id).await;

    // ① 未知 shell_id ⇒ 404 shell_not_found（升级前就拒，不给一条慢慢死的连接）。
    for ghost in [
        qaqh_ringing::shell_stream_path("shell_ffffffffffffffffffffffff"),
        qaqh_ringing::shell_stream_path("shell_dfffffffffffffffffffffff"),
    ] {
        let response = app
            .clone()
            .oneshot(upgrade_headers(&ghost, &interact_token, &interact_lease))
            .await
            .expect("ghost shell");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "未知 shell_id 必须 404: {ghost}"
        );
        let body = body_json(response).await;
        assert_eq!(body["code"], json!("shell_not_found"), "{body}");
    }

    // ② view 档：与 RPC 面同一道 scope 闸，403 而不是升级后另判一套。
    let shell_uri = {
        let (_, view) = service_call(&app, ADMIN, &interact_lease, "shell.open", json!({})).await;
        qaqh_ringing::shell_stream_path(view["id"].as_str().expect("shell id"))
    };
    let response = app
        .clone()
        .oneshot(upgrade_headers(&shell_uri, &view_token, &view_lease))
        .await
        .expect("view scope");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "view 档不得连 shell 流"
    );

    // ③ 借别人的 lease：两条路（ws + RPC）都必须 401，同一条闸不许只装在一边。
    for (label, request) in [
        (
            "ws",
            upgrade_headers(&shell_uri, &thief_token, &interact_lease),
        ),
        (
            "rpc",
            Request::builder()
                .method("POST")
                .uri("/ringing/v2/service/shell.list")
                .header(header::AUTHORIZATION, format!("Bearer {thief_token}"))
                .header(CS_HEADER, &interact_lease)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("thief rpc request"),
        ),
    ] {
        let response = app.clone().oneshot(request).await.expect("thief oneshot");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{label} 路：身份与 lease 不绑定即无 lease"
        );
        let body = body_json(response).await;
        assert_eq!(body["code"], json!("lease_required"), "{label}: {body}");
    }

    // thief 自己的 lease 是有效的，只是那条 shell 不归它：ws 路上同一个 404 文案
    // （存在性探针问不出东西），RPC 路上它自己的 list 是空的。
    let response = app
        .clone()
        .oneshot(upgrade_headers(&shell_uri, &thief_token, &thief_lease))
        .await
        .expect("thief own lease");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "ws 路：别人的 shell_id 在自己 lease 下必须 404"
    );
    let body = body_json(response).await;
    assert_eq!(body["code"], json!("shell_not_found"), "{body}");
    let (_, body) = service_call(&app, &thief_token, &thief_lease, "shell.list", json!({})).await;
    assert_eq!(
        body["shells"].as_array().expect("shells").len(),
        0,
        "thief 看不见别人的壳: {body}"
    );

    // ④ 非升级请求（route_table 探针走的就是这条路）：不得是 router 的 fallback。
    //    判完身份与 attach 之后才轮到 `WebSocketUpgrade` 抽取器，所以合法 owner 发的
    //    普通 GET 拿到的是抽取器自己的 400（「这不是升级请求」），不是 404/401/403 ——
    //    顺序本身就是结论：400 只在四类拒绝全通过之后才可能出现。
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&shell_uri)
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
                .header(CS_HEADER, &interact_lease)
                .body(Body::empty())
                .expect("plain get"),
        )
        .await
        .expect("plain get");
    assert_ne!(response.status(), StatusCode::NOT_FOUND, "路由必须已注册");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "非升级请求必须被抽取器当场拒,不许悄悄升级"
    );

    // 收尾：把这条真 shell 关掉，别让它占到 daemon 关停。
    let shell_id = shell_uri
        .strip_prefix("/ringing/v2/shells/")
        .and_then(|rest| rest.strip_suffix("/stream"))
        .expect("shell id from path");
    let (_, closed) = service_call(
        &app,
        ADMIN,
        &interact_lease,
        "shell.close",
        json!({ "shell_id": shell_id }),
    )
    .await;
    assert_eq!(closed["reason"], json!("closed"), "{closed}");
}
