# octos-arc 重构报告：内核自带编排 + 极薄平台胶水

分支 `main-refactor`，基线 `2e408ee232547f1c28302971368bdf42347548a2`（origin/main 在开工后已前进到
`0b953d79`，本分支仍按要求从 `2e408ee2` 起）。

**状态：进行中。已完成删除与 K4，编排层卡在一个需要你拍板的决定上（见第 6 节）。**

---

## 0. 本轮进展（决定已拍板：走 A）

你选了 **A + 删东西**，并纠正了我对重试上限的误读。据此的最新状态：

- ✅ **A 已实现**：`crates/octos-pipeline/src/graph.rs` 的 `HandlerKind::from_str` 加上
  `"shell_check" => Some(Self::ShellCheck)`。只补一个 DOT 拼写，不新增能力——规则 23 本来就
  豁免 `ShellCheck`，它的命令由图作者写死、节点没有 LLM 工具面。
  新增回归测试 `should_parse_shell_check_handler_and_keep_it_off_the_no_shell_rule`；
  `cargo test -p octos-pipeline --lib parser::` → **43 passed**。
- ✅ **重试上限按你的纠正处理**：`max_retries` 本来就是 DOT 节点属性
  （`parser.rs:626`），DAG 路径 `dispatch_node` 会用它（`executor.rs:4774`），所以写
  `max_retries="5"` 即可，**`MAX_NODE_RUNS = 10` 是回边重跑整段区域的死循环保险丝，保持原样**。
  （一个需要注意的语义细节：`execute_with_retries` 只在 `OutcomeStatus::Error` 时重试；
  验收失败是 `Fail`，走的是回边修复轮——两者分工不同，不冲突。）
- ✅ **整条环已在真实内核上跑起来**（不是推演）：
  `[tool/started] run_pipeline {"pipeline": "arc_smoke"}` →
  `Pipeline 'arc_smoke' running: implement (1/3 nodes, ...)` →
  `implement [custom/qwen3.6:35b-a3b]: thinking (iteration 4)`。
  规则 1 / 规则 23 的报错都已消失，图通过校验并被 DAG 调度器执行。
- ✅ **think:false 找到了内核原生做法**（正是 K1，不用改内核）：配置里设
  `reasoning_effort = "none"`（`ReasoningEffort::Disabled` 序列化成 `"none"`）。
  端点实测：**3s / reasoning 0 token**，对比不设时 **22s**，约 7 倍。
- ⚠️ **一个必须如实说的问题**：pipeline 的 `codergen` 节点是**工具模式**（靠模型调
  `write_file`）。用 `qwen3.6:35b-a3b` 实测，该节点连续 8 轮只有 `thinking`，
  **一次 `write_file` 都没落地，`index.html` 始终没被写出来**（跑到 ~500s 仍未收敛）。
  而改前基线之所以 88s / 1 个请求就拿到 score=100，是因为它走的是
  **tiny-tier 单请求 codegen**：模型只吐 HTML，**由 Python 落盘，完全不用工具**。
  也就是说，换成 pipeline 编排会把「单请求出码」换成「工具模式出码」，对小模型是明显退步。
  平台上的 `deepseek-v4-flash` 工具能力更强，很可能没这个问题，但**我没有平台 key，
  无法验证**，所以这条先记为风险，不当成已解决。
  另外观察到：`run_pipeline` 是 `spawn_only`（后台执行、立即返回），模型会**重复调用第二次**，
  胶水层必须自己去重并等待后台完成。

---

## 1. 原始决定点（已解决，保留备查）

§4.2 要求「写一份 pipeline 定义……放到内核会扫描的 pipeline 目录」，并且 §7.3 要求 pack.sh 把
这份定义打进包。这条路的**结构**是对的，但内核当前有一个硬约束让它走不通：

- DOT pipeline 里 `handler="shell"` 被校验规则 23（`RuleId::NoShell`）**直接拒绝**
  （`crates/octos-pipeline/src/validate.rs:248`）。
- pipeline 里的 `codergen` 节点被**无条件**禁掉 `shell` / `bash` / `exec_command` / `exec` /
  `write_stdin`（`crates/octos-pipeline/src/handler.rs:712-731`，注释原文：
  “Shell is arbitrary code execution — banned in pipelines”）。
- 内核**确实有**一个合法的命令验收 handler —— `HandlerKind::ShellCheck`，规则 23 明确给它开了
  豁免（`validate.rs:245-247`）。但它**只能从 typed-IR 构造**
  （`crates/octos-pipeline/src/ir.rs:256`），`HandlerKind::from_str`
  （`crates/octos-pipeline/src/graph.rs:269-279`）里**没有 `shell_check` 这个拼写**，所以 DOT
  写不出来。
- `check` 工具不是测试运行器，是 `cargo check` / `tsc --noEmit` / `go vet` 的静态检查
  （`crates/octos-agent/src/tools/check.rs:529`），跑不了 Playwright。

也就是说「验收 = command validator」这件事，**今天在 DOT 里无法表达**。两条出路：

| | A. DOT + 一行内核改动 | B. typed-IR，零内核改动 |
|---|---|---|
| 怎么触发 | 模型只需说 `pipeline="arc_build"`（短名字，鲁棒） | 模型要原样吐出整段 IR JSON（节点数多时脆弱） |
| 定义放哪 | `<data_dir>/pipelines/arc_build.dot`，符合 §4.2/§7.3 | IR 是工具参数，不落在扫描目录，§4.2/§7.3 对不上 |
| 内核改动 | `graph.rs` 的 `from_str` 加一行 `"shell_check" => Some(Self::ShellCheck)` | 无 |
| 是否越线 | **越 §5 红线**（K1–K4 之外），所以我停下来问 | 不越线 |

A 只是给**已经存在**的 handler 补一个 DOT 拼写，不是新造机制，规则 23 本来就放行它；但它确实
落在 K1–K4 之外，§5 说这种改动要停下来写进报告、不要自己决定。所以我没有动手。

**我的建议是 A**：触发方式鲁棒得多（一个短名字 vs 一大段 JSON），定义能落进扫描目录因而满足
§4.2/§7.3，而且复用的是内核已有并已豁免的 handler。但这是你的决定。

---

## 2. 已经做完并验证过的

### 2.1 删除（−14,989 行）

| 删除项 | 行数 | 验证 |
|---|---|---|
| `crates/octos-arc/` 整个 crate | 14,670 | 目录已不存在 |
| `octos arc` 子命令（`commands/mod.rs` 的 enum / dispatch / re-export） | −36 | `octos --help` 已无 `arc` |
| `arc/rust_engine.py`（`OCTOS_ARC_ENGINE=rust` 入口） | 257 | 文件已删 |
| workspace 成员与依赖（根 `Cargo.toml`、`octos-cli/Cargo.toml`） | −3 | 构建通过 |

提交：`f07a57d7`。

### 2.2 K4：每轮可用工具由策略决定（已实现并实测）

这是**触发 pipeline 的前提**，原先根本做不到：

- `serve --stdio --solo` 会走 `enable_stdio_solo_lean_defaults()`
  （`commands/serve.rs:618`），强制加载精简 `coding` profile
  （`runtime/profile.rs:1580`）。
- 该 profile **不含** `run_pipeline`（`octos-agent/src/profile/mod.rs:719` 的排除列表），
  且 `apply_tool_envelope` 会再显式 `retain` 掉它（`runtime/profile.rs:790`）。
- 已有的 `OCTOS_STDIO_SOLO_TOOLS` 只能**收窄**，不能放宽（原注释：“can only ever narrow”）。

改动（`crates/octos-cli/src/runtime/profile.rs`，两处）：显式给出 allow-list 时，它就是该轮工具面的
**权威**，可以放宽到内建 coding 集之外；仍然只能在**已注册**的工具里选，写一个不存在的名字什么也
变不出来。

**实测证据**（改前 / 改后，同一个 spike、同一个模型）：

- 改前：模型 `tool_search("run_pipeline")` 只搜到 `bash`/`shell`/`exec_command`，回复
  “I do not have a `run_pipeline` tool available in my current toolkit.”
- 改后：`[tool/started] tool_name: "run_pipeline", arguments: {"pipeline": "arc_smoke", ...}`
  —— 工具在场，且**我放进 `<data_dir>/pipelines/arc_smoke.dot` 的自定义 pipeline 被按裸名发现并选中**。

### 2.3 「内核怎么发现 pipeline」——§4.2 要求先查清的结论

- 发现器：`PipelineDiscovery`（`crates/octos-pipeline/src/discovery.rs`）。
- `run_pipeline` 用的是 `PipelineDiscovery::new_operator_trusted(&data_dir)`
  （`tool.rs:167`），它**故意不含** `<working_dir>/.octos/pipelines`，防止模型自己丢一个 `.dot`
  再按名字跑。
- 因此有效搜索路径（先找到先赢）：
  1. `<data_dir>/pipelines/`
  2. `<data_dir>/skills/`（再往下一层扫 `skills/<name>/*.dot`）
  3. `<octos_home>/skills/`、`<octos_home>/pipelines/`（`with_octos_home`）
  4. `<octos_home>/bundled-pipelines/`（优先级最低，内置 `deep_research` 落在这里）
- serve 路径下 `RunPipelineTool::new(llm, memory, data_dir, data_dir)`
  （`runtime/profile.rs:1408`）——**working_dir 与 data_dir 都是 `--data-dir`**，
  所以 ARC 只要把 `.dot` 写进 `<data_dir>/pipelines/` 即可。
- 选中方式：`pipeline` 参数是个 **enum**，由 `discovery.list_available()` 实时生成
  （`tool.rs:~725`），所以装进去的 pipeline 会自动出现在候选里，按**裸名**（文件 stem）选中。
- 内联 DOT **已被拒绝**（“free-form DOT was the unsafe legacy contract and is now REJECTED”）。

### 2.4 新 DAG 执行路径的硬约束（已核对源码，与你的提示一致）

要拿到「失败重试 + 把失败原因交回节点」，必须 `OCTOS_PIPELINE_DAG=1`
（`executor.rs:5171-5177`）。并且图必须 `graph_is_dag_schedulable`（`executor.rs:5189-5219`）：

- 节点：不能是 `Parallel` / `DynamicParallel`，不能有 `converge`、不能有 `suggested_next`；
- 前向边：**不能带 `label`**，`weight` 必须是默认 1.0；
- 回边：必须带 `condition`，且 condition/label 里要含 `retry` / `back_edge` / `back-edge` /
  `guard_back` 之一（`validate.rs:1266-1272`），并且目标要么是 start 节点、要么另有前向前驱。

两个额外的坑（实测踩到）：

- `find_start_node`（`validate.rs:394`）**不排除回边**，所以回边指向首节点会让规则 1 报
  “no start node found”。规避办法是显式加一个 id 为 `start` 的节点——该函数对
  `nodes.contains_key("start")` 直接短路返回。
- 修复轮上限**不需要改内核**：`max_retries` 是 DOT 节点属性（`parser.rs:626`），DAG 路径
  `dispatch_node` 会用它（`executor.rs:4774`），写 `max_retries="5"` 即可。
  `MAX_NODE_RUNS = 10`（`executor.rs:4326`）是回边重跑整段区域的死循环保险丝，不是节点重试
  上限，保持原样。（此处修正了本报告早期版本的错误判断。）

死代码 `manager` / `server` / `thread` 三个模块按你的提示未使用。

### 2.5 K1 / K2：核对结果与 prompt 的前提不一致（未改内核）

- **K1**：prompt 说「现在只对 api.deepseek.com 发」。在基线 `2e408ee2` 上**已经不是这样**——
  该 commit 本身就是 `perf/reasoning-control-beyond-deepseek` 的合并。reasoning 控制现在按
  **模型名**决定（`octos-llm/src/openai.rs:119-165` 的 `ModelHints::detect` →
  `ReasoningStyle`），覆盖 deepseek-v4/reasoner、kimi-k3、glm-4.5+/z、grok-4/o 系/gpt-5，
  与 base_url 无关，且 `reasoning_style` 可按路由在配置里覆盖。
  **结论：K1 已经满足，不需要改内核**；对未知模型名想启用时，设置配置里的 `reasoning_effort` /
  `reasoning_style` 即可（opt-in：不设就不发）。
- **K2**：prompt 说「deepseek-v4 已是 32768，确认即可」。实际 `model_catalog.json` 里
  `deepseek/deepseek-v4-flash` 的 `max_output` 是 **384000**，远高于 16384；
  ARC 侧 `main.py:667` 又显式设了 `gateway.max_output_tokens = 65536`。
  **结论：K2 已满足**（数字比 prompt 写的更高）。llm_proxy 里 `ensure_max_tokens(..., 32768)`
  的注释针对的是旧版内核（arc.11 发 4096），在本基线上不再是同一个情况。
- **K3**：token/费用记账继续用内核事件。已确认事件在 stdio 流里就有，无需自造：
  `turn/completed` 带 `tokens_in` / `tokens_out` / `cache_hit`，
  `progress/updated` 带 `token_cost_update`（`input_tokens` / `output_tokens` /
  `session_cost` / `model`）。spike 日志里两者都实测到了。

---

## 3. 改前基线（真实跑分，非推算）

按 §2 要求，动手前先跑了一次 `arc/tasks/smoke--counter`。

平台 key 不在这台机器上（`ARCBENCH_API_KEY` 全盘未找到），所以用了你提供的自建
OpenAI 兼容端点。**这不是 arc-bench.com 的计费端点**，费用一栏需照此理解。

| 项 | 改前（baseline-before） |
|---|---|
| 提交 | `2e408ee2`（基线） |
| 题目 | `arc/tasks/smoke--counter` |
| 模型 / 端点 | `qwen3.6:35b-a3b` @ `http://office.liyao.space:40101/v1` |
| **打分（`grade-local.py`）** | **1/1 passed，score=100** |
| 节点状态 | `REQ-1: PASSED`、`ROOT: PASSED` |
| 通过率 / 功能率 | 1/1（100%）/ 1/1（100%） |
| 请求数 | 1（codegen 单请求路径） |
| Token | prompt 491 / completion 1815 |
| 费用 | 估算 $0.001163 —— **内核按价目表估的数，不是真实账单**（自建端点不计费）。<br>**模型确实被调用**的硬证据是非零 token 与 `turn/completed` 事件，不是这个数字。 |
| 耗时 | 88s（其中 Playwright 私有安装 46s） |

改后数据待编排层定下来后补齐；**在端到端跑通前不会填任何编造的数字**。

关于模型选择：先按你说的用了 `qwen3.6:35b-a3b`。后来收到 Osaka 端点后做了对照——短请求
（max_tokens=300）Osaka 看着更快（6.5s vs 25.2s），但那是因为它在 `reasoning` 里耗尽预算提前
`finish_reason: length`；换成真实 codegen 任务（max_tokens=3000）是 **152.7s vs 22.3s**，
`qwen3.6:35b-a3b` 快约 7 倍。所以基线与后续对照都用它，保证可比。

---

## 4. 还没做的（诚实清单）

以下都依赖第 1 节的决定，故未动手：

1. **`main.py` 缩成胶水（§4.3）**：目前仍是 3,811 行。
2. **删 `llm_proxy.py`（§4.4）**：K1/K2 已确认由内核满足、K3 事件已确认存在，删除本身不再有
   技术障碍，但要和新编排一起改，避免中途留下跑不通的仓库。
3. **删 `acceptance.py`（§4.5）**：取决于第 1 节选 A 还是 B。
4. **仪器移出提交包（§4.6）**、**pack.sh 显式清单更新（§7.3）**。
5. **提交包 Python ≤ 800 行（§7.3）**：当前 pack.sh 清单合计 **7,613 行**。
   已找到一笔确定的减量：`arc/arcbench_agent_runtime/`（1,208 行）与已安装的 pip 包
   `arcbench-runtime==0.1.0` **逐字节相同**（7 个文件 diff 全为 0），而 `requirements.txt`
   本来就声明了它，平台会装——所以这 1,208 行可以直接不打进包。
6. **改后端到端跑分**与改前并排对比。

---

## 5. 验收标准逐条自查（当前状态）

| # | 标准 | 状态 |
|---|---|---|
| 1 | `cargo build --release -p octos-cli --no-default-features --features api` | ✅ 通过（删 crate 后 4m41s，K4 后 2m41s） |
| 2 | 受影响 crate `cargo test` | ⏳ 未跑（编排层定稿后统一跑） |
| 3 | pack.sh 成功 / 包内容 / Python ≤ 800 行 | ❌ 未做（见第 4 节） |
| 4 | 仓库不再出现 `octos arc run` 与 `crates/octos-arc` | ⚠️ crate 与子命令已删；仅剩 `arc/arc-policy.toml` 与 `arc/main.py` 两处**注释**提到旧命令，随 §4.3 改写一并清掉 |
| 5 | 改前/改后各跑一次 smoke--counter | ⚠️ 改前已跑（score=100，见第 3 节）；改后待补 |

---

## 6. 仍需你拍板的事

1. ~~编排走 A 还是 B~~ —— **已定：A**，且已实现（见第 0 节）。
2. ~~每节点重试上限~~ —— **已澄清**：用 `max_retries="5"`，不动 `MAX_NODE_RUNS`。
3. **工具模式出码的收敛风险（新）**：见第 0 节最后一条。pipeline 的 codergen 节点靠模型调
   `write_file`，小模型实测不收敛；而现有 tiny-tier 是「单请求出码 + Python 落盘」。
   如果平台模型也出现同样情况，成绩会比改前差。可选缓解：给 `implement` 节点配
   `max_iterations` 与更强的提示词、或对小树保留单请求路径。**需要你决定是否接受这个风险**，
   以及能否给一次平台 key 做一次真实验证。
4. **改了内核怎么让平台用上**：`arc/README.md` 写明平台运行时按 `main.py` 里的
   `OCTOS_RELEASE_URL` 现场下载 Octos。K4 是内核改动，所以上传前必须：
   编 Linux x86_64 → 在本仓库发 Release 挂 tar.gz → 把 `OCTOS_RELEASE_URL` 指过去 → 重新
   `pack.sh`。**否则平台跑的仍是官方版，K4 不生效，pipeline 根本不会被触发。**
   这台机器是 macOS(arm64)，交叉编 Linux 产物这一步我做不了，需要你安排。

---

## 7. 平台上传步骤（arc-bench.com）

1. （改了内核时必做）编 Linux x86_64 版 Octos，在本仓库发一个 Release 挂上 tar.gz，
   把 `main.py` 里的 `OCTOS_RELEASE_URL` 改成该地址。
2. `sh arc/pack.sh` → 生成 `octos-arc-bundle.zip`（根目录须有 `main.py`、`requirements.txt`、
   `template/`）。
3. 打开 https://arc-bench.com 对应比赛页 → **New submission** → 上传该 zip。
4. 模型填 `deepseek-v4-flash`，Base URL 填 `https://api.arc-bench.com/v1`。
5. 选题 → **Run**。
6. 平台上的提交只能人工上传，这一步我做不了。

---

## 8. 红线自查

- **绝不预生成**：没有引入任何「模型没被调用也能出货」的路径。基线跑分的产物来自一次真实模型
  调用（491/1815 token，`turn/completed` 事件在案）。
- 未改写 git 历史、未 force push、未整树替换、未删仓库目录结构。
- `arc/public-tests/` 与 `arc/tasks/` 原样保留，且 pack.sh 本来就不打包它们。
- 未新增自造机制：K4 复用已有的 `OCTOS_STDIO_SOLO_TOOLS` 语义；缺的能力（DOT 的
  `shell_check` 拼写、可配置重试上限）写进第 6 节待办，没有自己动手。
- 报告里没有编造数字；未跑的地方明确写「未跑」。
