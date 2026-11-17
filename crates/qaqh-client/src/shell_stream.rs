//! 用户 shell 终端的 **ws 数据面客户端**（`feature = "shell-stream"`，ADR-0028 切片 5 · 方案 A）。
//!
//! # 为什么这份代码在 Rust 而不是 Electron 的 TS 里
//!
//! 词表（`attach`/`stdin`/`resize`/`closed`）与 offset 语义的真源是
//! [`qaqh_ringing::ShellStreamControl`]，与 daemon 的 `shell_ws.rs` **同一个类型**。
//! 放在 `qaqh-client` 里，TUI / Android / HarmonyOS / 桌面壳共用一份协议运行时；
//! 在 TS 再写一遍就是第二份实现（§切片 5 施工前置把这条列为裁定的主要理由）。
//!
//! # 这条流上只有四件事（不许长出第五件）
//!
//! - 下行：**裸字节**（PTY 输出）+ 一种 text 帧 `closed{reason}`；
//! - 上行：text 帧 `attach{from_offset?}` / `stdin{bytes(base64)}` / `resize{cols,rows}`；
//! - **没有** seq / ack / `resync_required`：恢复语义就是 `from_offset` 这一个数；
//! - 断线**不** kill shell（§D5），重连即重新 attach 取有界回放尾（256 KiB）。
//!
//! # `from_offset` 是什么数（以及它不是什么）
//!
//! 客户端记的是**自己已经收到的字节数**。首次连接不带这个参数 = 服务端按 0 attach
//! = 整段有界回放尾（打开终端立刻看到提示符）。之后每次重连都带上它：正常情形下这个
//! 数正好等于服务端写入环的水位，于是回放为空、只续实时流，不会重复刷屏。
//!
//! 它**不是**会话事件流的 `epoch:seq` 游标，服务端也不为它记任何客户端状态。若环已经
//! 截断了这一段（落后太久），这个数落在环头之前，服务端按可用段给（可能重复送一段尾巴）
//! —— 这是本条流刻意接受的语义：要干净屏幕就 `attach(None)` 配合 UI 清屏重建，而不是
//! 在这里发明第二套 resync 协议（§D3 约束 3）。
//!
//! # 鉴权只有一套
//!
//! 握手带 `Authorization: Bearer <daemon token>` 与 `x-qaqh-client-session-id`，两个值
//! 每轮现取（[`Client::credentials`] / 当前 lease），**不**缓存、**不**自报 owner：
//! daemon 侧 `service_api::shell_owner()` 从身份反查 owner，与 `shell.*` RPC 三条共用
//! 同一个函数（§D3 约束 2）。

use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use qaqh_ringing::{CLIENT_SESSION_HEADER, ShellStreamControl, ShellStreamEvent, shell_stream_path};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Request, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::client::Client;
use crate::error::{ClientError, Result};

/// 单帧上限，与 daemon 的 `SHELL_WS_MAX_CONTROL_BYTES` 同值同律：一次粘贴是一帧
/// （base64 后约 1.34×），256 KiB 的回放尾是一次下行 message。
const SHELL_WS_MAX_BYTES: usize = 1024 * 1024;

/// 重连退避：与会话单流（`v2_stream.rs`）同一组常数，不新造一档节奏。
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 30_000;

/// 控制帧队列深度。`stdin` 是逐键/逐粘贴的小帧；积压到 64 条说明这条流已经断了很久，
/// **当场报错**给调用方，不静默吞键（宪法 5：不能工作要响）。
const CTL_QUEUE: usize = 64;

/// 一条 shell 流的读循环回调。与 [`crate::v2_stream::V2StreamHandlers`] 同形状：
/// 数据、协议事实、传输状态各一条，互不重叠。
pub struct ShellStreamHandlers {
    /// 下行裸字节（PTY 输出），原样交给 UI。
    pub on_bytes: Arc<dyn Fn(String, Vec<u8>) + Send + Sync>,
    /// 下行 text 控制帧。词表里只有 `closed{reason}` 一种。
    pub on_event: Arc<dyn Fn(String, ShellStreamEvent) + Send + Sync>,
    /// 连接生命周期（connecting / open / reconnecting / closed）。
    pub on_status: Arc<dyn Fn(String, ShellStreamStatus) + Send + Sync>,
}

/// ws 连接的生命周期状态。`Reconnecting` 与 `Closed` 的界线由**协议事实**决定：拿到
/// `closed{reason}` 或握手 404 才算 Closed（不再重连）；其余断线一律重连（§D5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellStreamStatus {
    Connecting,
    /// `delivered` = 本条连接已交付的字节数，即重连时交回服务端的 `from_offset`。
    Open { delivered: u64 },
    Reconnecting {
        retry_ms: u64,
        delivered: u64,
        reason: String,
    },
    Closed {
        reason: String,
    },
}

/// 一次拨号（含整条连接的存活期）的结果。
enum Dial {
    /// 连接断了，但这条 shell 还该继续看：外层退避后重连。
    Reconnect(String),
    /// 这条流已经没有意义（终止事实已到）：不再重连。
    Terminal(String),
}

/// 一条 shell 的 ws 订阅句柄。句柄活着 = 读循环活着。[`Self::close`] 只断这条
/// **订阅**，不碰子进程（§D5 断联不 kill）；要杀 shell 走 `shell.close` 服务面。
pub struct ShellStream {
    shell_id: String,
    ctl_tx: mpsc::Sender<ShellStreamControl>,
    stop_tx: watch::Sender<bool>,
}

impl ShellStream {
    /// 起一条 shell 流：先读 daemon 显式声明的能力位 `shell_stream`，再 spawn 常驻
    /// 读循环。**不**按 daemon 版本猜能力（§D3 约束 5）。
    pub async fn start(
        shell_id: impl Into<String>,
        client: &Client,
        handlers: ShellStreamHandlers,
    ) -> Result<Self> {
        let shell_id = shell_id.into();
        let state = client
            .session_state()
            .await
            .ok_or_else(|| ClientError::Negotiation("shell 流需要先协商 lease（client 尚未 open）".into()))?;
        if !state.capabilities.shell_stream {
            return Err(ClientError::Negotiation(
                "daemon 未声明 shell_stream 能力：这条流不存在，不许用版本猜测绕过".into(),
            ));
        }
        let (ctl_tx, ctl_rx) = mpsc::channel::<ShellStreamControl>(CTL_QUEUE);
        let (stop_tx, stop_rx) = watch::channel(false);
        let worker = Worker {
            shell_id: shell_id.clone(),
            client: client.clone(),
            handlers,
            ctl_rx,
            stop_rx,
            client_stop: client.stop_signal(),
            delivered: 0,
        };
        tokio::spawn(worker.run());
        Ok(Self {
            shell_id,
            ctl_tx,
            stop_tx,
        })
    }

    /// 原始 PTY 输入。一次粘贴是一帧，一次 keystroke 也是一帧——都编成
    /// `stdin{bytes(base64)}` 的 text 帧（上行只有 text 一种形状，见 shell_stream.rs
    /// 服务端注释：同一个动作两种写法就是两份事实源）。
    pub async fn stdin(&self, bytes: &[u8]) -> Result<()> {
        self.send(ShellStreamControl::Stdin {
            bytes: STANDARD.encode(bytes),
        })
        .await
    }

    /// UI 测量的几何。真值**不**在这条流上回执（没有 ack 帧）：要确认就读 `shell.list`。
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.send(ShellStreamControl::Resize { cols, rows }).await
    }

    /// 主动重建订阅：`None` = 整段有界回放尾再来一遍（配合 UI 清屏），
    /// `Some(n)` = 从第 n 个字节起给。
    pub async fn attach(&self, from_offset: Option<u64>) -> Result<()> {
        self.send(ShellStreamControl::Attach { from_offset }).await
    }

    /// 停这条订阅。shell 本身继续活着（断联不 kill）。
    pub fn close(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// 把一帧排进这条流的上行队列。
    ///
    /// `send` 失败的唯一条件是读循环已经退出并丢掉接收端——那句话因此不需要再带一个
    /// "是否活着"的布尔：channel 的关闭就是这件事的单一真源。
    async fn send(&self, control: ShellStreamControl) -> Result<()> {
        self.ctl_tx.send(control).await.map_err(|_| {
            ClientError::Transport(format!(
                "shell {} 的 ws 订阅已终止，不再接受控制帧（要重新看就重新订阅）",
                self.shell_id
            ))
        })
    }
}

impl Drop for ShellStream {
    fn drop(&mut self) {
        self.close();
    }
}

/// 读循环本体：连接 → 搬字节/搬控制帧 → 断线退避重连（带 from_offset）→ 直到终止事实。
struct Worker {
    shell_id: String,
    client: Client,
    handlers: ShellStreamHandlers,
    ctl_rx: mpsc::Receiver<ShellStreamControl>,
    /// 本句柄的订阅级停止（`ShellStream::close`）。
    stop_rx: watch::Receiver<bool>,
    /// `Client::close()` 的全局停止：它的文档承诺「停掉全部后台任务」，这条流虽然由
    /// 调用方持有，也不该在客户端已关闭后继续重连一个死 lease。
    client_stop: watch::Receiver<bool>,
    /// 已交付字节数 = 重连时的 `from_offset`。这条流上客户端唯一记住的数。
    delivered: u64,
}

impl Worker {
    async fn run(mut self) {
        let mut retry_ms = RETRY_BASE_MS;
        loop {
            if self.should_stop() {
                // 如实报一句「我们停了」——不是 shell 死了，也不是 daemon 说了什么。
                // 上层（桌面壳的桥）只把它进日志；渲染层的 `closed` 只来自协议帧。
                self.report_closed(self.stop_reason());
                return;
            }
            match self.dial().await {
                Dial::Terminal(reason) => {
                    self.report_closed(&reason);
                    return;
                }
                Dial::Reconnect(reason) => {
                    if self.should_stop() {
                        self.report_closed(self.stop_reason());
                        return;
                    }
                    (self.handlers.on_status)(
                        self.shell_id.clone(),
                        ShellStreamStatus::Reconnecting {
                            retry_ms,
                            delivered: self.delivered,
                            reason,
                        },
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
                        _ = self.stop_rx.changed() => {}
                        _ = self.client_stop.changed() => {}
                    }
                    retry_ms = std::cmp::min(retry_ms * 2, RETRY_MAX_MS);
                }
            }
        }
    }

    fn should_stop(&self) -> bool {
        *self.stop_rx.borrow() || *self.client_stop.borrow()
    }

    /// 停止的原因是**本地事实**，分两档说清楚：调用方主动退订（`shell.close` 那条
    /// napi 口 / 句柄被 drop）与整个客户端关闭（daemon 重启后主进程换新客户端）。
    /// 两者都不是 daemon 告诉我们的，所以走 `on_status` 而不是 `on_event`。
    fn stop_reason(&self) -> &'static str {
        if *self.stop_rx.borrow() {
            "unsubscribed"
        } else {
            "client closed"
        }
    }

    fn report_closed(&self, reason: &str) {
        (self.handlers.on_status)(
            self.shell_id.clone(),
            ShellStreamStatus::Closed {
                reason: reason.to_string(),
            },
        );
    }

    /// 拨一次号并把这条连接跑到终止/断线为止。
    async fn dial(&mut self) -> Dial {
        (self.handlers.on_status)(self.shell_id.clone(), ShellStreamStatus::Connecting);
        let creds = self.client.credentials();
        let session_id = match self.client.session_id_header().await {
            Ok(id) => id,
            Err(error) => return Dial::Reconnect(format!("lease 不可用: {error}")),
        };
        let url = match shell_ws_url(&creds.base_url, &self.shell_id, self.delivered) {
            Ok(url) => url,
            Err(error) => return Dial::Terminal(error.to_string()),
        };
        let request = match shell_handshake_request(&url, &creds.token, &session_id) {
            Ok(request) => request,
            Err(error) => return Dial::Terminal(error.to_string()),
        };
        // `WebSocketConfig` 是 `#[non_exhaustive]`：只能从 Default 出发逐字段设，
        // 正好逼我们把「为什么设这两条」写在旁边而不是塞进一个字面量。
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(SHELL_WS_MAX_BYTES);
        config.max_frame_size = Some(SHELL_WS_MAX_BYTES);
        let config = Some(config);
        let connector = match (creds.tls_fp.clone(), url.starts_with("wss://")) {
            (Some(fp), true) => match crate::tls_pin::pinned_client_config(&fp) {
                Ok(tls_config) => Some(Connector::Rustls(Arc::new(tls_config))),
                Err(error) => return Dial::Terminal(error.to_string()),
            },
            // 没有指纹就没有信任锚。这里**不**降级成明文、也**不**改用系统默认信任：
            // 那等于给同一个 daemon 开第二条凭证面（§D3 约束 2）。
            (None, true) => {
                return Dial::Terminal(
                    "wss 端点缺少 tls_fp：没有指纹就没有信任锚，不降级到明文".to_string(),
                );
            }
            (_, false) => None,
        };

        let (mut ws, _) = match connect_async_tls_with_config(request, config, false, connector).await
        {
            Ok(pair) => pair,
            Err(error) => return classify_handshake(&error),
        };
        (self.handlers.on_status)(
            self.shell_id.clone(),
            ShellStreamStatus::Open {
                delivered: self.delivered,
            },
        );
        // 连上了：退避归零，下次断线重新从短间隔起。
        loop {
            tokio::select! {
                biased;
                // 控制帧先行：打字是本地毫秒级的小活。
                Some(control) = self.ctl_rx.recv() => {
                    let text = match serde_json::to_string(&control) {
                        Ok(text) => text,
                        Err(error) => {
                            return Dial::Terminal(format!("控制帧编码失败（词表被改过？）: {error}"));
                        }
                    };
                    if let Err(error) = ws.send(Message::text(text)).await {
                        return Dial::Reconnect(format!("上行失败: {error}"));
                    }
                }
                _ = self.stop_rx.changed() => return Dial::Reconnect("unsubscribed".into()),
                _ = self.client_stop.changed() => return Dial::Reconnect("client closed".into()),
                received = ws.next() => match received {
                    // 服务端把这条连接的读端收走：断联不等于 shell 死了，重连。
                    None => return Dial::Reconnect("连接被服务端结束".into()),
                    Some(Err(error)) => return classify_stream(&error),
                    Some(Ok(Message::Binary(bytes))) => {
                        self.delivered += bytes.len() as u64;
                        (self.handlers.on_bytes)(self.shell_id.clone(), bytes.to_vec());
                    }
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ShellStreamEvent>(text.as_str()) {
                            // 词表内只有 closed 一种：它是终止事实，报上去即收摊。
                            Ok(event) => {
                                let reason = match &event {
                                    ShellStreamEvent::Closed { reason } => reason.clone(),
                                };
                                (self.handlers.on_event)(self.shell_id.clone(), event);
                                return Dial::Terminal(reason);
                            }
                            Err(error) => {
                                return Dial::Terminal(format!(
                                    "下行 text 帧不在词表内: {error}"
                                ));
                            }
                        }
                    }
                    // 上行只认 text、下行只认裸字节 + closed。出现别的形状说明两侧
                    // 对词表的理解已经分家（服务端那边只会用 1002/1008 关掉连接）。
                    Some(Ok(other)) => {
                        return Dial::Terminal(format!("意料之外的下行帧: {other:?}"));
                    }
                }
            }
        }
    }
}

/// 握手失败分型。
///
/// 只有 404 是「这条 shell 不在了」：daemon 对不存在与不属于本身份的 shell 给同一个
/// 404 `shell_not_found`（D2 的机器面），重连问不出任何东西，也不再有权看它。其余
/// （401/403/5xx/TCP 失败）都可能是 lease 或 daemon 重启的瞬态——lease 每轮现取，
/// 退避重连即可自愈。
fn classify_handshake(error: &WsError) -> Dial {
    if let WsError::Http(response) = error
        && response.status() == StatusCode::NOT_FOUND
    {
        return Dial::Terminal("shell_not_found".to_string());
    }
    Dial::Reconnect(error.to_string())
}

/// 连接建立之后的错误分型：**一律重连**。
///
/// 这条流的终止事实只有一个来源——服务端发下来的 `closed` 帧（以及握手阶段的 404）。
/// tungstenite 自己的 IO/EOF/`ConnectionClosed`/写故障都只说明「这条连接不能用了」，
/// 而 §D5 的口径是**断联不 kill**：shell 大概率还活着，重新 attach 才是要做的事。
///
/// （第一版把这类错误按「协议层错误」收摊，`reconnect_resumes_from_the_bytes_already_delivered`
/// 当场红：假服务端裸拆 socket（不做 closing handshake）时 tungstenite 报的是**协议级**
/// 错误，于是客户端把它当成「两侧词表分家」，再也没有拨第二次号。§D5 说断联不 kill，
/// 所以这里唯一正确的动作就是重连。）
fn classify_stream(error: &WsError) -> Dial {
    Dial::Reconnect(format!("连接中断: {error}"))
}

/// `http(s)://` → `ws(s)://` + shell 流路径 +（重连时）`?from_offset=`。
///
/// 路径由 [`shell_stream_path`] 单点生成——真源在 `qaqh-ringing`，与 daemon 路由表
/// 同一个函数；本函数只换 scheme，不再抄一遍路径。
pub(crate) fn shell_ws_url(base_url: &str, shell_id: &str, from_offset: u64) -> Result<String> {
    let authority = if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        return Err(ClientError::Protocol(format!(
            "base_url 必须是 http(s) 端点，无法推导 ws 端点: {base_url}"
        )));
    };
    let path = shell_stream_path(shell_id);
    // delivered=0（首次连接）不带参数 = 服务端按 0 attach = 整段有界回放尾。
    if from_offset == 0 {
        Ok(format!("{authority}{path}"))
    } else {
        Ok(format!("{authority}{path}?from_offset={from_offset}"))
    }
}

/// 握手请求。
///
/// `IntoClientRequest for &str` 会生成 `host` 与 `sec-websocket-key`（切片 4 实测：
/// 手工构造 `Request` 少任一条即 `InvalidHeader("host")` /
/// `InvalidHeader("sec-websocket-key")`），但**不**替你补鉴权——那两条必须显式给，
/// 否则 daemon 的 `resolve_identity` 当场 401，而且报出来的是一副「连接被拒」的样子，
/// 把排查方向指向网络而不是凭证。
pub(crate) fn shell_handshake_request(
    url: &str,
    token: &str,
    client_session_id: &str,
) -> Result<Request<()>> {
    let mut request = url
        .into_client_request()
        .map_err(|error| ClientError::Transport(format!("握手请求构造失败: {error}")))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|error| ClientError::Protocol(format!("daemon token 不是合法 header 值: {error}")))?;
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let session = HeaderValue::from_str(client_session_id).map_err(|error| {
        ClientError::Protocol(format!("client_session_id 不是合法 header 值: {error}"))
    })?;
    request.headers_mut().insert(CLIENT_SESSION_HEADER, session);
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_switches_scheme_and_keeps_the_shared_path() {
        assert_eq!(
            shell_ws_url("http://127.0.0.1:5555", "shell_01", 0).unwrap(),
            "ws://127.0.0.1:5555/ringing/v2/shells/shell_01/stream"
        );
        // 重连带 from_offset；路径仍来自 shell_stream_path（不抄第二份）。
        assert_eq!(
            shell_ws_url("https://192.168.1.5:8443", "shell_02", 4096).unwrap(),
            "wss://192.168.1.5:8443/ringing/v2/shells/shell_02/stream?from_offset=4096"
        );
    }

    #[test]
    fn url_refuses_a_scheme_it_cannot_map() {
        // 推导不出来就是端点写错了，当场报错，不「试试看 ws://unix://…」。
        let error = shell_ws_url("unix:///tmp/daemon.sock", "s", 0).unwrap_err();
        assert!(error.to_string().contains("http(s)"), "{error}");
    }

    #[test]
    fn handshake_carries_credentials_and_generated_ws_headers() {
        // 切片 4 踩过的坑：tungstenite 只替你生成 host/sec-websocket-key，
        // 鉴权两条要自己给。四条头缺一条，daemon 那边的失败形状都会伪装成网络问题。
        let request = shell_handshake_request(
            "ws://127.0.0.1:5555/ringing/v2/shells/shell_01/stream",
            "tok3n",
            "cs_abc",
        )
        .unwrap();
        let headers = request.headers();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer tok3n");
        assert_eq!(
            headers.get(CLIENT_SESSION_HEADER).unwrap(),
            "cs_abc",
            "session 头必须用协议常量 {CLIENT_SESSION_HEADER}"
        );
        assert_eq!(headers.get("host").unwrap(), "127.0.0.1:5555");
        assert!(
            headers.contains_key("sec-websocket-key"),
            "sec-websocket-key 缺失会被 tungstenite 判为 InvalidHeader"
        );
        assert_eq!(request.uri().path(), "/ringing/v2/shells/shell_01/stream");
    }

    #[test]
    fn stdin_frame_is_base64_on_the_wire() {
        // 帧型与 daemon 同一个类型；这条断言锁的是「上行 keystroke 经 base64」这条
        // 纪律（粘贴里的 ESC 序列不能被 UTF-8 假设吃掉），金标准 fixtures 同形状。
        let frame = ShellStreamControl::Stdin {
            bytes: STANDARD.encode(b"exit\r\n"),
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"type":"stdin","bytes":"ZXhpdA0K"}"#
        );
        assert_eq!(
            serde_json::to_string(&ShellStreamControl::Attach {
                from_offset: Some(2048)
            })
            .unwrap(),
            r#"{"type":"attach","from_offset":2048}"#
        );
        assert_eq!(
            serde_json::to_string(&ShellStreamControl::Resize {
                cols: 132,
                rows: 44
            })
            .unwrap(),
            r#"{"type":"resize","cols":132,"rows":44}"#
        );
    }

    #[test]
    fn handshake_404_is_terminal_but_other_codes_reconnect() {
        let response = http_response(StatusCode::NOT_FOUND);
        assert!(matches!(
            classify_handshake(&WsError::Http(Box::new(response))),
            Dial::Terminal(reason) if reason == "shell_not_found"
        ));
        let forbidden = http_response(StatusCode::FORBIDDEN);
        assert!(matches!(
            classify_handshake(&WsError::Http(Box::new(forbidden))),
            Dial::Reconnect(_)
        ));
        // IO/TCP 失败是 daemon 重启窗口的常态：必须走重连而不是收摊。
        let io = WsError::Io(std::io::Error::other("connection refused"));
        assert!(matches!(classify_handshake(&io), Dial::Reconnect(_)));
    }

    #[test]
    fn a_dead_connection_is_never_a_terminal_fact() {
        // §D5「断联不 kill」的机器面：连接层面的失败一律重连。这条测试是
        // classify_stream 从「协议层错误就收摊」被改成「一律重连」之后补的锚——
        // 没有它，下一个人看到 `Protocol(_)` 会很自然地再把它判成致命。
        let cases = [
            WsError::ConnectionClosed,
            WsError::AlreadyClosed,
            WsError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
            WsError::Utf8("断在 UTF-8 中间".into()),
        ];
        for case in cases {
            assert!(
                matches!(classify_stream(&case), Dial::Reconnect(_)),
                "{case} 必须走重连，不是把还活着的 shell 判死"
            );
        }
    }

    #[test]
    fn closed_reason_decodes_from_the_downlink_frame() {
        // 客户端只承认词表内的下行帧；`closed` 的 reason 必须是原样的字符串。
        let raw = r#"{"type":"closed","reason":"child_exited"}"#;
        let event: ShellStreamEvent = serde_json::from_str(raw).unwrap();
        let ShellStreamEvent::Closed { reason } = &event;
        assert_eq!(reason, "child_exited");
        assert!(serde_json::from_str::<ShellStreamEvent>(r#"{"type":"output"}"#).is_err());
    }

    fn http_response(status: StatusCode) -> Response<Option<Vec<u8>>> {
        // `Response::builder()` 只挂在 `Response<()>` 上（http 1.5），
        // 而 `Error::Http` 装的是 `Response<Option<Vec<u8>>>`——只能走 new + 改状态。
        let mut response: Response<Option<Vec<u8>>> = Response::new(None);
        *response.status_mut() = status;
        response
    }

    // ── 真实 socket 上的重连路径 ─────────────────────────────────────────
    //
    // 假服务端用 `accept_hdr_async` 记录每一次握手（URI + 两条鉴权头），按脚本
    // 决定这条连接怎么死。客户端跑的是生产读循环：`with_lease_for_test` 只换了
    // 端点与 lease 的来源，没有替换任何逻辑。
    //
    // 脚本 = 每条连接一个 `Plan`，与连接到达顺序一一对应（accept 循环先取脚本
    // 再 accept，因此不存在两条连接抢同一个 Plan 的非确定性）。

    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::http::Response;
    use tokio_tungstenite::tungstenite::handshake::server::{
        ErrorResponse, Request as ServerRequest, Response as ServerResponse,
    };

    #[derive(Debug)]
    struct Handshake {
        uri: String,
        authorization: Option<String>,
        session: Option<String>,
    }

    #[derive(Clone)]
    enum Plan {
        /// 回放这些字节 → 把客户端第一条上行帧转给测试 → **不发 closed** 就把 socket 拆了。
        ReplayThenBreak(&'static [u8]),
        /// 发 `closed{reason}` 后收摊。
        Closed(&'static str),
        /// 握手即 404（shell 不在了 / 不属于本身份）。
        NotFound,
    }

    /// 假 shell ws 服务端。脚本 = 每条连接一项，与连接到达顺序一一对应
    /// （accept 循环**先取脚本再 accept**，所以不存在两项脚本抢一条连接）。
    async fn spawn_fake_shell(
        script: Vec<Plan>,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<Handshake>>>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let (plan_tx, mut plan_rx) = mpsc::channel::<Plan>(8);
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (uplink_tx, uplink_rx) = mpsc::unbounded_channel();
        for plan in script {
            plan_tx.send(plan).await.expect("脚本入队");
        }
        drop(plan_tx);
        // 任务里用的是 clone：返回值还要把同一份记录交回测试（一个 Arc，两处视图）。
        let sink = Arc::clone(&recorded);
        tokio::spawn(async move {
            while let Some(plan) = plan_rx.recv().await {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let handshakes = Arc::clone(&sink);
                let uplink = uplink_tx.clone();
                let reject = matches!(plan, Plan::NotFound);
                tokio::spawn(async move {
                    // `#[expect]` 而不是 `#[allow]`：返回类型由 tungstenite 的 `Callback`
                    // 约定写死（Err 分支是 `Response<Option<String>>`），不由这条测试收窄；
                    // 哪天真能收窄了，expect 会自己红给我看。
                    #[expect(clippy::result_large_err)]
                    let callback = move |request: &ServerRequest, response: ServerResponse| {
                        handshakes.lock().expect("记录锁").push(Handshake {
                            uri: request.uri().to_string(),
                            authorization: request
                                .headers()
                                .get(AUTHORIZATION)
                                .map(|v| v.to_str().unwrap_or_default().to_string()),
                            session: request
                                .headers()
                                .get(CLIENT_SESSION_HEADER)
                                .map(|v| v.to_str().unwrap_or_default().to_string()),
                        });
                        if reject {
                            // 拒绝发生在**握手**里，不是升级成功后慢慢死——
                            // 与服务端 shell_ws.rs 的「attach 前判完」同一条纪律。
                            let mut refusal =
                                ErrorResponse::new(Some("shell_not_found".to_string()));
                            *refusal.status_mut() = StatusCode::NOT_FOUND;
                            return Err(refusal);
                        }
                        Ok(response)
                    };
                    let Ok(mut ws) = accept_hdr_async(socket, callback).await else {
                        return;
                    };
                    match plan {
                        Plan::ReplayThenBreak(tail) => {
                            let _ = ws.send(Message::binary(tail)).await;
                            // 转交第一条上行帧，然后**裸拆**（不发 close 帧）：
                            // 这就是「daemon 那边断网了，但 shell 还活着」的形状。
                            while let Some(Ok(message)) = ws.next().await {
                                if let Message::Text(text) = message {
                                    let _ = uplink.send(text.to_string());
                                    break;
                                }
                            }
                            drop(ws);
                        }
                        Plan::Closed(reason) => {
                            let frame = ShellStreamEvent::Closed {
                                reason: reason.to_string(),
                            };
                            let _ = ws
                                .send(Message::text(serde_json::to_string(&frame).unwrap()))
                                .await;
                            let _ = ws.close(None).await;
                        }
                        Plan::NotFound => {}
                    }
                });
            }
        });
        (format!("http://{addr}"), recorded, uplink_rx)
    }

    /// 收集回调到三条 channel，便于按顺序断言（含 Open/Reconnecting 的节奏）。
    /// 三条都带 shell_id：回调签名带它，测试就不该假装它不存在。
    type Collectors = (
        ShellStreamHandlers,
        mpsc::UnboundedReceiver<(String, Vec<u8>)>,
        mpsc::UnboundedReceiver<(String, ShellStreamEvent)>,
        mpsc::UnboundedReceiver<(String, ShellStreamStatus)>,
    );

    fn collector() -> Collectors {
        let (bytes_tx, bytes_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        (
            ShellStreamHandlers {
                on_bytes: Arc::new(move |shell_id, bytes| {
                    let _ = bytes_tx.send((shell_id, bytes));
                }),
                on_event: Arc::new(move |shell_id, event| {
                    let _ = event_tx.send((shell_id, event));
                }),
                on_status: Arc::new(move |shell_id, status| {
                    let _ = status_tx.send((shell_id, status));
                }),
            },
            bytes_rx,
            event_rx,
            status_rx,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_resumes_from_the_bytes_already_delivered() {
        let (base_url, handshakes, mut uplink_rx) = spawn_fake_shell(vec![
            Plan::ReplayThenBreak(b"hi"),
            Plan::Closed("child_exited"),
        ])
        .await;
        let client =
            Client::with_lease_for_test(&base_url, "tok3n", None, "cs_test").await;
        let (handlers, mut bytes_rx, mut event_rx, mut status_rx) = collector();

        let stream = ShellStream::start("shell_01", &client, handlers)
            .await
            .expect("能力位由 adopt_for_test 置上");

        // 首帧：整段有界回放尾（不带 from_offset）→ 2 个字节 → 原样交给上层。
        let (shell_id, bytes) = bytes_rx.recv().await.expect("下行裸字节");
        assert_eq!(shell_id, "shell_01");
        assert_eq!(bytes, b"hi".to_vec());

        // 上行 keystroke：服务端看到的必须是 base64 的 text 帧。
        stream.stdin(b"exit\r\n").await.expect("stdin 入队");
        let uplink = uplink_rx.recv().await.expect("上行帧");
        assert_eq!(uplink, r#"{"type":"stdin","bytes":"ZXhpdA0K"}"#);

        // 等状态走到 Open（这条连接已建立，delivered 此刻是 0——Open 报的是连接
        // 建立时的水位）。
        let mut saw_open = false;
        while let Some((shell_id, status)) = status_rx.recv().await {
            assert_eq!(shell_id, "shell_01", "回调里的 shell_id 必须是订阅时那一个");
            match status {
                ShellStreamStatus::Open { .. } => {
                    saw_open = true;
                    break;
                }
                ShellStreamStatus::Connecting => {}
                other => panic!("意料之外的状态（Open 之前）: {other:?}"),
            }
        }
        assert!(saw_open);

        // 假服务端裸拆 socket ⇒ 重连，且**第二次握手带 from_offset=2**。
        // 退避基准 1s，所以这里等的是真实节奏，不是自己造的快路径。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let uris = handshakes.lock().expect("记录锁").iter().map(|h| h.uri.clone()).collect::<Vec<_>>();
            if uris.len() >= 2 {
                assert_eq!(uris[0], "/ringing/v2/shells/shell_01/stream");
                assert_eq!(
                    uris[1], "/ringing/v2/shells/shell_01/stream?from_offset=2",
                    "重连必须带上已交付的水位，否则回放会重复刷屏（§D3 约束 3）"
                );
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("8s 内没有看到第二次握手（重连没发生？）: {uris:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 两条握手的鉴权头都得在，值取自 lease（不是测试自己塞的第二个来源）。
        // 作用域包起来：std 的 MutexGuard 不许活过下一个 await（future 会变 !Send，
        // 而 clippy 的 await_holding_lock 就是钉这件事的）。
        {
            let recorded = handshakes.lock().expect("记录锁");
            for handshake in recorded.iter() {
                assert_eq!(handshake.authorization.as_deref(), Some("Bearer tok3n"));
                assert_eq!(handshake.session.as_deref(), Some("cs_test"));
            }
        }

        // 第二条连接发 closed ⇒ 上层拿到协议事实，且**不再重连**。
        let event = event_rx.recv().await.expect("closed 帧");
        assert_eq!(
            event,
            (
                "shell_01".to_string(),
                ShellStreamEvent::Closed {
                    reason: "child_exited".to_string()
                }
            )
        );
        let mut terminal = None;
        while let Some((_, status)) = status_rx.recv().await {
            if let ShellStreamStatus::Closed { reason } = status {
                terminal = Some(reason);
                break;
            }
        }
        assert_eq!(terminal.as_deref(), Some("child_exited"));
        // 再等一下，确认没有第三次握手（终止事实之后不许复活这条流）。
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            handshakes.lock().expect("记录锁").len(),
            2,
            "收到 closed 之后还在重连：把终止事实当成了瞬态断线"
        );
        stream.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_404_ends_the_stream_without_retrying() {
        let (base_url, handshakes, _uplink_rx) = spawn_fake_shell(vec![Plan::NotFound]).await;
        let client = Client::with_lease_for_test(&base_url, "tok3n", None, "cs_test").await;
        let (handlers, _bytes_rx, _event_rx, mut status_rx) = collector();

        // 能力位是 true（本机的 lease），但服务端对这个 shell_id 给 404——
        // 「不存在」与「不是你的」同一个码，客户端据此收摊，不猜第二轮。
        let stream = ShellStream::start("shell_gone", &client, handlers)
            .await
            .expect("订阅本身不碰 shell 存在性");
        let mut reason = None;
        while let Some((shell_id, status)) = status_rx.recv().await {
            assert_eq!(shell_id, "shell_gone");
            if let ShellStreamStatus::Closed { reason: tail } = status {
                reason = Some(tail);
                break;
            }
        }
        assert_eq!(reason.as_deref(), Some("shell_not_found"));
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(handshakes.lock().expect("记录锁").len(), 1);
        // 流已终止：继续打字必须**报错**，不能静默吞掉按键（宪法 5）。
        let error = stream.stdin(b"ls\r\n").await.unwrap_err();
        assert!(error.to_string().contains("已终止"), "{error}");
    }
}
