import io, json, sys
from collections import Counter, defaultdict

path = r"D:\project\QAQ-Harness\.stress\qaqh-home\.qaqh\ringing\ringing-timeline\75dc1917.json"
with open(path, "rb") as f:
    data = json.load(f)

snap = data["snapshot"]
journal = data.get("journal", [])
total = len(json.dumps(data, ensure_ascii=False).encode())
snap_bytes = len(json.dumps(snap, ensure_ascii=False).encode())
journal_bytes = len(json.dumps(journal, ensure_ascii=False).encode())
out = []
out.append(f"total={total} ({total/1048576:.2f}MB)  snapshot={snap_bytes/1048576:.2f}MB ({snap_bytes/total:.1%})  journal={journal_bytes/1048576:.2f}MB ({journal_bytes/total:.1%})")
out.append(f"watermark={snap['watermark']} turns={len(snap['turns'])} journal_entries={len(journal)}")
open_turns = [t for t in snap["turns"] if not t.get("sealed")]
out.append(f"sealed={len(snap['turns'])-len(open_turns)} open={len(open_turns)}")

kinds = Counter(); payload = defaultdict(int)
for e in journal:
    k = e["event"]["type"]; kinds[k] += 1
    if k == "text_delta": payload[k] += len(e["event"]["delta"].encode())
    elif k == "block_checkpoint": payload[k] += len(e["event"]["text"].encode())
    elif k == "tool_progress": payload[k] += len(e["event"]["chunk"].encode())
out.append(f"journal kinds: {dict(kinds)}")
out.append(f"payload bytes: {dict(payload)}")

# snapshot 文本总量
st = sum(len((b.get("text") or "").encode()) for t in snap["turns"] for r in t.get("rounds",[]) for b in r.get("blocks",[]))
sp = sum(len(((b.get("tool") or {}).get("progress") or "").encode()) for t in snap["turns"] for r in t.get("rounds",[]) for b in r.get("blocks",[]))
out.append(f"snapshot text={st/1048576:.2f}MB progress={sp/1048576:.2f}MB")

# checkpoint 折叠验证：journal 中同一 block 是否只有最新一条
acc = defaultdict(list)
for e in journal:
    if e["event"]["type"] == "block_checkpoint":
        acc[e["event"]["block_id"]].append(e["event"]["text"])
dups = {k: len(v) for k, v in acc.items() if len(v) > 1}
out.append(f"blocks with >1 checkpoint in journal: {dups if dups else 'NONE (folded OK)'}")
max_block = max((len(v[-1]) for v in acc.values()), default=0)
out.append(f"max live block checkpoint text = {max_block} chars")

with open(r"D:\project\QAQ-Harness\.stress\final_analysis.txt", "w", encoding="utf-8") as f:
    f.write("\n".join(out))
print("done")
