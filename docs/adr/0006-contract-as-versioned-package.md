# ADR-0006 契约分发为版本化包 + provenance，取代手抄文件

状态: `已采纳（未实施）` · 决策日期 2026-11-10

## 背景（已核对）

- 契约产物由 `.github/workflows/release.yml:56-65` 导出并打成 `qaqh-sdk-contract-<tag>.zip`，
  经 `:73-78` 上传、`:92-103` 发布到 GitHub Release，资产说明写明"供 qaqh-sdk / qaqh-electron 消费"。
- 本仓 `package.json` 是 `{"private": true}`（仅用于携带版本戳，无任何 npm 分发）。
- 消费者是同级的 `../qaqh-electron`（`justfile:7-9,31-33`：桌面客户端已拆仓，本仓不再含前端构建）。
- `docs/lts-v3.0-maintenance.md` §6 checklist 的最后一步是人肉动作：
  "**前端仓库把依赖 pin 到新 tag**（契约 zip 里的 `QAQH_SOURCE_REVISION` 与 tag 提交一致）"。

## 问题

分发形态是"zip + 手工拷贝"，因此**跨仓对齐没有任何机器校验**。具体缺口：

1. 前端把契约文件复制进自己仓后，`contract_hash` 与它实际要连的 daemon 二进制之间**没有等式**。
   "两边都从 v3.0.3 拿的"只能靠发版人和评审的记忆保证。
2. 无法原子回滚：前端回滚代码不会自动回滚它复制走的类型定义。
3. provenance 靠人核对：`QAQH_SOURCE_REVISION` 与 tag 提交是否一致，是 checklist 上的一行字，
   不是一道检查。导出侧已经做了 dirty 拒发（`check-sdk-contract.py:55-67` 连外部 path 依赖的脏
   状态都拒），**最后一公里却退化成人工**，前紧后松。

## 决策

1. **契约是包，不是复制粘贴**：`release.yml` 发布 tag 时，把导出物发成版本化包
   `@qaqh/contract` （npm 走 GitHub Packages；Rust 侧 `qaqh-client` 门面已走 crates 语义，不重复）。
   包名带线：v3.0 线发 `3.0.x`，禁止跨线浮动 tag（与 ADR-0001 §决策 6 同源）。
2. **`contract_hash` 进包的 `package.json` 字段与 tarball 元数据**，成为机器可读的一等公民，
   而不仅在 `manifest.json` 里。
3. **前端 CI 增加一条等式校验**：
   `import 的契约 hash` == `本次要连的 daemon 二进制内嵌 hash`。不等则红。
   daemon 侧需暴露该 hash（已有 `DaemonDiscovery` 与 `qaqh-backend.lock.json` 两个现成挂载点，
   择一即可，不新建通道）。
4. **签名与 provenance**：导出已是确定性产物（`main.rs:103-171` 的 staging + 原子提交，
   `manifest.json` 逐文件 sha256，`contract_hash` 与 revision 无关），因此 SLSA provenance /
   cosign 签名几乎是白捡的收益 —— 一行"证明这个 zip 确实由该 commit 的 CI 产出"。
5. 依赖升级用机器人 PR（Renovate/Dependabot）而不是人肉 pin；LTS 线上该 PR 必须跑
   ADR-0005 的配对测试才允许合。

## 被否选项

- **维持 zip + 手工复制**：否 —— §6 checklist 已经把它写成流程，但它是**唯一没有机器兜底的一步**，
  而它偏偏是"用户空间"这一侧的落地动作。
- **前端直接依赖 GitHub Release 下载 URL**：否。浮动 URL 无 lockfile 语义、无缓存、无完整性校验，
  等于把 pin 变成"脚本里写死的字符串"。
- **前端从 daemon 运行时自省出类型**（启动时拉 `/openapi.json` 一类）：否 —— ADR-0002 已否 OpenAPI，
  且运行期拉类型会让前端编译依赖一个在线进程，破坏可离线构建。
- **发私有 registry 自建服务**：否（当前规模不值得；GitHub Packages 足够）。

## 后果与代价

- 新增一个发布面（npm 包）要维护权限与可见性：GitHub Packages 读匿名、发需 token，
  需要给 `release.yml` 加 `packages: write`（现仅 `contents: write`，`release.yml:8-10`）。
- 前端仓要改 import 路径（从相对目录复制改为包名）。这是**一次性跨仓协调**，属用户空间动作，
  须与 ADR-0005 的配对测试同时窗口内完成，否则会出现"新流程发老消费方式"的空窗。
- 包一旦发布即不可覆盖（npm 语义），这与我们已有的"禁止浮动 tag"是同一纪律，反而更硬。

## 落地清单

- [ ] `release.yml`：导出 → `npm pack` → publish 到 GitHub Packages（Linux job 内）
- [ ] 包元数据带 `contract_hash` / `source_revision` / `wire_version`
- [ ] daemon 暴露自身契约 hash（择 `DaemonDiscovery` 或 lock.json 挂载点）
- [ ] `qaqh-electron` CI：hash 等式校验 + 从包 import
- [ ] 加 provenance/签名步骤
- [ ] 手册 §6 最后一条从"人肉 pin"改为"机器人 PR 已合 + hash 等式绿"
