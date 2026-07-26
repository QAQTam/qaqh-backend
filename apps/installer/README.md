# DeepX Installer

DeepX 桌面应用的 Windows 安装器 / 卸载器。

- **安装器**：egui GUI 向导（macOS 风格），支持 SFX 单文件分发、进程检测与关闭
- **卸载器**：egui GUI，读注册表定位安装目录，TEMP 副本 + MoveFileEx 实现零孤儿进程自删除

## 构建

```bash
just all          # 全流程：后端 → 前端 → 安装器 → SFX 单文件
just frontend     # 仅前端打包
just installer    # 仅收集产物 + 构建安装器
just sfx-only     # 仅 SFX 打包（payload 已就位）
just check        # 快速编译检查
```

成品：`dist/DeepXInstaller-Setup.exe`（单文件，可直接分发）。

依赖外部项目：
- 后端：`D:\DeepX`（Rust workspace，`deepx-daemon`）
- 前端：`D:\deepx-desktop`（Electron + SolidJS）

## 项目结构

```
├── justfile                   # 构建编排
├── Cargo.toml                 # Rust 依赖
├── src/
│   ├── main.rs                # 安装器 UI（egui）
│   ├── install.rs             # 安装引擎（SFX / 目录双模式）
│   ├── win_process.rs         # Windows 进程检测与终止
│   └── bin/
│       └── uninstall.rs       # 卸载器（egui）
├── scripts/
│   ├── collect-payload.ps1    # 收集前后端产物
│   ├── finalize.ps1           # SFX 打包
│   ├── clean.ps1
│   └── status.ps1
└── payload/
    └── config/
        └── default.toml       # 默认配置模板（入库）
```
