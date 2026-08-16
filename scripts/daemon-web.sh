#!/usr/bin/env bash
# daemon-web.sh — 启动 QAQ-Harness 局域网 Web 服务（Linux/macOS）
# 用法: bash scripts/daemon-web.sh [PORT]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${1:-64413}"
DIST="$ROOT/apps/web/dist"

if [ -x "$ROOT/target/release/qaqh-daemon" ]; then
  DAEMON="$ROOT/target/release/qaqh-daemon"
else
  DAEMON="$ROOT/target/debug/qaqh-daemon"
fi

# ── 前置检查 ──────────────────────────────────────────────────────────────
if [ ! -f "$DIST/index.html" ]; then
  echo "错误: web 产物缺失 ($DIST)" >&2
  echo "请先构建: cd apps/web && npm install && npm run build" >&2
  exit 1
fi
if [ ! -x "$DAEMON" ]; then
  echo "错误: daemon 未构建 ($DAEMON)" >&2
  echo "请先构建: cargo build -p qaqh-daemon" >&2
  exit 1
fi

# 单实例检查
LOCK="${HOME}/.deepx/daemon.lock"
if [ -f "$LOCK" ]; then
  LOCK_PID="$(cat "$LOCK" 2>/dev/null || true)"
  if [ -n "$LOCK_PID" ] && kill -0 "$LOCK_PID" 2>/dev/null; then
    echo "错误: 已有 daemon 在运行 (pid $LOCK_PID)" >&2
    exit 1
  fi
  rm -f "$LOCK"
fi

# ── 局域网 IP ─────────────────────────────────────────────────────────────
# 优先取默认路由网卡（真实局域网 IP），避免选中虚拟网卡（docker/VM）
IP="$(ip route get 1 2>/dev/null | awk '{print $7; exit}' || true)"
[ -z "$IP" ] && IP="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
[ -z "$IP" ] && IP="127.0.0.1"

# ── Token ─────────────────────────────────────────────────────────────────
TOKEN="$(openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"

echo ""
echo "══════════════════════════════════════════════════"
echo "  QAQ-Harness · 局域网 Web"
echo "  访问地址 : http://${IP}:${PORT}/debug/"
echo "  token    : $TOKEN"
echo "  按 Ctrl+C 停止"
echo "══════════════════════════════════════════════════"
echo ""

# ── 启动 daemon（前台，Ctrl+C 停止）────────────────────────────────────────
export QAQH_DEBUG_RENDERER_DIR="$DIST"
export QAQH_SERVER_TOKEN="$TOKEN"
exec "$DAEMON" server --bind 0.0.0.0 --port "$PORT"
