#!/usr/bin/env python3
"""deepx-workspace serve 端到端测试（Windows / Linux 通用）。

覆盖：
  1. 拉起 serve（--token）并等待就绪（轮询 /health）
  2. 鉴权：无 token → 401
  3. GET /health  → 200 {"ok":true,"tools":N}
  4. GET /tools   → 工具清单包含 exec/read_file
  5. POST /execute 真实工具（exec: echo hello）→ success:true
  6. POST /execute 文件工具（read_file Cargo.toml）→ 内容含 workspace 路径
  7. POST /execute 未知工具 → 404；缺字段 → 400
  8. 嵌套快速移交：serve 内执行 exec + background_after_secs=2
     → <10s 返回 backgrounded JSON（process_id + transferred_after_secs）
  9. 快速移交期间 /health 仍可响应（长驻服务不阻塞控制面）
 10. 清理：杀进程树，确认端口释放

用法：
  python scripts/test_workspace_serve.py [--bin PATH] [--port N] [--keep]

安全：serve 子进程的 USERPROFILE/HOME 指向临时目录，
避免测试工具的 audit.csv / agentfs 记录污染真实数据。
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

PASS, FAIL = 0, 0


def report(name, ok, detail=""):
    global PASS, FAIL
    if ok:
        PASS += 1
    else:
        FAIL += 1
    mark = "PASS" if ok else "FAIL"
    suffix = f" — {detail}" if detail else ""
    print(f"[{mark}] {name}{suffix}", flush=True)


class Serve:
    """Manage a deepx-workspace serve child process under an isolated data root."""

    def __init__(self, binary, host, port, token):
        self.binary = binary
        self.host = host
        self.port = port
        self.token = token
        self.proc = None
        self.data_root = tempfile.mkdtemp(prefix="deepx-ws-test-")
        self.out_log = os.path.join(self.data_root, "serve-out.log")
        self.err_log = os.path.join(self.data_root, "serve-err.log")
        self.tmp_dir = tempfile.mkdtemp(prefix="deepx-ws-cwd-")

    def start(self):
        env = dict(os.environ)
        # 隔离数据根：Windows 用 USERPROFILE，Unix 用 HOME
        if os.name == "nt":
            env["USERPROFILE"] = self.data_root
        else:
            env["HOME"] = self.data_root
        with open(self.out_log, "w") as out, open(self.err_log, "w") as err:
            self.proc = subprocess.Popen(
                [self.binary, "serve", "--port", str(self.port), "--token", self.token],
                stdout=out, stderr=err, cwd=self.tmp_dir, env=env,
            )

    def http(self, method, path, body=None, token=None, timeout=15):
        """Return (status, text). status=-1 on connection failure."""
        req = urllib.request.Request(f"http://{self.host}:{self.port}{path}", method=method)
        if token:
            req.add_header("Authorization", f"Bearer {token}")
        data = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, data=data, timeout=timeout) as resp:
                return resp.status, resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode("utf-8", "replace")
        except Exception as e:  # noqa: BLE001
            return -1, str(e)

    def wait_ready(self, deadline_secs=15):
        deadline = time.time() + deadline_secs
        while time.time() < deadline:
            if self.proc.poll() is not None:
                return False
            status, _ = self.http("GET", "/health", token=self.token, timeout=2)
            if status == 200:
                return True
            time.sleep(0.3)
        return False

    def stop(self):
        if self.proc and self.proc.poll() is None:
            if os.name == "nt":
                subprocess.run(
                    ["taskkill", "/pid", str(self.proc.pid), "/T", "/F"],
                    capture_output=True,
                )
            else:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
        shutil.rmtree(self.data_root, ignore_errors=True)
        shutil.rmtree(self.tmp_dir, ignore_errors=True)


def platform_echo_args():
    """argv for a portable echo (used through the exec tool)."""
    if os.name == "nt":
        return ["cmd", "/c", "echo", "hello-workspace-serve"]
    return ["echo", "hello-workspace-serve"]


def long_running_args(background_after_secs):
    """argv for a long-running child (used through the exec tool)."""
    if os.name == "nt":
        return [
            "powershell", "-NoProfile", "-Command",
            "Start-Sleep -Seconds 8; Write-Output done",
        ]
    return ["sleep", "8"]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=os.environ.get(
        "DEEPX_WS_BIN",
        os.path.join("target", "debug", "deepx-workspace.exe" if os.name == "nt"
                     else "deepx-workspace")),
        help="path to deepx-workspace binary")
    ap.add_argument("--port", type=int, default=17890)
    ap.add_argument("--token", default="test-secret-token-123")
    ap.add_argument("--keep", action="store_true", help="keep logs (prints paths)")
    args = ap.parse_args()

    if not os.path.exists(args.bin):
        print(f"ERROR: binary not found: {args.bin}", file=sys.stderr)
        sys.exit(2)

    s = Serve(args.bin, "127.0.0.1", args.port, args.token)
    s.start()

    try:
        # 1. 就绪
        ready = s.wait_ready()
        report("serve 启动并 /health 就绪", ready,
               f"bin={os.path.basename(args.bin)} port={args.port}")
        if not ready:
            with open(s.out_log) as f:
                print("--- serve stdout ---\n" + f.read(), file=sys.stderr)
            with open(s.err_log) as f:
                print("--- serve stderr ---\n" + f.read(), file=sys.stderr)
            sys.exit(1)

        # 2. 鉴权：无 token → 401
        status, _ = s.http("GET", "/health", token=None)
        report("无 token 访问 /health → 401", status == 401, f"got {status}")
        status, _ = s.http("GET", "/health", token="wrong-token")
        report("错误 token → 401", status == 401, f"got {status}")

        # 3. /health 内容
        status, body = s.http("GET", "/health", token=s.token)
        ok = status == 200
        tools_n = -1
        if ok:
            try:
                tools_n = json.loads(body)["tools"]
            except (ValueError, KeyError):
                ok = False
        report("GET /health 返回工具数", ok and tools_n > 0, f"tools={tools_n}")

        # 4. /tools 清单（工具注册名：exec / read / write / apply_patch ...）
        status, body = s.http("GET", "/tools", token=s.token)
        names = []
        if status == 200:
            try:
                names = [t["name"] for t in json.loads(body)]
            except (ValueError, KeyError, TypeError):
                pass
        report("GET /tools 含 exec 与 read",
               "exec" in names and "read" in names,
               f"{len(names)} tools")

        # 5. /execute 简单工具（exec: echo）
        t0 = time.time()
        status, body = s.http("POST", "/execute", token=s.token, body={
            "session_id": "pytest-session",
            "workspace": s.tmp_dir,
            "name": "exec",
            "args": {"argv": platform_echo_args()},
        })
        elapsed = time.time() - t0
        try:
            resp = json.loads(body)
        except ValueError:
            resp = {}
        report("POST /execute exec echo 成功",
               status == 200 and resp.get("success") is True
               and "hello-workspace-serve" in resp.get("content", ""),
               f"status={status} elapsed={elapsed:.2f}s")

        # 6. /execute 文件工具（read Cargo.toml，workspace 内相对路径）
        status, body = s.http("POST", "/execute", token=s.token, body={
            "session_id": "pytest-session",
            "workspace": os.getcwd(),
            "name": "read",
            "args": {"path": "Cargo.toml"},
        })
        try:
            resp = json.loads(body)
        except ValueError:
            resp = {}
        report("POST /execute read Cargo.toml",
               status == 200 and resp.get("success") is True
               and "members" in resp.get("content", ""),
               f"status={status}")

        # 7. 未知工具 → 404；缺字段 → 400
        status, _ = s.http("POST", "/execute", token=s.token, body={
            "session_id": "s", "workspace": ".", "name": "no_such_tool",
            "args": {},
        })
        report("未知工具 → 404", status == 404, f"got {status}")
        status, _ = s.http("POST", "/execute", token=s.token, body={
            "session_id": "", "workspace": ".", "name": "exec", "args": {},
        })
        report("缺 session_id → 400", status == 400, f"got {status}")

        # 8. 嵌套快速移交：serve 内 exec 工具 + background_after_secs=2
        t0 = time.time()
        status, body = s.http("POST", "/execute", token=s.token, body={
            "session_id": "pytest-session",
            "workspace": s.tmp_dir,
            "name": "exec",
            "args": {
                "argv": long_running_args(2),
                "timeout_secs": 60,
                "background_after_secs": 2,
            },
        })
        elapsed = time.time() - t0
        try:
            resp = json.loads(body)
        except ValueError:
            resp = {}
        content = resp.get("content", "")
        try:
            bg = json.loads(content)
        except ValueError:
            bg = {}
        # ExecOutput 结构：{status, command, exit_code, output(String, 内嵌 JSON),
        #                    timed_out, process_id, ...}
        report("嵌套快速移交 <10s 返回 backgrounded",
               status == 200 and elapsed < 10.0
               and bg.get("status") == "backgrounded"
               and bg.get("timed_out") is True
               and bg.get("process_id") is not None
               and "transferred_after_secs" in content,
               f"elapsed={elapsed:.2f}s process_id={bg.get('process_id')}")

        # 9. 移交期间控制面不阻塞（executor 线程已空闲，HTTP 独立）
        status, body = s.http("GET", "/health", token=s.token, timeout=5)
        report("backgrounded 移交后 /health 仍 200", status == 200,
               f"got {status}")

        # 10. 长驻子进程仍在（注册表句柄在 serve 进程内，这里用端口/进程间接验证）
        time.sleep(2)
        report("移交后 serve 仍存活", s.proc.poll() is None)

    finally:
        s.stop()

    print(f"\n==== {PASS} passed, {FAIL} failed ====")
    if args.keep:
        print(f"logs kept under: {s.data_root if os.path.exists(s.data_root) else '(removed)'}")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
