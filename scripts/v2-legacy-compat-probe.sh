#!/usr/bin/env bash
# Ringing v2 legacy migrate-on-read 存量审计探针（P2 前置）。
#
# 用途：删除任何一条"读取旧格式并惰性迁移"的兼容路径之前，先跑本探针确认**存量数据里
# 已经没有那种形状**。审计口径来自 `docs/audit-legacy-protocol-2026-10-04.md` §3.2。
#
# 只读：本脚本不写任何文件、不改任何数据。
#
# 用法：scripts/v2-legacy-compat-probe.sh [data-root]
#   data-root 缺省为 $QAQH_DATA_DIR 或 ~/.qaqh
#
# 退出码：0 = 本机可判定的兼容项全部零命中；1 = 至少一项仍有存量数据（先别删）；
#         2 = 数据根不可读。
# 注意：零命中只证明**这台机器**不需要该兼容路径。beta 用户的数据在各自机器上，
#       删代码前应把本探针发给存量用户跑，或按发布策略确认版本跨度。
set -uo pipefail

ROOT="${1:-${QAQH_DATA_DIR:-$HOME/.qaqh}}"
[ -d "$ROOT" ] || { echo "数据根不存在：$ROOT"; exit 2; }

sessions=0
shopt -s nullglob
for d in "$ROOT"/sessions/*/; do [ -d "$d" ] && sessions=$((sessions + 1)); done
shopt -u nullglob

hits_total=0
# report <编号> <兼容点> <命中数> <判据说明>
report() {
  local id="$1" item="$2" n="$3" note="$4"
  if [ "$n" -gt 0 ]; then hits_total=$((hits_total + n)); fi
  printf '%-4s %-34s hits=%-4s %s\n' "$id" "$item" "$n" "$note"
}

count_grep() { grep -rlE "$1" $2 2>/dev/null | wc -l | tr -d ' '; }

# 1) meta.compact_skip 旧压缩语义（新压缩恒置 0）
n=$(for f in "$ROOT"/sessions/*/meta.json; do
      [ -f "$f" ] && node -e '
        const o=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));
        if(Number(o.compact_skip)>0) console.log(process.argv[1]);
      ' "$f" 2>/dev/null
    done | wc -l | tr -d ' ')
report 01 "meta.compact_skip>0" "$n" "旧压缩语义重放；有命中说明还存着前缀跳过水位"

# 2) 旧 [COMPACT 前缀标记（store.rs 清理路径）
n=$(count_grep '\[COMPACT' "$(find "$ROOT"/sessions -name 'messages.jsonl' -o -name 'messages.wal' 2>/dev/null | tr '\n' ' ')")
report 02 "旧 [COMPACT 上下文标记" "$n" "message/store.rs 的 retain 清理路径"

# 3) 中央 index.json → index.jsonl
n=$(find "$ROOT" -maxdepth 2 -name 'index.json' -type f 2>/dev/null | wc -l | tr -d ' ')
report 03 "sessions/index.json 存在" "$n" "迁移后应已自删；存在=尚未跑过迁移"

# 4) workspace.txt → meta.cwd
n=$(find "$ROOT"/sessions -maxdepth 2 -name 'workspace.txt' 2>/dev/null | wc -l | tr -d ' ')
report 04 "会话目录留 workspace.txt" "$n" "惰性迁移读侧；无此文件即可摘"

# 5) 旧反斜杠 cwd 修复（repair_legacy_backslash_cwd 只在非 Windows 生效，
#    所以这项的命中与否要按 JSON **解码后**的值判断，不能按文件里的转义形态 grep）
n=0
for f in "$ROOT"/sessions/*/meta.json; do
  [ -f "$f" ] || continue
  if node -e '
    const o=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));
    if(typeof o.cwd==="string" && o.cwd.includes("\\")) process.exit(0);
    process.exit(1);
  ' "$f" 2>/dev/null; then n=$((n + 1)); fi
done
report 05 "meta.cwd 含反斜杠未修复" "$n" "grouping.rs 的旧路径修复；注意它是跨平台存量修复，本机零命中≠别人机器零命中"

# 6) timeline-v3 目录改名
n=$(find "$ROOT" -maxdepth 3 -type d -name 'timeline-v3' 2>/dev/null | wc -l | tr -d ' ')
report 06 "timeline-v3/ 目录仍在" "$n" "一次性 pre-V1→V1 改名 fallback"

# 7) PersistedTimeline.journal 旧缓存字段（旧格式含尾部长度的条目）
tl=$(find "$ROOT" -maxdepth 3 -type d -name 'ringing-timeline' 2>/dev/null | head -1)
n=0
if [ -n "$tl" ]; then
  n=$(for f in "$tl"/*.json; do
        [ -f "$f" ] && node -e '
          const o=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));
          if(Array.isArray(o.journal)&&o.journal.length>0) console.log(1);
        ' "$f" 2>/dev/null
      done | wc -l | tr -d ' ')
fi
report 07 "timeline 缓存 journal 非空" "$n" "serde(default) 兼容字段；为空即可摘（注意会连带删 tail_replay 的 journal 读取）"

# 8) tool_outbox.wal 只读兜底
n=$(find "$ROOT" -name 'tool_outbox.wal' 2>/dev/null | wc -l | tr -d ' ')
report 08 "tool_outbox.wal 存在" "$n" "旧会话工具恢复 fallback"

# 9) DeepX 数据根 marker 改写
mk="$ROOT/.qaqh-data-root.json"
n=0
if [ -f "$mk" ]; then
  grep -qiE 'deepx|"product"[^,]*:[^,]*"deep' "$mk" && n=1
fi
report 09 "数据根 marker 仍是 DeepX 旧产品" "$n" "ensure_data_root() 启动即跑的改写；无命中=本机已是 QAQ-Harness"

# 10) 旧 provider_id →（provider_id, endpoint）
n=$(node -e '
  const fs=require("fs");const p=process.argv[1];
  if(!fs.existsSync(p)) { console.log(0); process.exit(0); }
  const txt=fs.readFileSync(p,"utf8");
  // 旧形态：[profiles.*] 段落里有 provider_id 而无 endpoint
  const blocks=txt.split(/^\s*\[/m).slice(1);
  let legacy=blocks.filter(b=>/provider_id\s*=/.test(b) && !/endpoint\s*=/.test(b)).length;
  console.log(legacy);
' "$ROOT/config.toml" 2>/dev/null || echo 0)
report 10 "profile 缺 endpoint（旧 provider_id 单值）" "$n" "registry::migrate_provider_id 兼容"

# 11) 明文 api_key → secret store
n=$(grep -cE '^\s*api_key[[:space:]]*=[[:space:]]*"' "$ROOT/config.toml" 2>/dev/null || true)
n=${n:-0}
report 11 "config.toml 内有明文 api_key" "$n" "命中即必须保留迁移与\"keeping plaintext\"降级路径"

# 12) 旧扁平 model/effort 字段 → [profiles.*]
n=$(awk '
  /^[[:space:]]*\[/ { in_section=1 }
  !in_section && /^(model|effort|reasoning_effort|endpoint|base_url)[[:space:]]*=/ { c++ }
  END { print c+0 }
' "$ROOT/config.toml" 2>/dev/null)
n=${n:-0}
report 12 "顶层扁平 model 字段" "$n" "首个 [section] 之前出现即旧形态"

# 13) 权限旧四档 permission_level（现行落盘键是 permission_tier，严格 1..=3）
n=$(grep -cE '^\s*permission_level[[:space:]]*=' "$ROOT/config.toml" 2>/dev/null || true)
n=${n:-0}
report 13 "config.toml 用旧键 permission_level" "$n" "from_legacy_u8 迁移；数字 3 新旧语义不同，别按现行档解读"

echo
echo "数据根：$ROOT（会话 $sessions 个）"
echo "本机可判定项命中合计：$hits_total"
echo
echo "本机不可判定（要 wire 侧/用户侧证据，不能靠扫盘删）："
echo '  A. CommandBody 的 "normal" alias（qaqh-domain/src/command.rs:39）——旧客户端出站请求才会用到'
echo "  B. 权限在 wire 上仍是裸 u8（qaqh-policy/src/lib.rs:68 注释）——取决于对端版本"
echo "  C. discovery 的 pre-0.9 兼容——要确认旧客户端是否还活着"
echo "  D. /control/v1/* 改名——配合 discovery 版本面，属对外契约变更"

if [ "$hits_total" -gt 0 ]; then exit 1; fi
exit 0
