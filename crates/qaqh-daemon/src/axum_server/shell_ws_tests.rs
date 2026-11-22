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

/// SDK 客户端专用的 marker（与 `MARKER` 分开：两条测试各自验各自的会话）。
const SDK_MARKER: &str = "qaqhclientsdk";

/// 反复收下下行裸字节，直到剥 ANSI 后的可见文本满足条件（口径见文件头：ConPTY 会把
/// 回显切成转义片段，不剥就会把明明在场的回显判成丢失）。
async fn drain_until(
    bytes: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    seen: &mut Vec<u8>,
    budget: Duration,
    want: impl Fn(&str) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_millis(500), bytes.recv()).await {
            seen.extend(chunk);
        }
        if want(&visible(seen)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}：{}s 内没等到期望内容，可见文本={:?}",
            budget.as_secs(),
            visible(seen)
        );
    }
}

/// 切片 5 判据 1 的**另一半**：用真 SDK 客户端（`qaqh_client::shell_stream`）打这条流。
///
/// 上面那条往返用的是手搓的 `tungstenite` 客户端——它证明服务端形状对，但证明不了
/// 客户端与服务端**理解同一套协议**：两边各自错到一起去，测试照样绿。这条换成真客户端，
/// 于是握手头、base64 `stdin`、裸字节下行、`closed` 解析、能力位闸门全部走生产代码；
/// §切片 5 的行为验收 ①②③④ 也因此在机器上断言得动（GUI 那一次仍要点，但它不再是
/// 唯一凭据）。lease 是真协商的：`clients/open` 作答 `shell_stream`，客户端只认这一位。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_shell_stream_round_trip_against_the_served_daemon() {
    use qaqh_client::shell_stream::{ShellStream, ShellStreamHandlers, ShellStreamStatus};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let state = test_state();
    let app = build_router(state.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_app = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, server_app).await.expect("serve");
    });

    let client = qaqh_client::Client::connect_async(qaqh_client::ClientOptions {
        remote: Some(qaqh_client::RemoteEndpoint {
            base_url: format!("http://{addr}"),
            token: ADMIN.to_string(),
            tls_fp: None,
        }),
        ..Default::default()
    })
    .await
    .expect("客户端必须与这台 daemon 协商出 lease");
    let lease = client.session_state().await.expect("lease 已协商");
    assert!(
        lease.capabilities.shell_stream,
        "能力位必须由 `clients/open` 显式作答；客户端侧的闸门只认这一位（§D3 约束 5）"
    );

    let opened = client
        .service_call("shell.open", json!({ "cols": 110, "rows": 30 }))
        .await
        .expect("shell.open 经客户端可达（放行表在 qaqh_client::service::SHELL_METHODS）");
    let shell_id = opened["id"].as_str().expect("shell id").to_owned();

    let (bytes_tx, mut bytes_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ShellStreamEvent>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ShellStreamStatus>();
    let stream = ShellStream::start(
        &shell_id,
        &client,
        ShellStreamHandlers {
            on_bytes: Arc::new(move |_id, bytes| {
                let _ = bytes_tx.send(bytes);
            }),
            on_event: Arc::new(move |_id, event| {
                let _ = event_tx.send(event);
            }),
            on_status: Arc::new(move |_id, status| {
                let _ = status_tx.send(status);
            }),
        },
    )
    .await
    .expect("订阅这条流");

    // ① 开终端就该看到提示符：首帧不带 from_offset = 服务端按 0 attach = 整段缓冲尾。
    let mut seen: Vec<u8> = Vec::new();
    drain_until(&mut bytes_rx, &mut seen, Duration::from_secs(25), |text| {
        !text.is_empty()
    }, "① 开终端的输出")
    .await;
    // ② 打字有回显：一帧 stdin，服务端 base64 解码后喂同一个 ConPTY。
    stream
        .stdin(format!("echo {SDK_MARKER}\r\n").as_bytes())
        .await
        .expect("stdin 上行");
    drain_until(&mut bytes_rx, &mut seen, Duration::from_secs(25), |text| {
        text.contains(SDK_MARKER)
    }, "② 打字的回显")
    .await;

    // ③ 几何真值只能读 `shell.list`：这条流上刻意没有 ack 帧。
    stream.resize(132, 44).await.expect("resize 上行");
    let listed = client
        .service_call("shell.list", json!({}))
        .await
        .expect("shell.list");
    let mine = listed["shells"]
        .as_array()
        .expect("shells")
        .iter()
        .find(|entry| entry["id"].as_str() == Some(&shell_id))
        .expect("本身份的清单里该有这条 shell");
    assert_eq!(
        (mine["cols"].as_u64(), mine["rows"].as_u64()),
        (Some(132), Some(44)),
        "③ resize 没落到 PTY: {mine}"
    );

    // ④ 子进程自己退出：退出观察线程落的 `child_exited` 必须原样到达 closed 帧。
    stream.stdin(b"exit\r\n").await.expect("exit 上行");
    let reason = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if let Some(ShellStreamEvent::Closed { reason }) = event_rx.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("④ 等不到 closed 帧");
    assert_eq!(reason, "child_exited", "④ 终止原因不是 child_exited");

    // 协议事实之后，传输层必须落到同一个终态，且不再接受控制帧（不静默吞字）。
    // 这里必须**等**而不是 `try_recv` 扫一遍：`closed` 帧先上报、状态后落地，是读循环
    // 的真实顺序（先搬完尾字节再宣布结束），用 try_recv 会把自己写成一条会赢的竞态。
    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match status_rx.recv().await {
                Some(ShellStreamStatus::Closed { reason }) => return reason,
                Some(_) => {}
                // 发送端先掉了却没落到 Closed = 读循环在没有终态的情况下消失，那是 bug。
                None => panic!("状态通道先于 Closed 断掉"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("传输状态没跟着协议事实落到 Closed"));
    assert_eq!(terminal, "child_exited", "传输状态与协议事实脱节");
    let error = stream.stdin(b"ls\r\n").await.unwrap_err();
    assert!(error.to_string().contains("已终止"), "流终止后还在收控制帧: {error}");

    server.abort();
}

/// 从剥过 ANSI 的可见文本里按出现顺序抽 `SEQX<n>` 序号（不引 regex：dep 面不为
/// 一条测试添重）。命令回显里的 `SEQX-$_` 剥掉符号后是 `SEQX` 后跟 `p`，天然不误匹配。
fn seqx_tokens(text: &str) -> Vec<u64> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"SEQX" {
            let mut j = i + 4;
            let mut value = 0u64;
            let mut digits = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                value = value * 10 + u64::from(bytes[j] - b'0');
                digits += 1;
                j += 1;
            }
            if digits > 0 {
                tokens.push(value);
                i = j;
                continue;
            }
        }
        i += 1;
    }
    tokens
}

/// Lagged → resync 的接缝上，同一段字节**绝不允许**投递第二遍。
///
/// 压法：洪峰（~30MB）打到一半时客户端停读 3s —— TCP 背压把服务端的
/// `frames.recv()` 钉住，broadcast 环（512 帧）溢出 ⇒ 服务端在洪峰中途重取回放尾；
/// 之后限速抽干，让接缝在洪峰还热着的时候反复经过；静默后再从服务端权威总数
/// （`shell.list` 的 `output_offset`）显式 attach 一发，把两条 resync 臂都压到。
/// 断言只有一个口径：剥 ANSI 后抽出的 `SEQX<n>` 序号全程**严格递增** —— 回退就是
/// 同一段字节被送了两遍。缺口（序号跳跃）允许：环只有 256KiB，落后超过一个环按
/// 设计只补尾巴（D3 约束 3，不发明第二套 resync 协议）。
///
/// 刻意**不发** `attach{from_offset: delivered}`（客户端自己的字节计数）：落后超过
/// 一个环之后那个数已不是合法续读点（中间有洞），按它重取会把已收到的尾巴重发
/// 一遍 —— 那是 `qaqh-client/src/shell_stream.rs` 头注里明写的「刻意接受的语义」，
/// 不是这里要抓的缺陷；本测试只锁服务端自己的 resync 绝不重发。
///
/// 修复前这条测试对竞态窗口是概率红的（重叠窗只有订阅→环快照那几微秒）；修复后
/// 「resync 后水位 = next_offset」让不重复成为确定性事实，这条测试锁的就是它。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_stream_lagged_resync_never_repeats_bytes() {
    const SEQ_LINES: u64 = 30_000;
    let state = test_state();
    let app = build_router(state.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_app = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, server_app).await.expect("serve");
    });

    let (lease, _) = open_lease(&app, ADMIN, "ws-lag-resync-instance").await;
    let (_, opened) = service_call(
        &app,
        ADMIN,
        &lease,
        "shell.open",
        json!({ "cols": 110, "rows": 30 }),
    )
    .await;
    let shell_id = opened["id"].as_str().expect("shell id").to_owned();

    let uri = qaqh_ringing::shell_stream_path(&shell_id);
    let (mut socket, _) =
        tokio_tungstenite::connect_async(ws_request(addr, &uri, ADMIN, &lease))
            .await
            .expect("ws handshake");

    // 洪峰：30000 行 × ~1KB ≈ 30MB。行尾填充把读泵的 8KB buffer 压满，
    // 让 512 帧的 broadcast 环在停读窗口内必然溢出。
    let command = format!("1..{SEQ_LINES} | ForEach-Object {{ \"SEQX-$_\" + ('p'*980) }}\r");
    socket
        .send(WsMessage::text(
            serde_json::to_string(&ShellStreamControl::Stdin {
                bytes: STANDARD.encode(command.as_bytes()),
            })
            .expect("control frame"),
        ))
        .await
        .expect("send flood command");

    // ① 停读 3s：TCP 背压 → 服务端 sink 阻塞 → broadcast 环溢出 → Lagged。
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // ② 限速抽干：每帧让出 1ms，让客户端持续落后于读泵 ⇒ 服务端在洪峰中途反复
    //    Lagged→resync。不发任何 attach：见上（计数在截断后不是合法续读点）。
    let mut seen: Vec<u8> = Vec::new();
    let drain_deadline = Instant::now() + Duration::from_secs(120);
    let mut quiet_since = Instant::now();
    loop {
        assert!(Instant::now() < drain_deadline, "120s 没抽干洪峰");
        match tokio::time::timeout(Duration::from_millis(250), socket.next()).await {
            Err(_elapsed) => {
                if quiet_since.elapsed() > Duration::from_secs(3) {
                    break;
                }
            }
            Ok(None) => panic!("下行流提前结束"),
            Ok(Some(Err(error))) => panic!("ws 帧错误: {error}"),
            Ok(Some(Ok(WsMessage::Close(_)))) => break,
            Ok(Some(Ok(WsMessage::Binary(bytes)))) => {
                seen.extend_from_slice(&bytes);
                quiet_since = Instant::now();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                panic!("洪峰阶段不该有下行控制帧: {text}");
            }
            Ok(Some(Ok(other))) => panic!("意外帧: {other:?}"),
        }
    }

    // ③ 显式 attach 臂 + 回放/实时接缝：洪峰静默后读权威总数，从那里 attach。
    //    回放为空、订阅与快照之间的重叠帧被 next_offset 水位挡住 ⇒ 不产生重复序号。
    let (_, listed) = service_call(&app, ADMIN, &lease, "shell.list", json!({})).await;
    let output_offset = listed["shells"][0]["output_offset"]
        .as_u64()
        .expect("shell.list 必须带 output_offset");
    socket
        .send(WsMessage::text(
            serde_json::to_string(&ShellStreamControl::Attach {
                from_offset: Some(output_offset),
            })
            .expect("control frame"),
        ))
        .await
        .expect("send attach at authoritative offset");
    // attach 之后再收 1.5s：有任何泄漏（重叠帧没被水位挡住）就并进来一起断言。
    let attach_window = Duration::from_millis(1500);
    let attach_started = Instant::now();
    loop {
        let remaining = attach_window.saturating_sub(attach_started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.next()).await {
            Err(_elapsed) => break,
            Ok(None) => panic!("attach 阶段下行流提前结束"),
            Ok(Some(Err(error))) => panic!("attach 阶段 ws 帧错误: {error}"),
            Ok(Some(Ok(WsMessage::Close(_)))) => break,
            Ok(Some(Ok(WsMessage::Binary(bytes)))) => seen.extend_from_slice(&bytes),
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                panic!("attach 阶段不该有下行控制帧: {text}");
            }
            Ok(Some(Ok(other))) => panic!("意外帧: {other:?}"),
        }
    }

    // ④ 断言：序号严格递增（重复 = 回退 = 红）；缺口允许；尾行必须到场
    //    （环恒保有尾巴，最后一次 resync 的回放一定覆盖到当前总数）。
    let tokens = seqx_tokens(&visible(&seen));
    assert!(
        tokens.len() >= 600,
        "洪峰没被观测到（token 只有 {} 个），压法失效",
        tokens.len()
    );
    for window in tokens.windows(2) {
        assert!(
            window[1] > window[0],
            "序号回退 = 字节重复投递：{} 之后出现 {}",
            window[0],
            window[1]
        );
    }
    assert_eq!(*tokens.last().expect("tokens 非空"), SEQ_LINES, "尾行必须到场");

    let (_, closed) = service_call(
        &app,
        ADMIN,
        &lease,
        "shell.close",
        json!({ "shell_id": shell_id }),
    )
    .await;
    assert_eq!(closed["reason"], json!("closed"), "{closed}");
    server.abort();
}
