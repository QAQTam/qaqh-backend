# DeepX 更新架构

状态：M1 可运行，M2/M3 MVP 已接通
范围：离线 Installer 驱动更新，以及未来 HTTP/CDN 更新的共同协议。

## 1. 目标

DeepX 使用同一套更新计划和应用引擎处理首次安装、离线更新和联网更新：

- `DeepXInstaller.exe` 提供离线更新源、首次安装 UI 和 Windows 系统集成。
- `deepx-updater.exe` 是唯一的修改、更新、校验、回滚和卸载维护执行器。
- Electron 主进程协调 renderer/shell 生命周期，并负责 daemon maintenance。
- daemon 通过现有 `stop-if-idle` 接口安全退出。

Installer 不是常驻服务。它与未来 HTTP/CDN 都实现 `UpdateSource`，向 updater
提供相同的 `catalog.json` 和 Bundle 字节。

## 2. 组件模型

| 组件 | 内容 | 最小切换动作 |
| --- | --- | --- |
| `renderer` | HTML、CSS、renderer JS、字体和静态资源 | reload BrowserWindow |
| `shell` | Electron Main、Preload、运行时 Node 依赖 | 重启 Electron，保留 daemon |
| `frontend` | 当前阶段的 shell + renderer (`app.asar`) | 重启 Electron，保留 daemon |
| `backend` | `deepx-daemon.exe`、daemon manifest | 只重启 daemon |
| `runtime` | Electron/Chromium Runtime、原生模块 | 重启 Electron |
| `full` | runtime + frontend + backend + 系统安装文件 | 首次安装、修复或完整升级 |

在 renderer/shell 完成拆分前，`frontend` 保持为一个需要重启 Electron 的组件。

## 3. 稳定目录

第一阶段保持 Electron 当前目录约定：

```text
DeepX/
├── DeepX.exe
├── deepx-updater.exe
├── uninstall.exe                 # 兼容旧卸载入口，仅转发
├── .deepx-install-root.json       # 与规范化安装根绑定的破坏性操作哨兵
├── install-state.json
├── resources/
│   ├── app.asar
│   ├── deepx-daemon.exe
│   └── daemon-manifest.json
└── .deepx-update/
    ├── pending.json
    └── staging/<operation-id>/
        ├── operation.json
        ├── *.zip
        └── runner/deepx-updater.exe
```

1.0 引入稳定 launcher 后，再迁移为 `runtime/`、`components/` 和版本目录。
Updater 协议不依赖物理目录，因此迁移不改变 UpdateSource/Catalog 接口。

## 4. UpdateSource

Updater 只依赖以下抽象：

```text
read_catalog() -> bytes
open_artifact(relative_path) -> byte stream
describe() -> local-sfx | local-directory | http
```

实现顺序：

1. `DirectoryUpdateSource`：开发和测试目录。
2. `EmbeddedSfxUpdateSource`：Installer 尾部内嵌归档。
3. `HttpUpdateSource`：未来通过 HTTPS 获取 catalog 和 artifact。

Catalog 中只保存规范化相对路径。HTTP Source 相对 catalog URL 解析，本地 Source
相对其根目录解析。Source 不允许 artifact 路径逃逸根目录。

## 5. Catalog v1

Catalog 描述目标 release 和可选择的 artifact。示例见
`schemas/update-catalog.v1.example.json`。

核心字段：

```text
formatVersion
releaseId
channel
publishedAt
components
artifacts[]
```

Artifact 的 `kind` 描述它覆盖的组件；`targets` 描述应用后的组件 build；
`requires` 描述可应用条件；`payload` 描述 Bundle 的相对路径、大小和 SHA-256。

V1 的“增量”是组件级增量。二进制差分以后使用 `strategy=binary-delta` 扩展，
并必须包含精确的 `baseSha256`；基础文件不匹配时回退到组件完整包或 Full。

## 6. Bundle Manifest v1

Bundle 是不可变归档，根目录包含：

```text
bundle.json
files/...
```

当前 manifest 字段继续有效：

```json
{
  "formatVersion": 1,
  "kind": "backend",
  "buildId": "backend-c789",
  "appVersion": "1.0.0-beta.1",
  "releaseId": "20260727.1",
  "channel": "stable",
  "components": {
    "backend": {
      "buildId": "backend-c789",
      "version": "1.0.0-beta.1",
      "controlProtocol": 1
    }
  },
  "requiresFullInstall": true,
  "files": [
    {
      "source": "files/resources/deepx-daemon.exe",
      "target": "resources/deepx-daemon.exe",
      "size": 26260480,
      "sha256": "..."
    }
  ]
}
```

Catalog 决定“选哪个 Bundle”，Bundle manifest 决定“允许写哪些文件”。Updater
必须同时验证 artifact SHA-256 和每个文件 SHA-256。

## 7. Installed State v2

现有扁平 `install-state.json` 在 updater 首次运行时迁移为 V2。示例见
`schemas/install-state.v2.example.json`。

每个组件记录：

- `current`：当前 build。
- `previous`：可回滚 build。
- `health`：`unknown | healthy | failed`。
- `protocol`：组件间兼容协议（适用时）。

已暂存事务写入 `.deepx-update/staging/<operation-id>/operation.json`，只有 Bundle
文件校验和替换完成后才提交 current/previous 指针。独立健康检查与失败自动回滚仍是
下一阶段工作。

## 8. 更新规划

Updater 输入：

```text
InstalledState + Catalog + UpdatePolicy
```

V1 规划规则按顺序执行：

1. 没有有效安装状态或安装文件校验失败：选择 `full`。
2. Electron Runtime 目标发生变化：选择 `runtime`，不存在则选择 `full`。
3. frontend 和 backend 同时变化且协议不兼容：选择 `full`。
4. 只有 backend 变化且协议兼容：选择 `backend`。
5. 只有 frontend 变化：选择 `frontend`。
6. renderer/shell 拆分后，优先选择覆盖面最小的 artifact。
7. 无适用的最小 artifact：回退 `full`。

Updater 输出确定性的 `UpdatePlan`：

```json
{
  "operationId": "op-...",
  "mode": "update",
  "artifacts": ["backend-c789"],
  "actions": [
    "stage",
    "prepareBackend",
    "applyBackend",
    "restartBackend",
    "verifyBackend",
    "commit"
  ]
}
```

## 9. Backend 无窗口退出升级

Electron 的 `DaemonControlClient` 已具备 daemon identity 检查、`stop-if-idle`、
等待退出、重新拉起、重连和 session re-attach。Updater 复用该生命周期。

```text
Updater                 Electron Main              daemon
   | prepareBackend(op)      |                       |
   |------------------------>| maintenance=true      |
   |                         | session.activity      |
   |                         |---------------------->|
   |                         | stop-if-idle          |
   |                         |---------------------->|
   |<----- ready | busy -----|                       |
   |                                                 |
   | apply daemon + manifest                         |
   |                                                 |
   | commitBackend(op, identity)                     |
   |------------------------>| launchDaemon()         |
   |                         |----------------------->|
   |                         | reconnect + reattach   |
   |<-------- healthy -------|                        |
```

必须新增显式 maintenance 状态：

- 清除 reconnect/upgrade timer。
- 拒绝或排队新的 backend request。
- daemon busy 时返回 `deferred`，禁止强制退出活动任务。
- daemon 停止后不允许自动拉起旧文件。
- updater 应用成功后重新读取 `daemon-manifest.json`，再启动 daemon。
- 新 daemon identity/协议验证失败时恢复 `.previous` 并拉起旧 daemon。

## 10. Frontend 更新

当前 frontend 是整个 `app.asar`，不能安全热替换：

1. Updater 将 Bundle stage 到 pending。
2. Electron 显示“重启完成更新”。
3. 用户确认后，Electron 释放 lease、断开 daemon，但不停止 daemon。
4. Electron 让已安装 updater 执行 `handoff`；updater 将自身复制到事务 runner。
5. Electron 退出；updater 替换 `app.asar` 并启动 `DeepX.exe`。
6. 新 Electron 连接仍在运行的兼容 daemon。

renderer/shell 拆分后，纯 renderer 更新可通过切换 renderer build 指针并 reload
BrowserWindow 完成；shell、preload 或 main 变化仍需重启 Electron。

## 11. 本地协调接口

当前 MVP 使用两层接口：

```text
Installer --push-update -> updater stage
updater -> .deepx-update/pending.json -> Electron Main IPC -> renderer UI
```

`pending.json` 是持久通知，即使 Electron 当时未运行，下次启动也能恢复提示。
renderer 只能通过 context-isolated preload 调用 `checkUpdate/applyUpdate`，不能直接
指定任意程序；Electron Main 会验证 operation 位于当前安装的 staging 根目录内。

若以后需要 installer/updater 主动推送细粒度进度，再增加当前用户 ACL 的 Named
Pipe；MVP 不为单机离线更新引入常驻服务或额外 IPC 服务端。

## 12. 事务状态机

```text
discovered
  -> planned
  -> staged
  -> preparing
  -> applying
  -> restarting
  -> verifying
  -> committed

任何阶段失败
  -> rolling_back
  -> rolled_back | failed
```

幂等约束：

- 同一 `operationId` 重复请求返回当前状态。
- `committed` 不重复应用。
- `applying` 中断后根据 transaction 和文件 hash 恢复或回滚。
- current 指针只在 `verifying -> committed` 时更新。

## 13. 安全边界

- 所有路径必须是安装根目录内的规范化相对路径。
- Bundle 路径只接受 `/` 分隔符；拒绝反斜杠、盘符、空段、`.` 和 `..`。
- 卸载和 updater 写入前必须验证 `.deepx-install-root.json` 与当前规范化安装根完全
  匹配；不能仅凭目录名或 `DeepX.exe` 存在判断。
- 用户数据使用独立的 `.deepx-data-root.json`，同时绑定当前用户 home 与规范化数据
  路径；没有数据哨兵时 `--delete-user-data` 必须在删除程序文件之前失败。
- 安装根和所有待遍历父目录禁止符号链接、Junction 和其他 reparse point。
- 递归删除安装目录只能接收内部 `VerifiedInstallRoot`，并在停止进程后、真正删除前
  再验证一次哨兵和危险目录黑名单。
- 删除前后各扫描一次目录树；超过 100,000 个条目或 20 GiB 的异常安装目录直接
  拒绝卸载，避免任何误判退化为磁盘级递归删除。
- stage 完成前不修改 current 文件。
- Bundle 和文件必须双层 SHA-256 校验。
- 联网版本要求 catalog 签名；HTTPS 不能替代发布签名。
- Named Pipe 仅限当前用户，并验证 operation/installation identity。
- Backend 活跃任务不强杀。
- Full 更新通过 staging 中的 runner 副本替换已安装 updater，避免覆盖正在运行的
  updater 自身。

生命周期维护同样复用 runner 接管机制，但保持两种显式操作：

```text
MaintenanceOperation
├── Update      # 可暂存、验证和回滚
└── Uninstall   # 显式破坏性操作，不允许由空 Catalog 推导
```

Windows `ModifyPath` 指向 `deepx-updater.exe maintain --interactive`；
`UninstallString` 指向 `deepx-updater.exe uninstall --interactive`。卸载 runner
必须先验证安装目录身份，只终止可执行文件位于该安装目录内的 DeepX 进程，并在程序
文件成功删除后再删除快捷方式和注册表。用户数据默认保留。

## 14. 实施阶段

### M1：共享协议与本地 Source

- [x] 新建 `deepx-update` library。
- [x] 将 Bundle 解析、路径校验、SHA-256、原子替换和 state 写入迁移进去。
- [x] 实现 Catalog 解析、Installed State V2 和确定性 planner。
- [x] Installer 使用同一 library 写入组件化状态。
- [x] 生成 `catalog.json + bundles/*.zip` 的本地 DirectoryUpdateSource。

### M2：Backend MVP

- [x] 新建 `deepx-updater.exe`。
- [x] 实现 Directory Source、规划、双层校验、stage 和 apply。
- [x] 通过 Electron Main 协调 `stop-if-idle`、替换、重启和 session re-attach。
- [x] 新 daemon 连接会校验 manifest identity；失败时恢复 `.previous` 与状态快照。
- [ ] EmbeddedSfx Source（当前由 installer `--push-update` 投递目录 Source）。

### M3：Frontend staged restart

- [x] 实现 durable pending、runner handoff 和 relaunch。
- [x] UI 显示更新状态和立即/稍后重启。
- [x] Electron 重启时保留 daemon。
- [x] runner 使用进程句柄显式等待旧 Electron 退出。
- [x] 新 Electron 在 renderer + daemon 就绪后写健康回执；Frontend 失败自动回滚。
- [ ] Full 失败自动回滚依赖 1.0 的版本化 runtime/launcher 目录；当前由 Full
  Installer 执行修复。
- [x] updater 提供统一维护 UI 和显式 uninstall 操作。
- [x] Windows 注册 ModifyPath、交互卸载和静默卸载入口。
- [x] 旧 `uninstall.exe` 缩减为 updater 兼容转发器。

### M4：Renderer/Shell 与联网 Source

- 拆分 renderer/shell。
- 实现 renderer reload。
- 实现签名 catalog 和 `HttpUpdateSource`。
- 需要时增加精确 base hash 的二进制差分。

## 15. V1 明确不做

- updater 常驻 Windows Service。
- 未经用户允许强制结束活动任务。
- 在运行中的 Electron 内覆盖 `app.asar`。
- 仅靠文件名或版本号选择二进制差分。
- Installer 和 updater 各自维护一套安装算法。
