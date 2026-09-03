---
name: agent-workspace-init
description: 初始化 Agent workspace，建立知识分层、任务空间和 MemRoute 本地检索。当用户提到“AGENT workspace 初始化”时触发。
---

# Agent Workspace 初始化与知识治理

## 触发条件

当用户输入包含“AGENT workspace 初始化”时执行本 Skill。

## 目标与原则

建立可持续维护的任务空间：知识可以增长，但每次任务只装配经过路由的最小证据上下文。

- `AGENTS.md` 保存简短的全局规则和知识导航；`memory/` 保存已确认、可复用、可追溯的长期知识；`tasks/` 保存任务过程、证据和产物。
- 每项关键事实只保留一个 canonical 来源；源码、OpenAPI、运行平台、工单和其他外部系统仍是原始权威来源。
- 临时观察、命令输出、备份、生成物和未确认推断留在任务目录，不能直接提升为长期事实。
- 不记录密码、Cookie、Token、私钥、个人数据、内部地址或完整敏感请求体。
- MemRoute 只提供候选证据和路由计划；索引可删除、可重建，不能取代源文件或权威系统。

## 目录结构

```text
<collection-root>/
├── memroute.toml
├── .gitignore
├── <workspace>/
│   ├── AGENTS.md
│   ├── memory/
│   ├── tasks/<task-name>/task.md
│   └── scripts/<task-name>/
└── .memroute/
```

`<collection-root>` 是多个可选 workspace 的注册根目录；`.memroute/` 保存 Tantivy 索引和 manifest，属于派生物，必须由 `.gitignore` 忽略。

## 五类信息与加载策略

| 类型 | 定义 | 默认位置 | 加载策略 |
|---|---|---|---|
| 全局规则 | 安全约束、目录规范、代码引用规范、工作边界 | `AGENTS.md` | L0；每次任务加载，保持简短 |
| 路由知识 | 主题入口、适用条件、跨 workspace 边界 | `AGENTS.md` 的“知识导航” | L0；由 MemRoute 建为 `route` 索引 |
| 可复用长期知识 | 已确认的稳定事实、约束、操作手册与决策结论 | `memory/` | L2；索引为 `stable_fact` 或 `runbook` |
| 任务过程与产物 | 临时观察、任务拆解、证据、备份、脚本和生成物 | `tasks/`、`scripts/` | 当前任务默认加载；历史任务按需检索为 `task_experience` |
| 权威外部来源 | 源码、OpenAPI、平台页面、运行数据和工单 | 源文档中的 URL、路径或锚点 | L3；索引为核验入口，不缓存敏感原文 |

## 初始化步骤

1. 确认 collection 根目录、workspace 名称、初始任务、环境、风险边界和允许的跨 workspace 关系；信息缺失时询问用户。
2. 创建每个 workspace 的 `memory/`、`tasks/`、`scripts/`，以及初始任务的同名 `tasks/<task-name>/`、`scripts/<task-name>/`。
3. 创建简短的 `AGENTS.md`，其中至少包含对象约束、行为指引、L0--L3、知识导航、跨 workspace 边界和代码引用要求。
4. 在 `<collection-root>/memroute.toml` 注册每个 workspace；根 `.gitignore` 必须忽略 `.memroute/`。
5. 只在已有可复用且可追溯事实时建立 `memory/` 内容；不要为占位创建空知识文件。
6. 在 `tasks/<task-name>/task.md` 记录目标、拆解、环境、风险边界和已加载知识。

最小 `memroute.toml` 结构如下，workspace 名称与路径必须由当前用户的 collection 决定：

```toml
index_dir = ".memroute/index"
manifest_path = ".memroute/manifest.json"
ignore_paths = ["backups", "generated", "data", ".memroute", ".git"]

[[workspace]]
name = "example-workspace"
path = "example-workspace"
enabled = true
```

## 使用本仓库的 MemRoute

令 `<memroute-repository>` 为本仓库克隆路径，`<collection-root>` 为目标 collection 根目录。所有命令均使用仓库中的 CLI，不依赖机器固定路径：

```bash
cargo run --manifest-path <memroute-repository>/Cargo.toml -- --root <collection-root> index
```

若当前目录就是 `<collection-root>`，可以省略 `--root <collection-root>`：

```bash
cargo run --manifest-path <memroute-repository>/Cargo.toml -- index
```

## 任务开始流程

1. 创建当前任务的 `tasks/<task-name>/`、`scripts/<task-name>/` 与 `task.md`。
2. 读取所属 workspace 的 L0 `AGENTS.md`，从任务请求提取 L1：目标、实体、环境、动作、生命周期与风险。
3. 运行 `index`，再执行以下路由命令，获取最多 3--8 个 L2 证据片段：

```bash
cargo run --manifest-path <memroute-repository>/Cargo.toml -- --root <collection-root> route "<task-summary>" --workspace <workspace> --intent <action> --entity <entity> --environment <environment> --limit 3 --json
```

4. 仅当所属 workspace 的知识导航明确允许时，附加 `--include-workspace <workspace>` 扩大范围。
5. 按路由输出中的 `path`、`heading_path` 与行号读取源内容；稳定事实和操作手册优先于任务经历。
6. 涉及生产变更、权限、删除、迁移、数据一致性或其他高风险行为时，必须添加 `--risk high`，并读取返回的 L3 authority 入口、核验原始系统后再执行。
7. 在 `task.md` 的“已加载知识”记录实际采用的文件路径、章节/行号和 L3 核验来源。

## 知识维护流程

- 更新 `memory/`、`AGENTS.md` 或任务记录后运行 `index`；source hash 会跳过未变化的输入。
- 修改长期知识或知识导航后，执行 `impact <workspace>/<path> --json`，检查受影响的索引片段和路由。
- 定期或实质更新后执行 `audit --json`，检查导航断链、无人路由的长期知识和规模阈值。
- 任务结束后执行 `review --workspace <workspace> --task <task-name> --json`；该命令只提出候选，必须由人工确认可复用性、正确性和权威来源后，才更新 `memory/` 或 `AGENTS.md`。

## Task 文件模板

```markdown
# [任务名称]

## 状态
[未开始 / 进行中 / 已完成 / 已搁置]

## 目标
[目标、环境、边界与风险]

## 已加载知识
- `memory/<topic>.md`：第 N 节（行 X--Y）；用途：...
- [权威来源标题](https://example.com)：核验项：...

## 拆解
- [ ] 子任务 1
- [ ] 子任务 2

## 记录
- YYYY-MM-DD: [进展、证据、决策与 MemRoute 审计/复盘结果]
```

## 小规模升级边界

当前以 Tantivy 的标题级全文、实体和路径检索为主，不建设服务化 RAG。只有在 `memory/` 超过约 30 个主题、任务反复需要检查超过 8 个候选片段、跨 workspace 成为常态，或 MemRoute 无法稳定处理同义表达时，再评估加入语义召回或服务化混合检索。
