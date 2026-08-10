# WinUI 更新设计——测试阶段闭环（基线增量 + 弹窗 UX）

状态：设计定稿（2026-08-10）
范围：测试阶段（`package/update/` 静态源）跑通「前后端分开打包 + 基线增量 +
WinUI 弹窗升级」闭环；联网源 / 签名 / 数据迁移为后续阶段，本设计预留扩展点。

## 1. 发布模型（基线累积）

```text
检查点（完整包）       1.0.0 ──────────── 1.1.0 ──── 2.0.0 …
增量（基于最近检查点）     └─ 1.0.1  1.0.2   └─ 1.1.1
                             累积（每个增量包含基线后全部变化）
```

**契约（6 条）**：
1. 检查点版本 = 完整包（planner 选 `full` 或组件全量）。
2. 中间版本 = 增量包，**基于最近检查点累积生成**（非逐版本链）。
3. 客户端版本 ≥ 基线 → 拉增量包，sha256 自适应（不匹配才下载）。
4. 客户端版本 < 基线 → 回退 `full`。
5. 安装目录布局变更 → 新检查点完整包（或卸载→重装，用户数据哨兵保障数据不丢）。
6. 数据格式迁移（如 jsonl）→ **不归更新器**，由 daemon 启动时版本化迁移（后续阶段）。

## 2. 更新源布局（安装根目录 updates\）

```text
<安装根>\updates\
├── catalog.json                 # v1.1
├── baselines/
│   └── 1.0.0/
│       └── manifest.json        # 检查点完整文件清单（逐文件 sha256）
└── bundles/
    ├── runtime-1.0.1.zip        # 增量：仅相对基线变化的文件
    ├── backend-1.0.1.zip
    └── full-1.0.0.zip           # 检查点完整包
```

测试期 = 现有 `DirectoryUpdateSource`（M1 已实现）；目录即 URL 约定的雏形，
M4 `HttpUpdateSource` 复用同一布局。更新源固定为安装根目录 `updates\`
（决策 2026-08-10）。

## 3. Catalog v1.1 扩展（向后兼容）

```json
{
  "id": "runtime-1.0.1",
  "kind": "runtime",
  "strategy": "file-level-delta",
  "baseline": "1.0.0",
  "targets": { "runtime": "1.0.1" },
  "requires": { "controlProtocol": 1 },
  "payload": {
    "path": "bundles/runtime-1.0.1.zip",
    "size": 5242880,
    "sha256": "…"
  }
}
```

- `strategy` 现值为 `component-full`；新增 `file-level-delta`（架构文档 L93 预留
  的 binary-delta 语义以文件为粒度落地）。
- `baseline` 记录增量参照的检查点；客户端**不依赖它做升级决策**（仅展示），
  决策只看 sha256。
- 完整包 artifact 不带 `baseline`。

## 4. 增量机制（服务端生成 / 客户端自适应）

**增量包 bundle.json 与完整包同构**，唯一差别：`files[]` 只列相对基线变化的
文件。updater stage 对每个目标文件：

```text
目标文件 sha256 在增量包 files[] 内？
  ├─ 是 → 与当前安装文件 sha256 相同 → 本地复制（0 下载）
  │        （覆盖"跳过 1.0.1 直升 1.0.2"的中间态复用）
  ├─ 是 → 不同 → 解压应用
  └─ 否 → 未变化，保持现状（隐含继承基线）
```

**服务端用基线决定"哪些文件进包"，客户端用 sha256 决定"哪些文件本地复用"——
完全解耦**，客户端不感知基线概念。

## 5. 打包：`just upgrade-package`

输入：`baselines/<checkpoint>/manifest.json` + 目标构建目录（collect-payload 产物）
输出：增量 zip（变化文件）+ catalog.json 更新
核心逻辑 = `deepx-update` 库纯函数（本地 `just upgrade-package` 与未来
GitHub Action 共用同一实现，纯函数可单测）：

```text
plan_delta(baseline_manifest, target_dir) -> DeltaPlan {
    files = []
    for (target, sha256) in target_manifest:
        if baseline[target] != sha256:      # 变化
            files.push({source, target, size, sha256})
    return DeltaPlan { files }
}
```

基线清单来源：CI 从 `package/update/baselines/` 读（随仓库/制品库保留）。

## 6. updater 改动（crates/deepx-update）

- ✅ **增量应用零改动**（step 2 已验证）：`apply_bundle_zip` 是纯覆盖式——
  只动 `files[]` 列出的文件，未列文件保持现状，增量 zip 直接可应用；回滚
  （`rollback_bundle_zip`）按同一 `files[]` 反向恢复，天然适配。单测覆盖
  增量应用（变化替换 + `.previous` 备份 + 未变不动 + state 推进）与回滚。
- `stage` 本地复制分支（网络下载场景 sha256 本地复用）**延后到 M4**
  HttpUpdateSource 一并实现（目录源无下载概念，无此需求）。
- planner 识别 `strategy=file-level-delta`（解析即可，决策仍走 sha256）。

## 7. WinUI 弹窗 UX

**拉起模型：前端（客户端）拉起 updater**（多客户端共识：winui / ratatui 等
共享 `spawn_updater` 库函数，各自只做 UI 呈现；daemon 不承担更新管理职责）。

```text
检测：启动时 + 设置页手动（更新源 = 安装根目录 updates\）
前端 spawn deepx-updater.exe check --source <updates\>
    ├─ 无更新 → updater 静默退出（exit 0，不写 pending）
    └─ 有更新 → 下载 + stage → 写 pending.json → 退出
前端轮询 pending.json（M3 已有）→ Toast → 点击展开 ContentDialog：
   版本号 + release notes + 自动选最小组件集 + [立即更新|稍后|忽略此版本]
用户点「立即更新」→ 前端 spawn deepx-updater.exe update --op <id>

按组件分流：
   backend  → updater 经壳协调 daemon（stop-if-idle → 替换 → 拉起新 daemon，
              M2 已有，架构文档 §9 时序图）→ Toast「后端已更新 vX」（前端无感）
   runtime  → 弹「重启完成更新（立即/稍后）」→ 释放 daemon lease →
              runner handoff（M3 已有）→ 壳退出 → 替换文件 → 拉起新壳 →
              健康回执 commit
   full     → 同 runtime，另含 updater 自身替换（runner 副本机制，已有）
忽略此版本 → 本地记录（版本号），不再提示
离线/失败 → 静默，设置页显示「检查更新失败 · 上次检查时间」
```

测试期（本地目录源）不引入 HTTP；M4 换 `HttpUpdateSource` 时 UI 零改动
（`UpdateSource` 抽象已隔离，架构文档 §4）。

## 8. 安全（沿用现状 + 增量补充）

- Bundle 与逐文件双层 SHA-256（已有）。
- 增量包同构校验；本地复制分支仍校验当前文件 sha256 后才复制。
- 联网签名（ed25519 catalog）为 M4 项，本设计不动 schema，仅预留字段位。

## 9. 实施顺序与验收

| 步 | 内容 | 位置 | 状态 |
|---|---|---|---|
| 1 | catalog v1.1（strategy/baseline）+ `plan_delta` 纯函数 + 单测 | deepx-update | ✅ a3a8e8a |
| 2 | 增量包应用/回滚验证（覆盖式语义天然支持，零改动）+ 单测 | deepx-update/apply.rs | ✅ ae3891b |
| 3 | `deepx-pack` CLI + `just upgrade-package` / `baseline-release` 发布流程 | deepx-update/bin + apps/installer/scripts + justfile | ✅ 22d8b52 |
| 4 | 更新源地址配置 + 检测（启动时/手动） | winui 壳 | ⬜ 待排期 |
| 5 | Toast + ContentDialog + 进度（后端热升级 / 前端 restart 两路径） | winui 壳 | ⬜ 待排期 |
| 6 | 端到端验收：打 1.0.0 基线 → 改代码 → 打 1.0.1 增量 → 1.0.0 客户端升级验证（含跳过版本） | 手工 | ⬜ 待排期 |

已落地（step 3）的发布命令：

```text
just baseline-release version=1.0.0   # 检查点：完整包重建 + baselines/1.0.0/manifest.json
just upgrade-package kind=runtime     # 增量：基于最近基线 → package/update/bundles/ + catalog
just upgrade-package kind=backend     # 同上，restartPolicy=daemon
```

说明：step 2 的「stage 本地复制分支」（网络下载场景 sha256 本地复用）因
目录源无下载概念，延后到 M4 HttpUpdateSource 一并实现。step 4-6 待 rc.4
冻结后排期（或随 M4 网络源一并做）。

## 10. 决策记录（已定）

1. **`plan_delta` 实现位置**：deepx-update 库（Rust，可测、CI 复用）。
2. **更新源地址**：固定为安装根目录 `updates\`（非 %LOCALAPPDATA%）。
3. **实施顺序**：先 1（catalog v1.1 + plan_delta）→ 跑通后上 2
   （updater stage 本地复制）→ 3（just upgrade-package）→ 4/5（壳 UI）。
4. **拉起模型**：前端（客户端）拉起 updater；多客户端共享 spawn_updater 库函数。
