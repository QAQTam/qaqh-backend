# DeepX Installer

DeepX 桌面应用的 Windows 安装与统一维护程序。

- **安装器**：egui GUI 向导（macOS 风格），支持 SFX 单文件分发、进程检测与关闭
- **维护程序**：`deepx-updater.exe`，统一负责修改、更新、修复与安全卸载
- **更新架构**：见 [`UPDATE_ARCHITECTURE.md`](UPDATE_ARCHITECTURE.md)

## 构建

在 monorepo 根目录运行：

```powershell
just package                 # 完整安装包 EXE
just package-update          # Full + Frontend + Backend 智能更新源
just package-update-frontend # 仅前端更新源
just package-update-backend  # 仅后端更新源
```

`package` 将成品写入仓库根目录的
`packages/DeepXInstaller-Full-<build-id>.exe`。
SFX 携带 `bundle.json`，安装时按文件白名单和 SHA-256 校验执行。
三个 `package-update*` 命令生成 `packages/update-source/catalog.json` 与
`packages/update-source/bundles/*.zip`，
不会额外拼接组件 SFX；
catalog 位于 artifact 外部，因此不存在“ZIP 对自身做哈希”的循环依赖。
`package-update` 只构建一次 Electron 目录，Frontend artifact 直接复用 Full 中
同一份 `app.asar`，保证 catalog 的组件 build identity 与 Full fallback 一致。
更新源目录还包含 `DeepXUpdate.exe`；双击后会把同目录 catalog 投递到当前用户的
默认 DeepX 安装。它只负责投递和暂存，不会在后台强制结束正在运行的程序。

旧命令 `package-full`、`package-installer`、`package-update-all`、
`package-update-full`、`package-frontend` 和 `package-backend` 仍可调用，但已从
`just --list` 隐藏；新脚本应使用上面的四个入口。

默认安装目录为 `%LOCALAPPDATA%\Programs\DeepX`。快捷方式和卸载信息均写入当前
用户范围，不要求管理员权限。Frontend/Backend 包要求目标目录已有完整安装。
Windows“修改”入口启动 `deepx-updater.exe maintain --interactive`，“卸载”入口
启动独立的 `uninstall` 操作；卸载不会被解释为空更新。
完整安装还会写入绑定规范化安装路径的 `.deepx-install-root.json`。维护程序只有在
该标记、`install-state.json`、`DeepX.exe` 和 updater 同时验证通过时才允许修改或
删除安装目录。
daemon 初始化用户数据时会另行写入 `.deepx-data-root.json`，绑定当前用户身份与
规范化数据路径。选择“同时删除用户数据”时必须再次验证该标记；缺失或不匹配只会
拒绝删除，不会根据 `.deepx` 目录名猜测所有权。

安装器根据包类型和目标目录区分操作：

- Full + 目标不存在：完整安装
- Full + 目标已有 `DeepX.exe`：完整升级
- Frontend/Backend：组件更新

组件替换会保留 `<文件名>.previous`，并更新 `install-state.json`。无界面验证或
外部 updater 可使用 `DeepXInstaller-*.exe --apply-self <目标目录>`。

现有安装可由 installer 将本地更新源投递给已安装的 updater：

```powershell
DeepXInstaller.exe --push-update <update-source目录> <DeepX安装目录>
```

该命令只做 catalog 交付、规划和暂存，不直接覆盖运行中的文件。updater 会写入
`.deepx-update/pending.json`；运行中的 Electron 随后显示更新提示，并根据计划执行
daemon 独立重启或 Electron 退出后的前端/完整替换。

调试时也可直接使用 updater：

```powershell
deepx-updater inspect <update-source>
deepx-updater plan <update-source> <安装目录>
deepx-updater stage <update-source> <安装目录>
deepx-updater apply-staged <operation.json> <安装目录>
deepx-updater rollback-staged <operation.json> <安装目录>
deepx-updater maintain --interactive --install-dir <安装目录>
deepx-updater uninstall --interactive --install-dir <安装目录>
```

## 项目结构

```
├── justfile                   # 构建编排
├── Cargo.toml                 # Rust 依赖
├── src/
│   ├── main.rs                # 安装器 UI（egui）
│   ├── install.rs             # 安装引擎（SFX / 目录双模式）
│   ├── win_process.rs         # Windows 进程检测与终止
│   └── bin/
│       └── uninstall.rs       # 旧卸载入口兼容转发器
├── scripts/
│   ├── collect-payload.ps1    # 按 full/frontend/backend 收集并生成 manifest
│   ├── finalize.ps1           # 按 Bundle 生成独立 SFX
│   ├── clean.ps1
│   └── status.ps1
└── staging/                   # 构建生成的三类 Bundle（不入库）
```

默认构建把 Bundle 写入 `staging/builds/<kind>/<build-id>/`，并用
`staging/<kind>.latest.json` 指向最新产物。这样无需删除可能正被 Windows
索引器占用的旧 `app.asar`；`just clean` 再统一清理历史 staging。
