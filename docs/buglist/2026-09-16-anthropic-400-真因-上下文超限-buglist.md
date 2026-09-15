# BUG-2026-09-16-04：anthropic 端点「双 read_image 400」真因 = 上下文超限（context_limit 配错）

## 症状

在 opencode zen 的 anthropic 兼容端点（`https://opencode.ai/zen/go/v1/messages`）上，
会话跑长后**每个 gate 请求都 HTTP 400**，错误体只有一行 `{"model":"deepseek-v4.1-flash"}`，
没有任何可判读信息。表面相关：连续两次 `read_image`（t33 r2、t34 r0、t38 r1、t39 r0）。

## 结论（先给答案）

**400 = 请求输入超出该端点的真实上下文上限。**
我们的 `context_limit` 写的是 **1,000,000**（`qaqh-config/src/dto.rs:226` 的默认值），
而该端点在我们真实请求形态下 **240,841 tokens 通过、再上一档即 400**。
压缩阈值 = `auto_compact_threshold(0.8) × context_limit` = 800k ⇒ 永不触发，
于是会话一旦涨过 ~245k，后续每个请求都被上游拒掉。

`read_image` 只是"压垮的最后一根稻草"：它恰好是那几轮的新增内容，与图片形态/大小/数量无关。

## 证据链

1. **最小形状探针**（`/tmp/probe_anthropic_images.py` / `probe_replay.py`）：
   `[user, asst(thinking + 2×tool_use), user([tr,tr,img,img])]` 用**图库真图 + 单编码**
   → 200。即：修好的 R1 形态（tool_result 连续且在前）是对的，400 另有其因。
2. **离线重放台**：临时 `#[ignore]` 测试把磁盘 `messages.jsonl` 经 **gate 自己的
   `convert_messages_to_anthropic`** dump 成请求体（`--ignored --nocapture`），再用
   Python 打向真实端点 → **400，与线上一致**，从此可离线二分。
3. **逐元素删除**：去掉全部图片 → 仍 400（2.66MB 纯文本）；去掉 thinking → 仍 400。
   ⇒ 图片、thinking 都不是触发条件。
4. **规模二分**（去图后按尾部 N 条消息切片）：
   `tail=550`（542 条 / 1.79MB）→ **200，usage.input_tokens=240,841**；
   `tail=560`（552 条 / 1.84MB）→ **400**。翻转点夹在 240.8k tokens 与下一档之间。
5. **排除项**（各自单独验证，均 200）：
   - 单条文本 270,221 tokens（0.42MB）→ 200 ⇒ 不是纯 token 上限；
   - 1,000 条微消息 → 200 ⇒ 不是消息条数上限；
   - 100 张小图（body 2.26MB）→ 200 ⇒ 不是 body 体积上限；
   - `max_tokens` 16→128000 六档 → 全部 200 ⇒ 不是 `input + max_tokens` 判定；
   - `stateful = false`（override 里已声明）⇒ 与增量切片无关。
6. **路径差异**：`/v1/chat/completions` 单条随机文本 **600,936 tokens → 200**。
   两个后端上限不同（openai ≈1M，anthropic ≈245k），所以"同一会话换端点后炸"不是
   端点的偶发，而是**anthropic 后端更小**。
7. **"压缩后自愈"吻合**：daemon 重启后执行 `[COMPACT] 26 turns`（日志
   `1789492360`），随后 t40/t41 各轮全绿 —— 与"砍掉历史即回到上限内"一致。

## 已修正的次生问题

- **工具图被投影两遍**：存储层同时保留内联 `result.images` 与兄弟 `ImageRef`
  （`crates/qaqh-message/src/store.rs:874` 外置落盘后未剥离原字节），anthropic
  转换器把两者都发出去 ⇒ 16 张工具图 → **32 个 image 块**（实测）。
  已在 `message_api.rs` 投影处按消息内引用数去重，回归锁
  `tool_image_inline_and_ref_are_projected_once`。
- **用户配置**：`~/.config/qaqh/config.toml` 的 `[profiles.anthropic].context_limit`
  1,000,000 → **240,000**（备份 `config.toml.bak-20260916-context-limit`）。
  ⚠️ 需**重启 daemon** 生效；在此之前任何一次设置保存都会用内存里的 1M 覆盖回文件。

## 待办（真正的结构性修复）

1. **端点需能声明上下文窗口**：`qaqh-config` 的 endpoint/registry schema 没有
   `context_limit` 之类的字段，profile 只能靠用户手填（默认 1M）⇒ 换端点即踩坑。
   建议 endpoint 增加 `context_window`（或 model 级 `context`），`profile.apply` 时
   以端点声明为准覆盖 profile。
2. **超限请求应给可判读错误**：上游错误体 `{"model":...}` 无信息量。gate 在发请求前
   用 `context_limit` 做一次 pre-flight（估算 token > limit ⇒ 本地返回
   `CONTEXT_OVERFLOW`，附"需压缩"提示），别再发注定 400 的请求。
3. **压缩要有硬兜底**：连续 400 且错误无法判读时，强制压缩后再试一次
   （当前只有 `auto_compact_threshold × context_limit` 一条路径，配错即死锁）。
4. **另两条协议路径同款图片重复**：`chat_completions_api.rs:775/866`、
   `responses_api.rs` 有同形的 inline + ImageRef 双份，需一并去重（含 `image_index`
   计数/`dropped_images` 语义核对）。
5. **`docs/buglist/2026-09-16-read_image连发触发400-buglist.md` 的结论需按本文修正**：
   那里记录的根因（tool_result 交织形态）已被证伪。

## 复盘

- 早前把两次探针结果（体积 400）当成"上游体积上限"，实为**我自己双重编码**了图库里的
  base64 文本（`b64encode(文件内容)`）——库里存的就是 base64 文本，必须原样使用或
  `b64encode(b64decode(text))` 归一化。**探针数据自证不足时，先验证数据本身合法。**
- `safe_provider_error_body` 只截 200 字符但**上游本来就没给原因**，所以"看日志找原因"
  这条路走不通；必须能离线重放真实请求体（本次的 dump 台）。
