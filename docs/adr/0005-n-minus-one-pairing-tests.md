# ADR-0005 兼容性由 N-1 配对测试证明，不由字节相等证明

状态: `已采纳（未实施）` · 决策日期 2026-11-10

## 背景

现有的全部 LTS 闸门都是**静态一致性**：

- `check-lts-api-surface.py:76-98` 逐字节比对导出物；`:112-116` 比对路由/门面快照；
- `check-sdk-contract.py:27-43` 校验 manifest 文件集、逐文件 sha256、`contract_hash`；
- `check-sdk-contract.py:80-123` 用严格 tsc（`:125` `strict: true` + `skipLibCheck: false`）
  编译一个内嵌消费者，并以 `@ts-expect-error` 断言**非法形状必须被拒**。

这些证明的是"契约工件没变"。**它们无法证明"用户空间能用"。**

## 论证：字节相等 ≠ 行为不变

下列改动**不产生任何 TS 类型变化**，因此六道闸门全绿，但足以破坏一个 pin 在 v3.0.0 的前端：

1. `#[serde(default)]` / `skip_serializing_if = "Option::is_none"` 的开合 ——
   字段从"总是出现"变成"可能缺键"，TS 侧 `T | null` 与"键不存在"不是一回事；
2. 字段发射顺序变化（哈希覆盖的是文件字节，若某处做了序列化排序，人眼看不出的漂移照样过）；
3. 数值边界：`main.rs:199` 的 `.with_large_int("number")` 决定 u64 → JS number，
   超过 `MAX_SAFE_INTEGER`（已在 `constants.ts` 导出）即静默丢精度，类型完全一致；
4. 空集合 vs `null`、空字符串 vs 缺键（本仓 fixture 已刻意区分二者，
   如 `service-session-turn-result.json` 用 `Option::<…>::None`）；
5. 运行期解析失败被吞：ADR-0004 记录的 `v2.rs:569-571` `.ok()` —— 该类型根本不在导出物里，
   字节比对对它**无定义**。

第 5 条尤其说明问题：闸门强度受限于"契约面有多宽"，而契约面本身可能小于用户空间可见面。

## 决策

1. 建立**跨线配对测试（pairing matrix）**，两个方向各一遍：
   - **N-1 旧前端 × 新后端**：用**已发布的** `qaqh-sdk-contract-<上一 tag>.zip`（由
     `.github/workflows/release.yml:56-65` 产出、`:100` 声明供 qaqh-sdk / qaqh-electron 消费）
     编译一个消费者，连**本次构建**的 daemon 二进制；
   - **N+1 新前端 × 旧后端**：本次导出的消费者连已发布的 daemon 资产。
2. 每个方向跑 fixture 驱动的 **happy path + 至少一条 reject path**。fixture 已经在契约 zip 里
   （`fixtures/` 106 个），无需新造数据。
3. **触发点**：`sdk-contract.yml`（已覆盖 PR / `main` / `LTS-v3.0`），以及 LTS 打 tag 前的
   强制干跑。**运行在 CI，不占本机内存**（本机 8 GB / 余 4.2 GB，双 daemon 构建不适合本地跑）。
4. 配对的**判定基准是行为**：状态码、关键字段存在性、枚举是否被识别、幂等重放是否得到同一 ack。
   不接受"用 HEAD 生成的契约自测" —— 那只是把静态一致性又跑了一遍。
5. `docs/lts-v3.0-maintenance.md` §6 checklist 第 2 条（"`contract_hash` 与上一版一致"）
   降级为**必要条件**，新增一条"配对测试绿"作为**充分条件**。

## 被否选项

- **加大静态强度**（如比对 serde derive 的 token 流）：否。仍是静态，仍然测不到 `default` /
  空值语义 / 精度这类只在运行期显形的漂移。
- **Pact 式 mock 交互**：已在 ADR-0002 否决（消费者自家、契约集中定义，broker 只增第二份真相）。
  本 ADR 的做法是**真进程真连接**，不需要 broker。
- **只靠 e2e（`scripts/e2e-m0.mjs` / `smoke-*.ps1`）**：现有 e2e 面向当前 HEAD，不构成跨线矩阵；
  可作为承载体复用，但必须显式喂"旧契约 zip"。

## 后果与代价

- CI 时长上升：两个平台各要构建 daemon。这是**必须花的钱**，因为它换来的是
  "用户空间未被破坏"的直接证据，而不是间接推论。
- 需要能拿到"上一版已发布契约 zip"：由 GitHub Release 资产取（ADR-0006 落地后可改由 npm +
  `contract_hash` 取，更干净）。在 ADR-0006 之前，测试 job 要先解析 tag → 资产 URL。
- 本机不跑（rule 10：磁盘/内存受不了）。本地预检只该用 ADR-0003 提到的 `just surface-quick`
  这类文本级秒级检查。

## 落地清单

- [ ] pairing harness：读 `qaqh-sdk-contract-<tag>.zip` → 生成/编译消费者 → 起 daemon → 跑 fixture
- [ ] `sdk-contract.yml` 增 job（或新增 `pairing.yml`），LTS 与 main 都触发
- [ ] reject path 至少覆盖：错 `contract_revision`、缺 lease、未知 method（对应
  `check-sdk-contract.py:106-107` 的 `not.a.method` 语义）
- [ ] 手册 §6 checklist 改写：哈希一致 = 必要，配对绿 = 充分
