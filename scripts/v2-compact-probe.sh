#!/usr/bin/env bash
# Ringing v2 compact archive probe.
#
# 真机链路：daemon + 本地 OpenAI-compatible fake provider →
# conversation_compact → messages.jsonl 追加 [Compacted] + meta 水位 →
# daemon 重启 → 下一轮模型请求只看到摘要与水位后的消息。
#
# 判据（缺一即红）：
#   ① 压缩后 messages.jsonl 仍保留旧前缀，并追加带新 msg_id 的摘要；
#   ② meta.compact_covered_through_msg_id 已落盘；
#   ③ 不创建 compact-context.json（第二真源）；
#   ④ 重启后的下一轮 provider 请求含摘要、不含被压缩前缀。
#
# 用法：scripts/v2-compact-probe.sh [data-root]
# data-root 必须以 `qaqh` 结尾（daemon 安全规则）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="${HOME:-/home/$(id -un)}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"

DATA="${1:-$HOME/.qaqh-compact-probe/qaqh}"
case "$DATA" in
    *qaqh) ;;
    *) echo "data root 必须以 qaqh 结尾：$DATA" >&2; exit 1 ;;
esac

echo "== building daemon =="
cargo build -q -p qaqh-daemon

export QAQH_COMPACT_PROBE_ROOT="$ROOT"
export QAQH_COMPACT_PROBE_DATA="$DATA"
python3 - <<'PY'
import json
import os
import pathlib
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(os.environ["QAQH_COMPACT_PROBE_ROOT"])
DATA = pathlib.Path(os.environ["QAQH_COMPACT_PROBE_DATA"])
WORK = DATA.parent / "work"
DAEMON = ROOT / "target" / "debug" / "qaqh-daemon"

OLD = "OLD_PREFIX_SENTINEL_7d3f"
KEEP = "KEEP_SENTINEL_91ab"
AFTER = "AFTER_RESTART_SENTINEL_c25e"
SUMMARY = "COMPACT_SUMMARY_SENTINEL_f81c"

shutil.rmtree(DATA.parent, ignore_errors=True)
DATA.mkdir(parents=True)
WORK.mkdir(parents=True)


class FakeProvider(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        if not payload.get("stream"):
            body = json.dumps(
                {"choices": [{"message": {"role": "assistant", "content": "compact probe"}}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        messages = payload.get("messages", [])
        self.server.stream_requests.append(messages)
        blob = json.dumps(messages, ensure_ascii=False)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if "CONTEXT CHECKPOINT COMPACTION" in blob or "structured handoff summary" in blob:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "content": (
                                    "### Decision Log\n"
                                    f"- **Decision**: {SUMMARY}\n"
                                    "- **Status**: done\n\n"
                                    "### State Snapshot\n"
                                    "- **Key files**: probe\n"
                                    "- **Build status**: pass\n"
                                    "- **Last successful action**: compact probe\n\n"
                                    "### Remaining Work\n"
                                    "- [small] none\n"
                                ),
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        else:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": "ack"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.01)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
server.stream_requests = []
threading.Thread(target=server.serve_forever, daemon=True).start()
port = server.server_address[1]
print(f"fake provider: http://127.0.0.1:{port}/v1")

(DATA / "config.toml").write_text(
    f'''provider_id = "openai"
active_profile = "default"
permission_level = 3

[profiles.default]
model = "fake-model"
max_tokens = 4096
effort = "low"
context_limit = 200000
base_url = "http://127.0.0.1:{port}/v1"
endpoint = "openai"
'''
)

daemon = None


def start_daemon():
    global daemon
    try:
        (DATA / "daemon.json").rename(DATA / "daemon.json.prev")
    except FileNotFoundError:
        pass
    env = os.environ.copy()
    env["QAQH_DATA_DIR"] = str(DATA)
    daemon = subprocess.Popen(
        [str(DAEMON), "run"],
        stdin=subprocess.DEVNULL,
        stdout=(DATA.parent / "daemon.out").open("ab"),
        stderr=subprocess.STDOUT,
        env=env,
        start_new_session=True,
    )
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        try:
            discovery = json.loads((DATA / "daemon.json").read_text())
        except (OSError, json.JSONDecodeError):
            time.sleep(0.1)
            continue
        if discovery.get("pid") == daemon.pid:
            return discovery
        time.sleep(0.1)
    raise SystemExit("daemon did not publish discovery")


def stop_daemon():
    global daemon
    if daemon is not None:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait()
        daemon = None


def request(method, path, session=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"authorization": f"Bearer {token}"}
    if session:
        headers["x-qaqh-client-session-id"] = session
    if data is not None:
        headers["content-type"] = "application/json"
    req = urllib.request.Request(endpoint + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            return response.status, json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            parsed = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            parsed = {"raw": raw.decode(errors="replace")}
        return error.code, parsed


def open_client(name):
    status, body = request(
        "POST",
        "/ringing/v2/clients/open",
        body={"schema": "qaqh.Ringing", "version": 2, "client_instance_id": name},
    )
    assert status == 200, f"open failed: {status} {body}"
    return body["client_session_id"]


def command(session, channel, body, seed=None, name="compact-probe"):
    envelope = {
        "schema": "qaqh.Ringing",
        "version": 2,
        "channel": channel,
        "command_id": str(uuid.uuid4()),
        "client_instance_id": name,
        "client_session_id": session,
        "command": body,
    }
    if seed:
        envelope["seed"] = seed
    return request("POST", f"/ringing/v2/commands/{channel}", session=session, body=envelope)


def wait_for(predicate, what, timeout=30.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(0.2)
    raise SystemExit(f"timeout waiting for {what}: {last!r}")


try:
    discovery = start_daemon()
    endpoint = discovery["endpoint"]
    token = discovery["token"]

    client = open_client("compact-probe-a")
    before = {entry.name for entry in (DATA / "sessions").iterdir()}
    status, ack = command(
        client,
        "control",
        {
            "channel": "control",
            "type": "session_create",
            "close_current": False,
            "cwd": str(WORK),
        },
        name="compact-probe-a",
    )
    assert status == 200 and ack.get("status") == "accepted", f"session_create: {status} {ack}"
    created = {entry.name for entry in (DATA / "sessions").iterdir()} - before
    created.discard("index.jsonl")
    assert len(created) == 1, f"unexpected new sessions: {created}"
    seed = created.pop()
    print(f"seed={seed}")

    status, claim = request(
        "POST", f"/ringing/v2/sessions/{seed}/driver/claim", session=client
    )
    assert status == 200 and claim.get("accepted") is True, f"driver claim: {status} {claim}"

    padding = " ".join(["context padding token"] * 5000)
    for marker in (OLD, KEEP):
        status, ack = command(
            client,
            "conversation",
            {
                "channel": "conversation",
                "type": "conversation_send_message",
                "text": f"{marker} {padding}",
            },
            seed=seed,
            name="compact-probe-a",
        )
        assert status == 200, f"send_message {marker}: {status} {ack}"
        wait_for(
            lambda marker=marker: (
                "assistant" in (DATA / "sessions" / seed / "messages.jsonl").read_text()
                and marker in (DATA / "sessions" / seed / "messages.jsonl").read_text()
            ),
            f"turn {marker}",
        )
        time.sleep(0.5)

    status, ack = command(
        client,
        "conversation",
        {"channel": "conversation", "type": "conversation_compact", "turn_id": None},
        seed=seed,
        name="compact-probe-a",
    )
    assert status == 200, f"conversation_compact: {status} {ack}"

    def compacted():
        try:
            meta = json.loads((DATA / "sessions" / seed / "meta.json").read_text())
            archive = (DATA / "sessions" / seed / "messages.jsonl").read_text()
        except OSError:
            return None
        if meta.get("compact_covered_through_msg_id") is None or "[Compacted " not in archive:
            return None
        return meta, archive

    meta, archive = wait_for(compacted, "compact watermark")
    assert OLD in archive and SUMMARY in archive, "archive must retain old history and append summary"
    assert not (DATA / "sessions" / seed / "compact-context.json").exists(), (
        "route 1 must not create a second compact truth source"
    )
    print(
        f"compact ok: covered={meta['compact_covered_through_msg_id']} "
        f"archive_lines={len(archive.splitlines())}"
    )

    stop_daemon()
    discovery = start_daemon()
    endpoint = discovery["endpoint"]
    token = discovery["token"]
    client = open_client("compact-probe-b")
    status, ack = command(
        client,
        "control",
        {"channel": "control", "type": "session_resume", "seed": seed},
        seed=seed,
        name="compact-probe-b",
    )
    assert status == 200, f"session_resume: {status} {ack}"
    status, claim = request(
        "POST", f"/ringing/v2/sessions/{seed}/driver/claim", session=client
    )
    assert status == 200, f"driver claim after restart: {status} {claim}"

    before_calls = len(server.stream_requests)
    status, ack = command(
        client,
        "conversation",
        {
            "channel": "conversation",
            "type": "conversation_send_message",
            "text": AFTER,
        },
        seed=seed,
        name="compact-probe-b",
    )
    assert status == 200, f"post-restart send_message: {status} {ack}"
    wait_for(lambda: len(server.stream_requests) > before_calls, "post-restart request")
    request_blob = json.dumps(server.stream_requests[-1], ensure_ascii=False)
    assert AFTER in request_blob, "new message missing from post-restart request"
    assert SUMMARY in request_blob, "summary missing from post-restart request"
    assert OLD not in request_blob, "compacted prefix resurrected after restart"
    print("post-restart model context: summary present, old prefix absent")
    print("RESULT: PASS compact archive probe")
finally:
    stop_daemon()
PY
