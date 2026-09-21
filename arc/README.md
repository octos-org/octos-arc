# arc/ — 在这一个仓库里完成 ARC-Bench 的改、测、交

这个目录是 Octos 参加 ARC-Bench 的全部外围：平台适配包、公开验收测试、本地做题和打分脚本。
学员只需要这一个仓库。内核源码在上一级（`crates/`），适配包在这里。

## 五步

```sh
# 0. 准备（一次）
pip install -r arc/requirements.txt          # pyyaml、arcbench-runtime
export ARCBENCH_API_KEY=ak_...               # arc-bench.com 个人页的 API key
export NODE_BIN=/opt/homebrew/opt/node@24/bin # 你的 node 目录（Linux 一般不用设）

# 1. 拿一个 Octos 二进制：官方版，或自己编的魔改版
export OCTOS_BIN=/path/to/octos              # 不设则用 ../target/release/octos
cargo build --release -p octos-cli --no-default-features --features api   # 编魔改版时

# 2. 本机做题（约 5 分钟，不到一分钱）
python3 arc/run-task-local.py arc/tasks/smoke--counter --name try1
#    --port 43300 换端口可并行跑多题；--template <已有产物目录> 进入 evolution 模式
#    （先跑 smoke--counter，再以它的产物为模板跑 smoke-evolution--counter）

# 3. 用平台原版 Playwright 测试打分（首次会自动装 Playwright）
python3 arc/grade-local.py arc/arc-output/try1 smoke--counter
python3 arc/metrics.py arc/arc-output/try1        # 轮数 / Token / 费用 / 耗时 / 节点状态 / 打分，一行表格
python3 -m unittest discover -s arc/tests -t arc  # 编排器纯函数的单元测试

# 4. 改一处：main.py 的提示词 / 环境变量，octos_stdio.py 的启动参数，或 crates/ 里的内核
#    改完回到第 2、3 步，改前改后各跑一次，比数字

# 5. 打包上传
sh arc/pack.sh                               # 得到 octos-arc-bundle.zip
# 到 arc-bench.com 对应比赛页 New submission 上传，模型填 deepseek-v4-flash，
# Base URL 填 https://api.arc-bench.com/v1，然后选题、Run
```

## 改了内核怎么让平台用上

平台运行时按 `main.py` 里 `OCTOS_RELEASE_URL` 现场下载 Octos。改了 `crates/` 之后必须：
编译 Linux x86_64 版 → 在本仓库发一个 Release 挂上 tar.gz → 把 `OCTOS_RELEASE_URL` 改成那个地址 → 重新 `pack.sh` 上传。
否则平台跑的仍是官方版，改了等于没改。

## 目录

| 文件 | 作用 |
|---|---|
| `main.py` | 平台入口与编排器：骨架轮 → 按依赖序逐节点「设计 → 实现 → 本地跑该节点的验收 spec → 修复 ≤5 轮 → 通过即 commit」→ 启动演练 |
| `requirement_order.py` | 需求树拓扑排序、祖先查找、节点指纹（evolution 差异） |
| `acceptance.py` | 本地 Playwright 验收：spec↔节点映射、起服务、跑 spec、四字段失败摘要 |
| `guard.py` | 守护规则：未验证就宣称完成、连续同一错误、改保护路径 |
| `octos_stdio.py` | 通过 `octos serve --stdio` 驱动内核（默认每轮新 session） |
| `metrics.py` | 从事件流读 Token / 费用 / 耗时 / 节点状态 |
| `CHANGELOG.md` | 每条改动的改前改后数据 |
| `public-tests/<题目>/` | 平台公开的 Playwright 验收测试（会自动喂给模型） |
| `tasks/<题目>/` | 各题需求文件的离线副本 |
| `template/` | 平台初始工作区模板（frontend + backend + README.md + template.yaml）：pack.sh 打进 zip 根，平台铺成初始工作区（`ARCBENCH_TEMPLATE_DIR`） |
| `run-task-local.py` / `grade-local.py` / `pack.sh` | 本机做题、打分、打包 |

已知平台细节：容器里 `/workspace/tests` 有验收测试；订票题的测试默认连 3301 端口而平台起在 3000，`main.py` 会要求后端两个端口都监听；容器到 npmjs 很慢，提示词要求零依赖并走 npmmirror。

## 编排器开关（环境变量）

| 变量 | 默认 | 作用 |
|---|---|---|
| `OCTOS_TIME_BUDGET` / `OCTOS_NODE_TIME_BUDGET` | max(3600, 1500×节点数) / 1500 s | 整体与单节点（含修复轮）的墙钟预算；单节点预算按剩余时间/剩余节点数自适应 |
| `OCTOS_NODE_TIMEOUT` / `OCTOS_DESIGN_TIMEOUT` | 1200 / 420 s | 单轮上限 |
| `OCTOS_REPAIR_ROUNDS` / `OCTOS_MIN_REPAIR_SECONDS` | 5 / 300 | 每节点验收修复轮数 K；剩余不足 300 s 不再开修复轮 |
| `OCTOS_ARC_REGRESSION_CHECKPOINT` | 4 | 第 4、8、16、24…个节点后并行重跑此前通过的用例（后续间隔不超过配置值的两倍），把实际失败传给下一节点修复；0 关闭。末节点由全套验收覆盖，剩余不足修复时间时跳过 |
| `OCTOS_DESIGN_TURN` / `OCTOS_DESIGN_MODE` | 1 / separate | 0 = 跳过设计轮；`inline` = 设计 JSON 在实现轮开头写出，不单开一轮（TB 上更省钱但更慢，见 CHANGELOG R7/R8） |
| `OCTOS_SESSION_SCOPE` | turn | 新 session 的粒度：`turn`（每轮新，spec 已内嵌所以修复轮自足）、`node`（设计/实现/修复共用）、`run`（全程一个） |
| `OCTOS_DESIGN_MIN_NODES` / `OCTOS_IMPLEMENT_FRACTION` | 2 / 0.6 | 节点数不足时跳过设计轮；实现轮最多占节点预算的比例，留时间给修复轮 |
| `OCTOS_PERF_CONTRACT` / `OCTOS_GUARD` | 1 / 1 | 0 = 关闭性能规则 / 守护注入 |
| `OCTOS_ARC_ALIAS_SPEC_IDS` | 1 | 0 = 不把节点状态镜像到 spec 侧 id |
| `OCTOS_ARC_INSTALL_PLAYWRIGHT` | 1 | 0 = 找不到 Playwright 时不尝试安装 |
| `OCTOS_ARC_PLAYWRIGHT_ROOT` | 自动 | 指定含 `node_modules/@playwright/test` 的目录 |
| `OCTOS_ARC_FULLY_PARALLEL` | 0 | 1 = 本地验收让同一文件内的测试也并行（比平台更严的压力测试） |
| `OCTOS_ARC_TEST_TIMEOUT_MS` / `OCTOS_ARC_SLOW_MS` | 10000 / 3000 | 本地验收单测试超时；超过 SLOW 阈值即提醒模型 |

Web 大题（32–138 节点）的建议参数见 `CHANGELOG.md` 末尾「ARC-Bench Web 六题的建议参数」。

### 同题内按步骤选择模型

`OCTOS_ARC_MODEL_ROUTES` 接受有序 JSON 规则。同一题的不同节点、首轮与修复可以使用不同模型；匹配依据只有阶段、完整消息与工具定义的字符数、工具/图片能力，不按题名分支。第一条匹配的规则生效，无匹配则保留原请求模型；不配置时保持原行为。上下文字符数按完整消息与工具定义的紧凑 JSON 计算（Unicode 字符，不是 token）。

Python 和支持 `--model-routes-json` 的新版 Rust 内核使用同一规则格式；旧版 Rust 会明确报不支持参数，需换用新构建。Rust 的代码生成和工具请求共用本机转发层，实际模型记录在 `.arc/model-routes.jsonl`；分档开启后单模型费用估算为空，费用以 provider 账单为准。尚未完成云端对等验证，默认引擎仍为 Python。

云端提交也可携带同一规则：保存为任意 JSON 文件，然后运行 `sh arc/pack.sh /path/to/routes.json`，包内会增加 `model-routes.json`。打包和运行都会校验规则。环境变量 `OCTOS_ARC_MODEL_ROUTES` 优先于包内配置，显式设置为空可禁用分档；不传配置文件时，打包结果不包含任何默认模型规则。

本地验证真实 CLI 与内核请求链路（不调用付费 provider）：

```sh
cargo build --locked -p octos-cli --bin octos
python3 arc/integration/routed_cli.py target/debug/octos /tmp/octos-routing-evidence
```

脚本让真实工具模式完成设计，再切换到代码生成模型，模拟余额错误后检查整个运行失败且不再请求。该检查不证明模型生成质量或云端成绩。

下面仅是配置示例，不是经过费用/质量验证的默认值。模型 ID 来自 2026-09-15 ARC provider 的 `/v1/models`；该端点没有提供参数量和价格，也没有列出 `qwen3.8-27b`。应以使用时 provider 返回的目录和参数能力为准。

```json
[
  {
    "model": "qwen3.6-flash",
    "phases": ["implement"],
    "max_input_chars": 8000,
    "tools": false,
    "images": false,
    "parameters": {"max_tokens": 4096}
  },
  {
    "model": "qwen3.8-max",
    "phases": ["repair"],
    "tools": true,
    "images": false,
    "parameters": {"max_tokens": 8192}
  }
]
```

- 允许阶段：`implement`、`repair`、`verify`、`design`。修复含重写；阶段由流程设置。小模型失败后，现有验收/修复流程进入 `repair`，使用该阶段的匹配规则。
- `max_input_chars` 是字符预算，不是 token 上下文容量或模型能力的保证；用实际任务对比校准，给输出与协议开销留余量。
- `tools` / `images` 表示能力声明，默认 false；含工具历史也要求工具能力。声明应先验证，路由不会虚构 provider 能力。
- `parameters` 可覆盖 `temperature`、`top_p`、`max_tokens`、`max_completion_tokens`、`thinking`、`reasoning_effort`。切换模型会去掉原请求的供应商推理字段，再应用该规则的参数，避免跨模型照搬。
- 模型偏好由规则顺序明确表达；不根据未验证价格自动排序。不存在的模型或不支持的参数由 provider 报错，不偷偷更换并重复计费。
- 用量 JSONL 记录每次请求选中的 `model`、`phase`、耗时和 provider 返回的 token 用量。模型价格未提供时不编造成本。
- 当前仅 Python 引擎支持；配置路由并选择 Rust 会明确报错。Rust 路由与跨模型效果仍待实现/评测。多个模型请求仍遵守同 key 串行的费用计量约束。
