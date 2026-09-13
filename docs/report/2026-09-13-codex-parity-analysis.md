# Codex 上游对照分析 — QAQ-Harness 隐藏 Bug 修法参照

> 对照对象：`D:\project\codex-main`（OpenAI Codex，codex-rs workspace）。
> 目的：对我们 buglist（`docs/buglist/2026-09-13-hidden-bug-scan.md`）中可对照的条目，
> 考察上游同问题的实现方式，校准/升级修法。
> 分析日期：2026-09-13。Codex 源码位置均以该快照为准。

## 结论摘要

- 7 个条目有实质对照：**BUG-01 的修法可升级**（从"每个调用点 normalize"升级为"路径收敛进归一化新类型"）；BUG-11 修法与上游逐字一致；BUG-02 有等效的第二种防法（f64 幂）；BUG-22 上游同样未封顶（挂 TODO），修了即领先。
- 我们 P0 级 5 个 bug **全部落在 Codex 用类型系统或结构性契约根除的类别**（路径类型、durable ref、幂构造），无一是上游也靠运行时检查将就的类别——说明此类问题应做结构性修复而非逐点打补丁。

## 逐条对照

### BUG-01 apply_patch 逃逸 ↔ `PathUri` + `AbsolutePathBuf`（价值最高）

Codex 分三层防御，我们的修法只覆盖了第 1 层：

**① join 时词法消解 `..`，且 clamp 在锚点内** — `codex-rs/utils/path-uri/src/lib.rs:516-530`

```rust
for component in path.split('/') {
    match component {
        "" | "." => {}
        ".." => {
            if depth > anchor_depth {   // 永远弹不出根/盘符
                segments.pop();
                depth -= 1;
            }
        }
        component => { segments.push(component); depth += 1; }
    }
}
```

`a/b/../../evil.txt` 在 join 阶段就被消解为 `<cwd>/evil.txt`——不存在"带着未消解
`..` 去 canonicalize、失败再降级放行"的分支。我们 buglist 写的
"入口先 normalize_lexically" 即此层。

**② fail-closed 后代校验（双保险）** — `lib.rs:538-554`：`join_descendant` 在 join 后
再验一次 `descendant.starts_with(self)`，不满足直接报 `JoinPathMustBeDescendant`。

**③ 类型系统根治** — `codex-rs/utils/absolute-path/src/lib.rs:16-24`：

```rust
/// A path that is guaranteed to be absolute and normalized
pub struct AbsolutePathBuf(PathBuf);
```

构造即 normalize + absolutize（`resolve_path_against_base` 对 base 与 path 两侧都先
normalize，L45-56）；serde 反序列化无 base 时拒绝相对路径（L343-345）。**未归一化
路径在该代码库无法存活**，不依赖每个调用点自律。其测试直接锁定了我们的攻击向量：
`./nested/../file.txt` normalize 用例（L522-528）。

> **修法升级**：最小修法维持 buglist 原案；正解是把 workspace 工具链的路径统一收敛到
> 一个 `NormalizedPathBuf` 新类型（构造即消解 `..`、拒绝 null 字节），一次投入消灭
> 全部工具的同类隐患（同时覆盖 BUG-16 的键形态不一致）。

### BUG-02 backoff 溢出 ↔ f64 幂 + 饱和转换

`codex-rs/core/src/util.rs:86-91`：

```rust
const BACKOFF_FACTOR: f64 = 2.0;
pub fn backoff(attempt: u64) -> Duration {
    let exp = BACKOFF_FACTOR.powi(attempt.saturating_sub(1) as i32);
    let base = (INITIAL_DELAY_MS as f64 * exp) as u64;   // float→int 饱和
    ...
}
```

用 **f64 `powi`** 而非整数 pow：溢出到 infinity 后 `as u64` 饱和为 `u64::MAX`，
永不 panic 也不回绕，上游再 clamp。与 buglist 的
`checked_pow(..).unwrap_or(u64::MAX)` 等效，且连 `unwrap_or` 都省了。另注意
`attempt.saturating_sub(1) as i32`——连 attempt 入口都先 saturate。

两种防法任选；整数路径（checked_pow）更贴近现有代码，改动最小。

### BUG-11 secrets 固定 tmp 名 ↔ `secrets/src/local.rs:295-296`（逐字一致）

```rust
let tmp_path = dir.join(format!(".{}.tmp-{}-{nonce}", ...));
```

tmp 名带 nonce + 写完 `sync_all` 再 rename（L318-323）+ rename 失败重试分支
（L323-335）。与 buglist 修法（pid+nonce）一致，**按原案执行即可**。

### BUG-04 图片不落盘 ↔ `AttachmentStore` 持久化契约

`codex-rs/attachment-store/src/lib.rs:19-30`：

```rust
/// Persists `data` and returns a durable reference to it.
/// A successful reference must remain valid when callers persist it...
pub trait AttachmentStore: Send + Sync {
    fn persist<'a>(&'a self, data: &'a [u8], ...) -> AttachmentStoreFuture<'a>;
}
```

契约是"**先持久化字节，durable ref 才存在**"——`AttachmentRef` 只在字节落盘后产生。
我们的缺陷正是把"字节落盘产生 ref"与"消息持久化"拆成两步，且消息持久化在 ref 产生
之前已克隆定格。buglist 修法"把图片外置挪到 ingest 之前"与此同向；结构性正解是
让 ImageRef 的构造函数本身要求"字节已在场"（类型级后置契约）。

### BUG-24 seed 碰撞 ↔ UUID 身份 + OS 写者锁

- 身份：`codex-rs/rollout/src/rollout_file_name.rs:62-74` —— `rollout-{timestamp}-{thread_id}.jsonl`，
  ThreadId 为 UUID，时间戳只是文件名可读性装饰，**身份不玩 hash 截断**，碰撞概率工程性消除。
- 单写者：`rollout/src/writer_lock.rs:17-42` —— OS 级 coordination lock 文件 + `WouldBlock`
  判定，"活跃写者"靠锁而非猜测。

我们的 exists 重试是止血；正解是 seed 扩到 64+ 位随机（或直接 128 位 UUID 形态）。

### BUG-22 retry-after 无上限 ↔ 上游同样未封顶（挂 TODO）

- `core/tests/suite/retry_after.rs:245`：`// TODO(anp) respect Retry-After` —— 解析了
  该头但明确标注未完成。
- 本地生成的延迟全部封顶：`core/src/responses_retry.rs:17-18, 86-88`
  （`saturating_mul(2).min(MAX_CONNECTION_RETRY_DELAY)`，硬顶 60s）。

即：**本地退避必封顶、服务端延迟是已知未完成项**。按 buglist 原案修复即领先上游。

### 附：debug/release 分裂行为策略

`core/src/util.rs:93-99`：

```rust
pub(crate) fn error_or_panic(message: impl ToString) {
    if cfg!(debug_assertions) { panic!(...) } else { error!(...) }
}
```

"debug 断言炸给开发者、release 降级日志"被封装为公共函数。对应 BUG-02 的
"debug panic / release 回绕"分裂——bug 要在 debug 暴露，但不能带着用户死。
我们大量使用 `debug_assert`（如 turn_lap/gate.rs:124-126 的 P2 就是 debug_assert
能炸的例子），此策略值得引进为公共 helper。

## 设计哲学对照（bug 类别分布）

| Codex 模式 | 对应我们的 bug |
|---|---|
| 不变量进类型（AbsolutePathBuf / PathUri 构造即归一化） | BUG-01、16、14（路径类全家桶） |
| 饱和算术优先于整数幂（f64 powi + saturating cast） | BUG-02、21（溢出 panic 面） |
| durable ref 是持久化的后置产物（AttachmentStore 契约） | BUG-04、05（持久化时序） |
| 唯一身份用 UUID 不用 hash 截断 + OS 锁判活 | BUG-24、26（身份碰撞类） |
| 降级必须显式且 fail-closed 方向 | BUG-15、13（fail-open 类） |
| 已知未完成项显式挂 TODO 注释 | BUG-22 |

**关键观察**：我们 P0 全部 5 个 bug 都落在 Codex"用类型系统或结构性契约根除"的类别，
没有一个落在 Codex 也用运行时检查将就的类别。此类 bug 不是"漏写一个 if"，
而是"把安全性寄托在每个调用点的自律"——修复应优先做结构性收敛，而非逐点补 if。

## 对修复计划的影响

1. BUG-01：修法升级为"workspace 路径收敛进 `NormalizedPathBuf` 新类型"（一次性投入，
   连带消除 BUG-16 同类隐患）；短期可先落 buglist 最小修法止血。
2. BUG-02：维持 checked_pow 方案（改动最小），可顺带把 `error_or_panic` 模式引入。
3. BUG-11：无变化，按原案。
4. BUG-04：短期按原案（push 后重新入队持久化）；中期把 ImageRef 构造改为
   "字节已在场"的后置契约。
5. BUG-24：短期 exists 重试；中期 seed 换 UUID 形态。
6. BUG-22：按原案；上游也未封顶，无额外参照。
