# ADR-0001 kernel/user-space 分支模型与 LTS API 冻结

状态: `已采纳` · 起点 `v3.0.0`（tag @ `f9b3a7f8`）· 决策人：仓库所有者
操作手册见 `docs/lts-v3.0-maintenance.md`，本文只记录**为什么这样切**与代价。

## 背景

前端只依赖后端的一个稳定面；后端内部（agent loop、session 存储、sandbox）需要快车演进。
两种速率放进同一条分支会互相拖累：要么前端被后端的重构打断，要么后端被前端的节奏冻住。

借用 kernel / user-space 的分界：**API 面是系统调用边界，边界以内随便改，边界不许动。**

| 分支 | 角色 | 内部实现 | 用户可见面（RingingV2 + SDK 契约） |
|---|---|---|---|
| `main` | 开发快车 | 允许破坏、重构、删除 | 允许演进（bump revision） |
| `LTS-v3.0` | 用户空间依赖的稳定面 | 可修 bug、可性能优化、可加私有代码路径 | **冻结**：行为与 `v3.0.0` 完全一致 |

**两条线已各自前进并合流两次**（2026-11-10）：`LTS-v3.0` 收到 `4a075e53` / `c9a2db3b` /
`66a6240b` / `bdd83d87`，`main` 把自己的契约提交与这些合并在一起。**不变量：`main` 必须是
`LTS-v3.0` 的超集** —— 否则 main→LTS 的 cherry-pick 会持续撞车（`.gitattributes` 那条规则
就是这么撞的，见 ADR-0007 §问题 4）。

闸门现状（诚实版）：ADR-0003 与 ADR-0007 的机器部分**已落在 main**（路由双表 + 禁止型 lint +
双向覆盖闸 + 行为测试 + 改动分类 job + CODEOWNERS + 两个 label）。但**这些还只活在 main**，
且 CODEOWNERS / required check 在 admin 开启分支保护之前是"会跑但不拦"（手册 §7）。
叠加"本仓有并发写入者"这一事实（两个 worker 20 分钟内各自修同一个洞、互不知情），
当前真正的风险不是缺机制，而是**机制尚未覆盖冻结线本身**。

## 决策

1. 冻结面就是 `docs/lts-v3.0-maintenance.md` §1 列的五条：HTTP 端点集合、
   信封与版本闸门（`ringing-commands-7` / `ringing-session-2` 在整个 LTS 周期保持原值）、
   `TYPED_METHODS` 方法表、命令与事件变体、`contract_hash`。
2. **判定权在机器，不在人**：LTS 上一律以 `lts-api-guard.yml` + `scripts/check-lts-api-surface.py`
   的红绿为准；文档 §1 的口头判定法（跑 `sdk-check`）只作预检。
3. **失败方向固定**：守卫红 ⇒ **回退实现，绝不回退快照**。因此"改夹具让它变绿"在 LTS 上是
   事故而非修复；夹具只允许修编译性破损，不得改描述被测行为的断言。
4. **换锁不是冻结**：确需修订契约时，那已是新 LTS 线（v3.1）的事，须 bump revision 并重切 tag
   （治理见 ADR-0007）。
5. LTS 周期内禁止升级外部 path 依赖 pin（`qaqtam/mutil-ai @ 4387f74d…`）。它的行为是用户空间
   可见面的一部分，pin 动了，"后端可以随便改"这句就不成立了。
6. 热修可跳过干跑，但**不可**跳过守卫，且 tag 必须走 `v3.0.x` 序号，禁止 `latest`/浮动 tag。

## 被否选项

- **单分支 + 版本 feature gate**：在一个二进制里用 `if version >= x` 分叉行为。否 —— RingingV2
  是带租约与 seq 的长连接面，条件分叉会把兼容性测试面从 O(线数) 放大到 O(分支数 × 组合数)，
  且没人能穷举。
- **只打 tag 不开分支**：否 —— 无分支就没有"只在 LTS 触发"的守卫挂载点（`lts-api-guard.yml`
  的 `on.push.branches: [LTS-v3.0]`），快车会随时把冻结面撞碎。

## 后果

- `main` 上没有冻结守卫，这是**设计**而非漏洞：快车的破坏由 ADR-0005 的配对测试在 LTS 边界上兜。
- 每次 LTS 补丁发版要同步 5 处版本戳（`version.txt` / `Cargo.toml` / `Cargo.lock` /
  `package.json` / `qaqh-backend.lock.json`），漏一处就会撞 `--locked` exit 101
  （手册 §5 已记录两次实战）。
- 双分支共用 CI 触发面差异，PR 目标分支选错 ⇒ 守卫根本不跑。ADR-0007 处理这一类"闸门没被触发"。
