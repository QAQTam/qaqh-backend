# QAQH Backend Docs

> **文档重置日期：2026-09-25**
> **当前基线：`f7d2d8a`**
>
> 从本文件起，`docs/current/` 是唯一权威文档区。`docs/archive/` 只保留历史，
> 不再作为实现、接口、排期或验收依据。

## 1. 目录规则

```text
docs/
├── README.md                 # 本入口
├── current/                  # 当前权威文档
│   ├── README.md
│   ├── architecture.md       # 当前代码架构
│   ├── decisions.md          # 已冻结决策
│   ├── status.md             # 当前完成度与基线
│   ├── debug-backlog.md      # 当前待办与 debug 清单
│   ├── handoff/              # 新交接文档（从 2026-09-25 起）
│   └── spec/                 # 新契约/规范（从 2026-09-25 起）
└── archive/                  # 历史文档，只读，不作当前依据
```

## 2. 权威顺序

发生冲突时按以下顺序判断：

1. 运行中的代码与测试；
2. `docs/current/`；
3. `docs/archive/`（仅用于追溯历史，不用于当前结论）。

## 3. 新文档规则

- 新文档只写 `docs/current/`。
- 新 spec 放 `docs/current/spec/`。
- 新 handoff 放 `docs/current/handoff/`。
- 文档必须写：
  - 日期；
  - 基线 commit；
  - 状态（draft / accepted / implemented / superseded）；
  - 适用代码路径。
- 不把历史文档复制回 `docs/current/`；需要新结论时重新写当前文档。

## 4. 归档规则

- 旧文档只通过 `git mv` 进入 `docs/archive/`，保留历史。
- 归档内容不修改正文语义；需要修正时在 `docs/current/` 写新文档并引用归档。
- 归档文档中的链接不再保证跟随新目录维护。
