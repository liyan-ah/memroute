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
- MemRoute 只提供候选证据和路由计划；索引与本地事件日志可删除、可重建，不能取代源文件或权威系统。

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

令 `<memroute-repository>` 为本仓库克隆路径，`<collection-root>` 为目标 collection 根目录。日常检索优先使用已安装且位于 `PATH` 的 CLI：

```bash
memroute --root <collection-root> index
```

若当前目录就是 `<collection-root>`，可以省略 `--root <collection-root>`：

```bash
memroute index
```

仅在首次安装或升级可执行文件，以及开发、调试或验证尚未安装的仓库源码时，使用：

```bash
cargo run --manifest-path <memroute-repository>/Cargo.toml -- --root <collection-root> <command>
cargo install --path <memroute-repository> --force
```

不得手动加载、复制或提交 `target/` 编译产物；Cargo 将其作为本地构建缓存管理。

### 本地事件日志与反馈

MemRoute 默认在 `<collection-root>/.memroute/events.jsonl` 追加本地 JSONL 调用事件；该文件是派生数据，必须被忽略且不得上传。可用 `--event-log <local-jsonl>` 改用另一本地日志，或用 `--no-event-log` 关闭记录；关闭后不能使用 `feedback`。

事件仅记录命令、耗时、索引指纹、workspace/scope、风险、候选 `chunk_id`、排名/类型，以及哈希化的 `task-id`/intent。不得记录任务摘要、查询、实体、环境值、路径、标题、正文、URL 或任何敏感数据。

`route` 默认返回 `request_id` 和每个候选的 `chunk_id`。Agent 或调用方仅在实际阅读并判断候选后，通过 `memroute feedback` 更新使用记录，并使用**同一 collection 根目录和同一事件日志**回填。ID 必须从该次 route 输出的 `l2_context` / `l3_authorities` 原样取得，不得引用其他 route、重建后的检索结果或自行推导的 ID。相同 `request_id`、相同 outcome 的多个候选优先合并为一条命令并重复 `--chunk-id`，避免 `&&` 链中前一条失败阻断后续记录；回填后检查 JSON 输出：

```bash
memroute --root <collection-root> feedback --request-id <request-id> --chunk-id <chunk-id-1> --chunk-id <chunk-id-2> --outcome used --json
```

`outcome` 可为 `used`、`background`、`rejected`、`needs-l3`；在因缺少 L3 authority 或其他安全边界而停止执行时，使用 `--outcome blocked` 且不带 `--chunk-id`。反馈只能引用该 `request_id` 的 L2/L3 候选，不可写解释文本或来源原文。事件为本地 append-only JSONL，CLI 已串行化并发写入。

## 任务开始流程

1. 创建当前任务的 `tasks/<task-name>/`、`scripts/<task-name>/` 与 `task.md`。
2. 读取所属 workspace 的 L0 `AGENTS.md`，从任务请求提取 L1：目标、实体、环境、动作、生命周期与风险。
3. 运行 `index`，再执行以下路由命令，获取最多 3--8 个 L2 证据片段：

```bash
memroute --root <collection-root> route "<task-summary>" --workspace <workspace> --intent <action> --entity <entity> --environment <environment> --limit 3 --task-id <task-name> --json
```

4. 仅当所属 workspace 的知识导航明确允许时，附加 `--include-workspace <workspace>` 扩大范围。
5. 按路由输出中的 `path`、`heading_path` 与行号读取源内容；稳定事实和操作手册优先于任务经历。
6. 涉及生产变更、权限、删除、迁移、数据一致性或其他高风险行为时，必须添加 `--risk high`，并读取返回的 L3 authority 入口、核验原始系统后再执行。
7. 在 `task.md` 的“已加载知识”记录实际采用的文件路径、章节/行号和 L3 核验来源；仅为实际判断过的 route 候选按本节的 request/chunk 归属规则使用 `feedback` 回填 `used`、`background`、`rejected`、`needs-l3` 或无候选的 `blocked`。

## 知识维护流程

- 更新 `memory/`、`AGENTS.md` 或任务记录后运行 `index`；source hash 会跳过未变化的输入。
- 修改长期知识或知识导航后，执行 `impact <workspace>/<path> --json`，检查受影响的索引片段和路由。
- 定期或实质更新后执行 `audit --json`，检查导航断链、无人路由的长期知识和规模阈值。
- 任务结束后执行 `review --workspace <workspace> --task <task-name> --json`；该命令只提出候选，必须由人工确认可复用性、正确性和权威来源后，才更新 `memory/` 或 `AGENTS.md`。

### 执行记录分析

使用 `analyze` 汇总同一事件日志中的命令耗时、路由覆盖、L2/L3 候选量、反馈分布与按知识类型的 `used` 数量，并输出基于数据质量、反馈覆盖、高风险 L3 返回和反馈区分度的有效性结论。它仅读取本地脱敏事件日志，不会写入新事件：

```bash
memroute --root <collection-root> analyze --json
memroute --root <collection-root> analyze --workspace <workspace> --json
```

优先关注 `data_quality.malformed_line_count`、`feedback_route_coverage_percent`、`used_candidate_rate_among_judged_percent` 与 `conclusion`。当结论为 `insufficient_data`、`early_signal`、`insufficient_discrimination` 或 `needs_attention` 时，不应据此认定检索质量已达标；应继续以实际判断结果回填 `background`、`rejected` 或 `needs-l3`，并修复日志完整性或高风险 L3 覆盖问题。只有 `effective` 且有足够样本与反馈覆盖时，才表示当前记录支持正向有效性结论。

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
