# 临时本地 Git 基础设施方案（GitHub 故障应急）

> 建立：2026-08-18（GitHub 持续异常约 4h+）
> 状态：~~当前有效（应急）~~ → **已收敛为 Gitea 唯一远端**
> 本文是唯一详细文档；HANDOFF 只放指向，不重复维护具体步骤。

> **【2026-08-18 后续变更】**：裸仓库 `~/git/qaqh-backend.git` **已删除**，`sshd-lan`(2222) **已停用**。
> 现在 **Gitea 是唯一 Git 远端**：Linux `origin` 与 Windows 均走
> `http://60.0.0.32:3000/qaq/QAQ-Harness.git`，`git push/pull` 体验与 GitHub 一致。
> 下文除 **Gitea 通道**外，SSH 裸仓库 / git:// 均视为**历史/已退役**，仅作排障参考。

---

## 0. 一句话结论

本机 `60.0.0.32` 以 **Gitea 为唯一已验收的 Git 通道**，供 Linux / Windows（`60.0.0.65`）在 GitHub 不可用期间 push / clone：

| 通道 | 地址 | 状态 |
|---|---|---|
| ~~SSH（用户级 sshd）~~ | ~~`ssh://qaqtamsy@60.0.0.32:2222/...`~~ | 已退役（裸仓库已删，sshd-lan 已停用） |
| **Gitea（唯一远端）** | `http://60.0.0.32:3000/qaq/QAQ-Harness.git` | ✅ 已验证 clone + push；Linux `origin` 即它 |
| ~~git:// daemon~~（已弃用） | ~~`git://60.0.0.32:9418/...`~~ | ⛔ receive-pack 对 Windows push 卡死，已停用 |

~~裸仓库：`/home/qaqtamsy/git/qaqh-backend.git`（已删除）~~
（历史基线已迁入 Gitea：`main = 86812b9`，另含 `docs/t17`、`refactor/nextdev-tool-mode-single-contract`）

---

## 1. 为什么会有这套方案

- GitHub 崩溃（API 与常规通道间歇 503）超过数小时。
- 过程中发现 `git://`（9418）**对 Windows 的 push 会卡死**：
  - 服务器已启动 `git receive-pack`，TCP `ESTABLISHED`、收发队列 `0/0`、服务端 0% CPU 干等；
  - 客户端（fetch/ls-remote 正常，push 不发数据）疑似协议 v2 / 客户端停滞问题；
  - 结论：`git://` 只适合本机/可靠性要求低的场景，LAN 远程 push 不可靠。
- 因此改走 **SSH 裸仓库**（稳定、类 GitHub 体验）+ **Gitea**（带 Web/账号的临时替代）。

---

## 2. ~~通道一：SSH 直推裸仓库（端口 2222）~~（**已退役**）

### 部署要点（用户级，无需 root）

- 二进制：`/usr/bin/sshd`（系统自带）。
- 配置：`~/.ssh/sshd_config`
  - `Port 2222`、`ListenAddress 0.0.0.0`
  - `HostKey ~/.ssh/ssh_host_ed25519_key`
  - `AuthorizedKeysFile .ssh/authorized_keys`
  - `PasswordAuthentication no`（仅公钥）
  - `AllowUsers qaqtamsy`
- 主机密钥：`~/.ssh/ssh_host_ed25519_key`
  - 指纹（首次连接核对）：`SHA256:sXnOv779Ji1Kyzhbutxe6Re/M5Q+s2Ha04J61r5zTuo`
- 授权公钥：`~/.ssh/authorized_keys`
  - 已含：本机自测钥 + Windows 的 `win-qaqh`（ED25519）
- 日志：`~/.ssh/sshd.log`（INFO）/ `~/.ssh/sshd-debug.log`（DEBUG 排障时用）

### 启动 / 管理（已注册为 systemd 用户服务 + 开启 linger）

成对服务：`gitea.service`（3000）与 `sshd-lan.service`（2222）都注册为 **systemd 用户服务**，
且已 `loginctl enable-linger qaqtamsy`（**Linger=yes**）→ **关机重启后不登录也自动拉起**。

```bash
# 统一用 systemctl --user 管理（不要再用裸命令行起 sshd）
systemctl --user status sshd-lan
systemctl --user restart sshd-lan
systemctl --user stop sshd-lan        # 停止
systemctl --user disable sshd-lan     # 取消开机自启

# 排障用 DEBUG 日志（先 stop 再手动前台跑，日志到文件）
systemctl --user stop sshd-lan
/usr/bin/sshd -D -f ~/.ssh/sshd_config -o LogLevel=DEBUG -E ~/.ssh/sshd-debug.log

# 加新 Windows 公钥（无需重启，改完即生效）
echo "<公钥内容>" >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys
# 看是否有人连过/推过
tail -n 100 ~/.ssh/sshd.log
```

### Windows 端用法

```powershell
# push（直接推分支）
git push ssh://qaqtamsy@60.0.0.32:2222/home/qaqtamsy/git/qaqh-backend.git <分支名>
# 存成 short name 方便用
git remote add lan-ssh ssh://qaqtamsy@60.0.0.32:2222/home/qaqtamsy/git/qaqh-backend.git
git push -u lan-ssh <分支名>
# clone
git clone ssh://qaqtamsy@60.0.0.32:2222/home/qaqtamsy/git/qaqh-backend.git
```

> 端口 2222 是因为用户级 sshd 无法绑 22；URL 必须带端口。

---

## 3. 当前唯一通道：Gitea（HTTP，端口 3000）

### 部署要点（用户级，无需 root）

- 来源：从 Arch `extra` 镜像直接下载 `gitea-1.27.2-1-x86_64.pkg.tar.zst` 解包，非系统安装。
- 二进制：`~/gitea/root/usr/bin/gitea`（1.27.2，bindata+sqlite 内置）。
- 配置：`~/gitea/custom/conf/app.ini`
  - `[server] HTTP_ADDR=0.0.0.0 HTTP_PORT=3000 DOMAIN=60.0.0.32 ROOT_URL=http://60.0.0.32:3000/ DISABLE_SSH=true`
  - `[database] DB_TYPE=sqlite3 PATH=~/gitea/data/gitea.db`
  - `[repository] ROOT=~/gitea/repos DEFAULT_PRIVATE=private`
  - `[security] INSTALL_LOCK=true`
  - `[service] DISABLE_REGISTRATION=false`（应急期间开放，退役前提防）
- 数据：`~/gitea/data`（DB）/ `~/gitea/repos`（仓库）/ `~/gitea/log`
- **systemd 用户服务**（关键：替代裸 `nohup`，避免被会话回收）：
  - 单元：`~/.config/systemd/user/gitea.service`
  - `Restart=on-failure`、开机自启
  - 已配合 `loginctl enable-linger qaqtamsy`（**Linger=yes**）→ 关机重启后不登录也自启
- 管理员：用户名 `qaq`，密码存在 `~/gitea/.admin-password`（**不要把明文写进 git**）
- 密钥：`~/gitea/.secret`（app.ini 的 `SECRET_KEY`）

### 为什么没用 `nohup` 裸跑

第一次用 `nohup gitea web ... &` 挂在工具会话里，会话结束收到 `SIGTERM` 被杀。
凡是要“跨会话常驻”的进程，优先：
1. **systemd 用户服务**（`systemctl --user`）
2. 或 `setsid ... &`（脱离进程组）
git-daemon/sshd 因为能自行 daemon 化所以没事；本方案里 sshd 也统一收编为 systemd 用户服务（`sshd-lan.service`）。

### 管理命令

```bash
systemctl --user status gitea
systemctl --user restart gitea
journalctl --user -u gitea -f          # 日志
```

### 后台建新管理员（gitea 1.27 语法已变，用 `admin user`）

```bash
gitea="~/gitea/root/usr/bin/gitea"
$gitea -w ~/gitea -C ~/gitea/custom -c ~/gitea/custom/conf/app.ini \
  admin user create --username <名> --password '<强密码>' --email <邮箱> --admin
$gitea -w ~/gitea -C ~/gitea/custom -c ~/gitea/custom/conf/app.ini admin user list
```

### Windows 端用法

```powershell
git clone http://60.0.0.32:3000/qaq/<仓库>.git
git remote add gitea http://60.0.0.32:3000/qaq/<仓库>.git
git push gitea <分支>
```

（HTTP 鉴权用 qaq 或其他已建用户；Gitea 内建 SSH 目前关闭，避免与 2222 的 sshd 抢 `authorized_keys`。）

---

## 4. Windows 侧排查要点（这次踩过的坑）

- `git://`（9418）push 会在 `receive-pack` 阶段卡死：
  - 特征：服务器 `git receive-pack` 挂着 0% CPU、TCP `ESTABLISHED`、队列 `0/0`；
  - fetch/ls-remote 正常 → 是"客户端起不来"而不是"服务器拒绝"；
  - 绕过：改用 SSH 或 HTTP（已验证）。
- 若之前为排查设过 `git config --global protocol.version 0`，SSH/HTTP 通道下可还原：
  `git config --global --unset protocol.version`

---

## 5. 安全与退役清单（GitHub 恢复后）

- 安全现状（应急可接受，长期不可）：
  - Gitea 是**明文 HTTP**、`DISABLE_REGISTRATION=false`、绑定 `0.0.0.0`；
  - sshd 是**公钥**登录但也是 LAN 全开放；
  - 只限局域网，**不要**暴露公网。
- 退役步骤：
  1. `systemctl --user disable --now gitea`
  2. `systemctl --user disable --now sshd-lan`
  3. （可选）`loginctl disable-linger qaqtamsy` 关闭不登录自启
  4. （可选）删除 `~/gitea`；保留或删除 `~/.ssh/ssh_host_*`、`authorized_keys` 按需
  5. 陈旧 SSH key 从 `~/.ssh/authorized_keys` 移除
- 若长期需要自建 Git：再补 TLS（Caddy/Let's Encrypt）、域名、关闭开放注册、启用 Gitea 内建 SSH（单独端口）。

---

## 6. 状态速查（2026-08-18）

| 项 | 值 |
|---|---|
| 主远端 | **Gitea** `http://60.0.0.32:3000/qaq/QAQ-Harness.git`（Linux + Windows 统一）|
| Linux origin | `http://60.0.0.32:3000/qaq/QAQ-Harness.git`（main 已跟踪 origin，`git push` 即 Gitea）|
| Gitea 分支 | `main=86812b9`、`docs/t17-frontend-bug-inventory=da20293`、`refactor/nextdev-tool-mode-single-contract=7c3b0b3` |
| ~~裸仓库~~ | ~~`/home/qaqtamsy/git/qaqh-backend.git`~~ **已删除** |
| ~~sshd-lan~~ | ~~2222~~ **已停用/取消自启**；~~git-daemon 9418~~ 已停 |
| 开机自启 | `Linger=yes`；仅 `gitea.service` `enabled`，关机重启后不登录也自动拉起 |
