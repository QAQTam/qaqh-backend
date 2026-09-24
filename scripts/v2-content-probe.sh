#!/usr/bin/env bash
# #345 真机探针：pending ask 的 modal 正文能否从 canonical ref 取回。
#
# 链路全真：真实 daemon + 本地 OpenAI-compatible fake provider（按脚本下发
# `ask` tool_call）→ 引擎挂起回合 → canonical `InteractionRequested` →
# v2 bootstrap 拿到 `request` ref → `GET /ringing/v2/content/{ref}` 取回正文。
#
# 判据（缺一即红）：
#   ① bootstrap 的 pending interaction 带 `ContentValue::Ref`；
#   ② 用该 ref 取 content 拿到 200，且 body 是 `kind=ask` 的正文（问题文本对得上）；
#   ③ 裸 hex 形态（strip `sha256:`）同样可取；
#   ④ **第二个 client session**（同进程内重连路径）resume 后 bootstrap 得到同一个
#      ref，并能取到同一份正文；
#   ⑤ v1 content 路由已硬切（404）。
#
# 用法：scripts/v2-content-probe.sh [data-root]
# data-root 必须以 `qaqh` 结尾（daemon 安全规则）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="${HOME:-/home/$(id -un)}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"

DATA="${1:-$HOME/.qaqh-content-probe/qaqh}"
case "$DATA" in
    *qaqh) ;;
    *) echo "data root 必须以 qaqh 结尾：$DATA" >&2; exit 1 ;;
esac
mkdir -p "$DATA/sessions"

echo "== building daemon =="
cargo build -q -p qaqh-daemon

export QAQH_CONTENT_PROBE_ROOT="$ROOT"
export QAQH_CONTENT_PROBE_DATA="$DATA"
python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(os.environ["QAQH_CONTENT_PROBE_ROOT"])
DATA = pathlib.Path(os.environ["QAQH_CONTENT_PROBE_DATA"])
WORK = DATA.parent / "work"
WORK.mkdir(parents=True, exist_ok=True)
QUESTION = "Proceed with the #345 content probe?"
DAEMON = ROOT / "target" / "debug" / "qaqh-daemon"


class FakeProvider(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        # 标题生成等非流式旁路请求不参与脚本计数。
        if not payload.get("stream"):
            body = json.dumps(
                {"choices": [{"message": {"role": "assistant", "content": "content probe"}}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        turn = self.server.main_calls
        self.server.main_calls += 1
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if turn == 0:
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_probe_ask",
                                        "type": "function",
                                        "function": {
                                            "name": "ask",
                                            "arguments": json.dumps(
                                                {
                                                    "question": QUESTION,
                                                    "options": ["yes", "no"],
                                                    "allow_custom": False,
                                                }
                                            ),
                                        },
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": "done"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.02)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
server.main_calls = 0
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()
print(f"fake provider: http://127.0.0.1:{port}/v1")

(DATA / "config.toml").write_text(
    f'''provider_id = "openai"
active_profile = "default"
permission_level = 3

[profiles.default]
model = "fake-model"
max_tokens = 4096
effort = "low"
context_limit = 100000
base_url = "http://127.0.0.1:{port}/v1"
endpoint = "openai"
'''
)

daemon_env = os.environ.copy()
daemon_env["QAQH_DATA_DIR"] = str(DATA)
daemon = subprocess.Popen(
    [str(DAEMON), "run"],
    stdin=subprocess.DEVNULL,
    stdout=(DATA.parent / "daemon.out").open("ab"),
    stderr=subprocess.STDOUT,
    env=daemon_env,
    start_new_session=True,
)


def stop_daemon():
    try:
        daemon.terminate()
        daemon.wait(timeout=5)
    except Exception:
        daemon.kill()


deadline = time.monotonic() + 20
discovery = None
while time.monotonic() < deadline:
    try:
        discovery = json.loads((DATA / "daemon.json").read_text())
    except (OSError, json.JSONDecodeError):
        time.sleep(0.2)
        continue
    if discovery.get("pid") == daemon.pid:
        break
else:
    stop_daemon()
    raise SystemExit("daemon did not publish daemon.json")

ENDPOINT = discovery["endpoint"]
TOKEN = discovery["token"]
print(f"daemon pid={daemon.pid} endpoint={ENDPOINT}")


def request(method, path, session=None, body=None, raw=False):
    data = None
    headers = {"authorization": f"Bearer {TOKEN}"}
    if session:
        headers["x-qaqh-client-session-id"] = session
    if body is not None:
        data = json.dumps(body).encode()
        headers["content-type"] = "application/json"
    req = urllib.request.Request(ENDPOINT + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=10) as response:
            payload = response.read()
            return response.status, (payload if raw else json.loads(payload or b"{}"))
    except urllib.error.HTTPError as error:
        payload = error.read()
        if raw:
            return error.code, payload
        try:
            return error.code, json.loads(payload or b"{}")
        except json.JSONDecodeError:
            return error.code, {"raw": payload.decode(errors="replace")}


def open_client(instance):
    status, body = request(
        "POST",
        "/ringing/v2/clients/open",
        body={"schema": "qaqh.Ringing", "version": 2, "client_instance_id": instance},
    )
    assert status == 200, f"open failed: {status} {body}"
    return body["client_session_id"]


def command(session, channel, command_body, seed=None, instance="content-probe"):
    envelope = {
        "schema": "qaqh.Ringing",
        "version": 2,
        "channel": channel,
        "command_id": str(uuid.uuid4()),
        "client_instance_id": instance,
        "client_session_id": session,
        "command": command_body,
    }
    if seed:
        envelope["seed"] = seed
    return request("POST", f"/ringing/v2/commands/{channel}", session=session, body=envelope)


def bootstrap(session, seed):
    return request("GET", f"/ringing/v2/sessions/{seed}/bootstrap", session=session)


def wait_until(predicate, what, timeout=30.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(0.25)
    stop_daemon()
    raise SystemExit(f"timeout waiting for {what} (last={last!r})")


failures = []


def check(name, ok, detail=""):
    print(f"  [{'✓' if ok else '✗'}] {name}{(' — ' + detail) if detail else ''}")
    if not ok:
        failures.append(name)


try:
    client_a = open_client("content-probe-a")
    before = {entry.name for entry in (DATA / "sessions").iterdir()}
    status, ack = command(
        client_a,
        "control",
        {"channel": "control", "type": "session_create", "close_current": False, "cwd": str(WORK)},
        instance="content-probe-a",
    )
    assert status == 200 and ack.get("status") == "accepted", f"session_create: {status} {ack}"
    created = {entry.name for entry in (DATA / "sessions").iterdir()} - before
    created.discard("index.jsonl")
    assert len(created) == 1, f"unexpected new sessions: {created}"
    seed = created.pop()
    print(f"seed={seed}")

    # driver 席位：conversation 命令受 not_driver gate 保护。
    status, claim = request(
        "POST",
        f"/ringing/v2/sessions/{seed}/driver/claim",
        session=client_a,
    )
    assert status == 200 and claim.get("accepted") is True, f"driver claim: {status} {claim}"
    wait_until(
        lambda: (
            (boot := bootstrap(client_a, seed))
            and boot[0] == 200
            and boot[1].get("control", {}).get("state", {}).get("driver", {}).get("holder")
            == client_a
        ),
        "driver seat",
    )

    status, ack = command(
        client_a,
        "conversation",
        {
            "channel": "conversation",
            "type": "conversation_send_message",
            "text": "ask me something",
        },
        seed=seed,
        instance="content-probe-a",
    )
    assert status == 200, f"send_message: {status} {ack}"

    def pending(session):
        status, body = bootstrap(session, seed)
        if status != 200 or "control" not in body:
            return None
        interactions = body["control"]["state"]["interactions"]
        return interactions[0] if interactions else None

    interaction = wait_until(lambda: pending(client_a), "pending interaction")
    print(f"pending interaction: {json.dumps(interaction)[:600]}")
    request_value = interaction["request"]
    check("① bootstrap 带 ContentValue::Ref", request_value.get("kind") == "ref", str(request_value))
    ref = request_value["data"]["content_ref"]
    check("   ref 是 sha256: 形态", ref.startswith("sha256:") and len(ref) == 71, ref)

    status, body = request("GET", f"/ringing/v2/content/{ref}", session=client_a, raw=True)
    body_json = json.loads(body) if status == 200 else {}
    check(
        "② 用 canonical ref 取回 ask 正文",
        status == 200
        and body_json.get("kind") == "ask"
        and body_json.get("questions", [{}])[0].get("question") == QUESTION,
        f"status={status} body={body_json}",
    )

    bare = ref.split(":", 1)[1]
    status_bare, body_bare = request("GET", f"/ringing/v2/content/{bare}", session=client_a, raw=True)
    check("③ 裸 hex 形态同样可取", status_bare == 200 and body_bare == body, f"status={status_bare}")

    # 同进程内「换一个 client session 重连」：resume 后 bootstrap 应拿到同一个 ref。
    client_b = open_client("content-probe-b")
    status, ack = command(
        client_b,
        "control",
        {"channel": "control", "type": "session_resume", "seed": seed},
        seed=seed,
        instance="content-probe-b",
    )
    assert status == 200, f"session_resume: {status} {ack}"
    reconnected = wait_until(lambda: pending(client_b), "pending interaction after reconnect")
    ref_b = reconnected["request"]["data"]["content_ref"]
    status, body_b = request("GET", f"/ringing/v2/content/{ref_b}", session=client_b, raw=True)
    check(
        "④ 第二个 client session 重建 modal 正文",
        ref_b == ref and status == 200 and body_b == body,
        f"ref_match={ref_b == ref} status={status}",
    )

    status, _ = request("GET", f"/ringing/v1/content/{ref}", session=client_a, raw=True)
    check("⑤ v1 content 路由已硬切", status == 404, f"status={status}")
finally:
    stop_daemon()

print()
if failures:
    print("RESULT: FAIL —", ", ".join(failures))
    raise SystemExit(1)
print("RESULT: PASS")
PY
