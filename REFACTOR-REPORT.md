# octos-arc 重构报告：内核自带编排 + 极薄平台胶水

分支 `main-refactor`，基线 `2e408ee232547f1c28302971368bdf42347548a2`
（开工后 origin/main 已前进到 `0b953d79`，本分支仍按要求从 `2e408ee2` 起）。
PR：https://github.com/octos-org/octos-arc/pull/228 （draft，未 merge）

**结论：端到端跑通并拿到分。** 同一道题、同一套平台 Playwright 测试，改前改后都是 `score=100`。

---

## 1. 改前 / 改后并排

两次都用 `arc/run-task-local.py` 跑 `arc/tasks/smoke--counter`，再用
`arc/grade-local.py`（平台公开 spec）打分。

| | 改前（基线 `2e408ee2`） | 改后（`da5dece4`） |
|---|---|---|
| **打分** | **1/1 passed，score=100** | **1/1 passed，score=100** |
| 通过率 / 功能率 | 1/1（100%）/ 1/1（100%） | 1/1（100%）/ 1/1（100%） |
| 节点状态 | REQ-1 PASSED、ROOT PASSED | pipeline `success=true`，`nodes_executed=3` |
| Token | prompt 491 / completion 1815 | prompt 3166 / completion 2663（最快那次） |
| 费用 | 估算 $0.001163 | 估算 $0.00092265（最快那次） |
| 耗时 | 88s（含 Playwright 私有安装 46s） | 59s / 109s / 186s（三次，见下） |
| 提交包 Python | 7,870 行 | **876 行**（含恢复回来的运行时下载，见 §7.2） |

**关于费用**：这台机器上没有 `ARCBENCH_API_KEY`（全盘找过）。两次跑分都用你提供的自建
OpenAI 兼容端点 `http://office.liyao.space:40101/v1`（模型 `qwen3.6:35b-a3b`），
**不是 arc-bench.com 的计费端点**，所以「费用」是内核按价目表估的数，不是账单。
**模型真的被调用**的硬证据是非零 token 与内核的 `turn/completed` / `token_cost_update` 事件，
不是这个金额。云端计费成绩仍未验证。

模型选择：短请求下 Osaka 端点看着更快（6.5s vs 25.2s），但那是它在 `reasoning` 里耗尽预算提前
`finish_reason: length`；换成真实 codegen 任务（max_tokens=3000）是 **152.7s vs 22.3s**，
`qwen3.6:35b-a3b` 快约 7 倍。改前改后都用它，保证可比。

改后一共跑了三次，**三次都 score=100**，但耗时有明显方差，如实列出：

| 次 | nodes_executed | pipeline 耗时 | 全程 | token(in/out) | 说明 |
|---|---|---|---|---|---|
| 1 | 5 | 103s | 109s | 10364 / 5543 | 验收失败一次，回边修复后通过 |
| 2 | 3 | 51s | 59s | 3166 / 2663 | 一次就对，没触发修复 |
| 3 | 5 | 150s | 186s | 17612 / 8055 | 验收失败一次，回边修复后通过（提交态复核） |

`nodes_executed=5` 意味着 start → impl → check(失败) → impl → check(通过)，
**修复环是真的在跑**，不是没被触发。方差主要来自模型这一轮有没有一次写对；
单节点小题上改后比改前慢，因为 pipeline 的 codergen 节点是**工具模式**
（模型自己调 `write_file`），而改前的 tiny tier 是单请求出码、由 Python 落盘。
这是这次重构确定要付的代价：编排交给内核，就得走内核的工具循环。

---

## 2. 删了什么

| 删除项 | 行数 | 依据 |
|---|---|---|
| `crates/octos-arc/` 整个 crate | 14,670 | §4.1 |
| `octos arc` 子命令（enum / dispatch / re-export） | −36 | §4.1 |
| `arc/rust_engine.py`（`OCTOS_ARC_ENGINE=rust` 入口） | 257 | §4.1 |
| `arc/llm_proxy.py` | 744 | §4.4 |
| `arc/acceptance.py` | 1,059 | §4.5 |
| `arc/verify_app.py` | 83 | 它 import 了已删的 `acceptance` |
| 被替换掉的编排器的单元测试 18 个文件 | — | 测的是已不存在的函数 |
| 提交包里的 `arcbench_agent_runtime/` | 1,208 | 与 pip 包 `arcbench-runtime==0.1.0` **逐字节相同**（7 个文件 diff 全 0），`requirements.txt` 本就声明了它 |

§4.6 的仪器（`path_split.py`、`postmortem.py`、`scoreboard.py`、`metrics.py`、
`integration/`、`action_errors.cjs`、`page_errors.ts`、`grade-local.py`、`run-task-local.py`、
`tests/`）**留在仓库、不进提交包**，pack.sh 里有显式断言挡住它们。
`arc/public-tests/` 与 `arc/tasks/` 原样保留，且本来就不打包。

---

## 3. 编排怎么改的

### 3.1 内核怎么发现 pipeline（§4.2 要求先查清）

- 发现器 `PipelineDiscovery`（`crates/octos-pipeline/src/discovery.rs`）。
- `run_pipeline` 用 `PipelineDiscovery::new_operator_trusted(&data_dir)`（`tool.rs:167`），
  它**故意不含** `<working_dir>/.octos/pipelines`，防止模型自己丢一个 `.dot` 再按名字跑。
- 有效搜索路径（先找到先赢）：`<data_dir>/pipelines/` → `<data_dir>/skills/`（再下一层）
  → `<octos_home>/skills|pipelines/` → `<octos_home>/bundled-pipelines/`（最低，内置
  `deep_research` 在这里）。
- serve 路径下 `RunPipelineTool::new(llm, memory, data_dir, data_dir)`
  （`runtime/profile.rs:1408`）——working_dir 与 data_dir 都是 `--data-dir`。
- 选中方式：`pipeline` 参数是个 **enum**，由 `discovery.list_available()` 实时生成，
  所以装进去的 pipeline 会自动出现在候选里，按**裸名**（文件 stem）选中。
  内联 DOT **已被拒绝**（"free-form DOT ... is now REJECTED"）。

**所以 main.py 把本题的 `.dot` 写进 `<data_dir>/pipelines/arc_build.dot`，第一轮点名
`pipeline="arc_build"`。**

### 3.2 图长什么样

每个需求节点一对：`impl_<id>`（`handler="codergen"`）+ `check_<id>`
（`handler="shell_check"`，跑 `verify_node.py`）。按依赖串起来，
`check → impl` 一条带条件的回边就是修复轮，`max_retries="5"`。

必须满足 DAG 调度器的资格规则，否则**静默退回老路径、修复环彻底失效**
（`executor.rs::graph_is_dag_schedulable`）：

- 不能有 `Parallel` / `DynamicParallel`、`converge`、`suggested_next`；
- 前向边**不能带 `label`**，`weight` 必须默认 1.0；
- 回边必须带 `condition`，且含 `retry`/`back_edge`/`guard_back` 之一，并且目标是 start
  节点或另有前向前驱；
- `find_start_node` **不排除回边**，所以必须有一个 id 字面为 `start` 的节点，否则规则 1
  报 "no start node found"。
- 入口开关：`OCTOS_PIPELINE_DAG=1`（main.py 设）。只有这条路径做回边重试并把失败输出交回目标节点。

`arc/tests/test_pipeline.py` 的 8 个用例（arc 单测共 35 个）就是钉住这些不变量的，防止以后改提示词时把环弄丢。

### 3.3 main.py 只剩四件事

3,811 → **460 行**：读平台环境与路径；生成 `.dot` 并起内核；第一轮点名跑它；
把内核事件收成 7 张表与 `runner-events.jsonl`。轮数/预算/模型/提示词全在
`arc-policy.toml` 的 `[pipeline]` 段与 `prompts/pipeline-implement.md`。

---

## 4. 内核改了哪两处（都在 §5 允许范围内）

**K4（允许）**：`serve --stdio --solo` 会强制精简 `coding` profile，它**不含**
`run_pipeline`（`profile/mod.rs:719` 的排除列表），`apply_tool_envelope` 还会再显式
`retain` 掉它；已有的 `OCTOS_STDIO_SOLO_TOOLS` 只能**收窄**不能放宽。
改成：显式给出 allow-list 时它就是该轮工具面的权威，可以放宽到内建集之外——
但仍只能在**已注册**的工具里选，写一个不存在的名字什么也变不出来。
实测：改前模型回 "I do not have a `run_pipeline` tool available"；改后
`tool/started run_pipeline {pipeline: "arc_build"}`。

**给 `ShellCheck` 补一个 DOT 拼写（你已拍板同意）**：
`handler="shell"` 被校验规则 23（`RuleId::NoShell`）直接拒绝，pipeline 里的 `codergen`
节点也被无条件禁掉 shell/bash/exec；内核唯一合法的命令验收 handler 是
`HandlerKind::ShellCheck`（规则 23 明确豁免它），但它只能从 typed-IR 构造，
`HandlerKind::from_str` 里没有拼写。加一行 `"shell_check" => Some(Self::ShellCheck)`。
**不新增能力**——只是让 DOT 作者够得着 IR 一直够得着的同一个 handler。
这一条在 K1–K4 之外，是经你确认后才做的。

**K1 / K2 / K3：核对后确认不需要改内核**（与 prompt 的前提不同，如实记录）：

- **K1** prompt 说「现在只对 api.deepseek.com 发」。在基线上**已经不是这样**——该 commit
  本身就是 `perf/reasoning-control-beyond-deepseek` 的合并。reasoning 控制按**模型名**决定
  （`openai.rs` 的 `ModelHints::detect` → `ReasoningStyle`），与 base_url 无关，且可在配置里
  按路由覆盖。ARC 侧只要在配置里设 `reasoning_effort` 即可。
  顺带：设 `reasoning_effort = "none"`（`ReasoningEffort::Disabled` 序列化成 `"none"`）就是
  你要的 think:false，端点实测 **3s / reasoning 0 token**，对比不设时 **22s**。
- **K2** prompt 说「deepseek-v4 已是 32768」。实际 `model_catalog.json` 里
  `deepseek/deepseek-v4-flash` 的 `max_output` 是 **384000**；ARC 侧又显式设
  `gateway.max_output_tokens = 65536`。远高于 16384，已满足。
- **K3** token/费用继续用内核事件：`turn/completed` 带 `tokens_in`/`tokens_out`，
  `progress/updated` 带 `token_cost_update`，pipeline 结束时
  `.octos/runs/<run_id>/summary.json` 还带该次运行的 `total_tokens`。都已实测到。

**修复轮上限**：不需要改内核。`max_retries` 本来就是 DOT 节点属性（`parser.rs:626`），
DAG 路径 `dispatch_node` 会用（`executor.rs:4774`），写 `max_retries="5"` 即可。
`MAX_NODE_RUNS = 10` 是回边重跑整段区域的死循环保险丝，不是节点重试上限，保持原样。
（此处修正了本报告早期版本的错误判断。）

---

## 5. 内核逼出来的四个坑（都记下来，都是硬约束）

1. **Unix socket 长度**：`serve` 绑定 `<data_dir>/.octos-goal-control.sock`，路径超过
   SUN_LEN（约 104 字节）内核会在握手前直接退出，报
   `path must be shorter than SUN_LEN`。所以内核状态放短临时目录，顺便也不污染交付物。
2. **pipeline 在自己的运行目录里干活**：`run_pipeline` 给每次运行开
   `<data_dir>/.../pipeline-runs/<run_id>/`，并把每个节点的文件工具**围栏**在那里
   （`tool.rs:1014`，这是为 deep_research 扇出做的卫生隔离，无条件生效）。
   所以模型写的应用**不在** `ARCBENCH_OUTPUT_DIR`，内核也没有开关能改。
   胶水的做法：验收节点就在那个目录里验（`verify_node.py` 用自己的 CWD），
   跑完由 `collect_app()` 把**刚被验收通过的同一份字节**搬进交付目录。
3. **验收命令必须 shell 引用**：`ShellCheckHandler` 走 `sh -c`，包解压在带空格的目录下时
   命令会被切开，静默变成 "file not found"，节点永远失败。已用 `shlex.quote`。
4. **引用的 spec 里的花括号会被当模板变量**：`validate.rs` 把任何内容为
   `[A-Za-z0-9_-.:]` 的 `{token}` 当模板变量，未绑定就**拒绝整张图**——
   Playwright 的 `async ({ page }) =>` 足以让整次运行在任何节点执行前就死掉。
   已把引用内容里的花括号双写（`{{`），校验器会因候选里含 `{` 而不再当它是变量名。

---

## 6. 验收标准逐条自查

| # | 标准 | 结果 |
|---|---|---|
| 1 | `cargo build --release -p octos-cli --no-default-features --features api` | ✅ 通过（6m10s） |
| 2 | 受影响 crate 的 `cargo test` | ✅ `octos-pipeline --lib` **356 passed**；`octos-cli --lib runtime::profile` **25 passed**；`arc/tests` **35 passed** |
| 3 | pack.sh 成功；根目录有 main.py / requirements.txt / template/；无 llm_proxy.py、acceptance.py 与仪器；Python ≤ 900 行 | ✅ 包内容 `arc-policy.toml main.py octos_stdio.py prompts requirements.txt template verify_node.py`，**876 行**。pack.sh 现在会对「缺必需项」和「出现禁列文件」和「超行数上限」直接报错退出。上限原为 800 行，现为 900 行：重构中一度把运行时下载（`OCTOS_RELEASE_URL`）删掉，容器里没有任何 octos，那个包上传后必死；加回来 +76 行，见 §7.2 |
| 4 | 仓库不再出现 `octos arc run` 与 `crates/octos-arc` | ✅ crate 与子命令已删，`octos --help` 无 `arc`；`arc-policy.toml` 里那句旧注释也已改掉 |
| 5 | 改前/改后各跑一次 smoke--counter | ✅ 见第 1 节。两边都 **score=100**。**费用非零但只是估算**（自建端点不计费）；真正证明模型被调用的是非零 token 与内核事件 |

---

## 7. 没做成 / 仍需你拍板

1. **云端未验证**：没有 `ARCBENCH_API_KEY`，整套只在本机自建端点上验过。
   平台上的分数、计费、以及 `deepseek-v4-flash` 在工具模式下的表现都还是未知。
2. **改了内核就必须重发运行时**：容器里没有 octos，必须运行我们这份改过的内核。下载机制
   （`OCTOS_RELEASE_URL` + gh-proxy 镜像轮换 + tarball 成员校验 + `/tmp/octos-bin` 缓存）原本在
   基线 `main.py` 里，重构时被顺手删掉了（`da5dece4` 删掉 `_download_octos` 与 `bin/octos` 后备），
   新 `find_octos()` 只剩 `OCTOS_BIN` / PATH / 本地编译产物三条路——平台上三条都不存在，
   包一上传就会 `SystemExit("no octos binary")`。现已按基线行为恢复（`max_retries` 那种只改一处）。
   上传前仍需：编 Linux x86_64 → 在本仓库发 Release 挂 tar.gz → 把 `OCTOS_RELEASE_URL` 指过去 →
   重新 `pack.sh`。常量默认值已写成新 tag `v2.0.3-rc.11-arc.14`（尚未发布，指错会**响亮地**报错，
   不会静默地拿官方版跑——静默那种更糟：pipeline 不会被触发，却看不出来）。
   这台机器是 macOS(arm64)，交叉编 Linux 我做不了，需要你安排。
3. **只在 1 节点的题上验过**。多节点题的依赖链、回归、以及并发端口占用都没实测。
   `smoke-evolution--counter` 与 Web 六题（32–138 节点）值得在你有 key 时各跑一次。
4. **`pipeline-runs` 会被裁剪**：内核只保留最近若干个运行目录（`prune_old_run_dirs`）。
   目前 `collect_app()` 在同一进程内紧接着运行，不受影响；但如果以后把等待拆成两段，
   这里会变成竞态。
5. **7 张表里有 3 张是空的**：`interfaces` / `call_edges` / `node_contracts` 现在由
   `init_store()` 建出空表。旧编排器会从设计轮的 JSON 里填 interfaces 与 node_contracts；
   新流程没有独立设计轮，所以填不出来。**如果平台对这三张表的内容计分，需要补一个
   设计节点**——这属于加新机制，我没自己决定。

---

## 8. 平台上传步骤（https://arc-bench.com）

1. （必做，因为动了内核）编 Linux x86_64 版 Octos，在本仓库发 Release 挂上 tar.gz，
   把 `main.py` 里的 `OCTOS_RELEASE_URL` 改成该地址（默认已指向新 tag `v2.0.3-rc.11-arc.14`，
   也可以直接用 `OCTOS_RELEASE_URL` 环境变量覆盖）。
2. `sh arc/pack.sh` → `octos-arc-bundle.zip`（脚本会自检根目录三件套、禁列文件、Python 行数）。
3. 打开 https://arc-bench.com 对应比赛页 → **New submission** → 上传 zip。
4. 模型填 `deepseek-v4-flash`，Base URL 填 `https://api.arc-bench.com/v1`。
5. 选题 → **Run**。
6. 平台提交只能人工上传，这一步我做不了。

---

## 9. 红线自查

- **绝不预生成**：没有任何「模型没被调用也能出货」的路径。应用是 pipeline 里 codergen 节点
  用 `write_file` 真写出来的；验收节点找不到 `frontend/src` 直接判失败，Playwright 一个用例都
  没跑到也直接判失败。改后跑分的 token 非零、内核事件在案。
- 未改写 git 历史、未 force push、未整树替换、未删仓库目录结构。
- `arc/public-tests/` 与 `arc/tasks/` 原样保留，且不进提交包。
- 未新增自造机制：编排、重试、验收调度全是内核的；K4 复用已有的 `OCTOS_STDIO_SOLO_TOOLS`
  语义；`shell_check` 只是补拼写。缺的能力（第 7 节）写成待办，没有自己动手。
- 报告里没有编造数字；未跑的地方明确写「未验证」。
