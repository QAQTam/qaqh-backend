# 已实测的宿主注入示例（bun / fnm / corepack-pnpm）

以下全部在某 Arch Linux + KDE + systemd user daemon 宿主上端到端验证
（2026-11-26）。形态可移植，路径需按实际宿主改写。

## `~/.config/qaqh/config.toml` 成品示例

```toml
[exec]
[exec.env]
# 直装型：bun（安装器当年把 export 写进了 .bashrc/.bash_profile，daemon 不吃）
BUN_INSTALL = "/home/qaqtam/.bun"
# fnm 用 aliases/default 稳定目录（node/npm/npx/corepack/pnpm-shim 都在其中；
# 用户 `fnm alias default vX` 重定别名，此处无需改动自动跟随）
PATH = "/home/qaqtam/.bun/bin:/home/qaqtam/.local/share/fnm/aliases/default/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin"
# corepack 遇未缓存版本默认弹 [Y/n] 交互确认，会挂住 exec 会话（实测复现）
COREPACK_ENABLE_DOWNLOAD_PROMPT = "0"
```

## 要点逐条

- **fnm**：每 shell 的动态目录在 `~/.local/state/fnm_multishells/<pid>_<ts>/`
  ——一次性、会被复用编号、绝不能注入。稳定入口是
  `~/.local/share/fnm/aliases/<alias>/bin`。若宿主没设 default 别名，
  让用户跑一次 `fnm alias default <ver>`。
- **pnpm/yarn 多版本**：alias 目录里的 `pnpm` 实为指向 corepack dist 的
  shim 软链，运行时按当前工作目录的 `package.json#packageManager` 逐项目
  选版本（缓存在 `~/.cache/node/corepack`）。只要 PATH 里有该 shim，自适应
  语义与人手一致；**不要**给某个 pnpm 版本写绝对路径。
- **验证矩阵实例**（同一注入，三个项目目录各钉不同版本）：

  ```
  packageManager pnpm@9.15.4  → pnpm -v = 9.15.4
  packageManager pnpm@10.26.2 → pnpm -v = 10.26.2
  packageManager pnpm@10.14.0 → pnpm -v = 10.14.0（未缓存，自动下载零交互）
  HOME 直下（无 packageManager）→ 12.23.0（corepack 缺省版）
  node v26.13.1 / npm 11.20.0 / bun 1.4.5 裸命令全通
  ```

- **改前改后对照**：未注入时 exec 里 `which bun` 未命中且 PATH 仅
  `/usr/local/bin:/usr/bin:/bin:...`；注入后 `sleep 2.5` 即得上述结果，
  daemon 全程未重启、既有会话未中断。
