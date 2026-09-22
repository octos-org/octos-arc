# ARC-Bench 内核优化记录

当前交付基线：`origin/main`（第一轮运行记录保留原始 `arc-opt` 证据）。所有数字均来自本机事件流、测试输出或 GitHub Actions 产物；评测分数与单元测试结果分开记录。

## 结论摘要

| 项目 | 改前 | 改后 | 证据 |
|---|---:|---:|---|
| stdio/solo `Reply OK` 输入 Token | 17,205 | 5,714 | `turn/completed`，`/private/tmp/arc-stdio-probe-events.jsonl` |
| stdio/solo 模型可见工具数 | 62 | 12 | serve 日志 `tool_count` |
| Counter 轮数 | 3 | 3 | 两个 `.arc/octos-events.jsonl` |
| Counter 输入 Token | 64,658 | 32,423 | 两个 `.arc/octos-events.jsonl`；改后使用最终 arm64 二进制的 `cmp-arc-opt-final2` |
| Counter 输出 Token | 20,337 | 16,558 | 两个 `.arc/octos-events.jsonl` |
| Counter 费用 | 0.01474648 | 0.09336362 | 两个 `.arc/octos-events.jsonl` 的 `token_cost_update` |
| Counter 耗时 | 667 秒 | 369 秒 | 两个 `.arc/runner-events.jsonl` 的 running/completed 时间 |
| Counter Playwright | 1/1 | 1/1 | 修正 `baseURL` 后的公开测试 |
| Ticket Booking 轮数 | 0 | 4（最终二进制） | 官方初次运行无完成回合；当前 arc-opt 运行含骨架、2 节点和最终检查 |
| Ticket Booking 输入 / 输出 Token | 0 / 0 | 243,149 / 126,310 | 两次 `.arc/octos-events.jsonl`；当前 arc-opt 使用 `ticket-arc-opt-final` |
| Ticket Booking 费用 | 无记录 | 0.99110494 | `token_cost_update` |
| Ticket Booking 耗时 | >10 分钟后中断 | 1,878 秒 | `runner-events.jsonl` 起止时间 |
| Ticket Booking Playwright | 未执行 | 10/10 | `grade-local.py` 公开测试 |

## 第二轮：工作流 B（P0-0、B1、B2、B3、B5）

本轮从 `origin/main` 的 `82e3bef3` 新建 `wf-kernel`，没有改动 `legacy`，也没有启用或复用上游发布工作流。

### P0-0：stdio/solo 默认 coding 工具面

该项已由第一轮合入的 `82e3bef3` 继承并复核：`serve --stdio --solo` 默认使用 coding 工具白名单，跳过 bundled app-skills/platform-skills；无 memory/goal 时不注入对应 snapshot，panes 树不进入模型上下文。实测为 5,714 input tokens、12 个工具，达到不超过 6,000 的目标。

### B1：Linux x86_64 手动发布

改动位置：`.github/workflows/arc-linux-release.yml`。

保留唯一的 ARC 专用 `workflow_dispatch` 工作流，不启用上游继承工作流。它按输入的精确 ref 构建 `x86_64-unknown-linux-gnu` runtime 和 bundled tools，生成 `octos-bundle-x86_64-unknown-linux-gnu.tar.gz`，并在 Release 中上传 bundle SHA-256、`octos` 二进制 SHA-256、源码提交、rustc 和版本信息。最终成功运行是 [34746031597](https://github.com/octos-org/octos-arc/actions/runs/34746031597)，创建了 [v2.0.3-rc.11-arc.10](https://github.com/octos-org/octos-arc/releases/tag/v2.0.3-rc.11-arc.10)。源码提交为 `ca337e4ff409144fa8e4469b4057371f4bca1d3f`，rustc 为 `1.98.0 (88d9e12ae 2026-08-18)`；bundle SHA-256 为 `68e10832f382a3f9b6cba3f142666b9ba3ffae30b072fa9808557cdbeda9027f`，`octos` SHA-256 为 `5e7a1b8b2cd3db40f0db290932f3b2e9909b4dd5c1adea29057873fc288b7753`。下载归档后用发布的 `bundle.sha256` 与 `octos.sha256` 均核验通过，并确认归档内 `octos` 是 Linux x86-64 ELF；`arc-runtime-lock.json.runtime_release` 已回填上述真实值。

### B2：DeepSeek 费用异常

改动位置：`crates/octos-llm/src/openai.rs`、`pricing.rs`、`crates/octos-core/src/ui_protocol.rs`、`crates/octos-cli/src/api/ui_protocol_transport.rs`。

OpenAI 兼容响应现在解析 DeepSeek 顶层 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`，并将其归一化为不重复计入的 input/cache-read 口径；DeepSeek cache hit 使用 0.1 的输入价格折扣。`turn/completed` 同时暴露 `cache_hit`，便于将计费数据与请求数据逐轮核对。新增 8 个 `octos-llm` cache/pricing 测试及 CLI 完成事件回归测试；尚未再次调用付费 ARC 任务，因此本轮没有声称线上费用下降，实际评测应标记为“未评测”。

本轮本地 Counter 运行 `wf-kernel-counter`（事件流：`/Users/mac/Desktop/octos-official-demo/arc-output/wf-kernel-counter/.arc/octos-events.jsonl`）为 3 轮、37,691 input、22,556 output、658,176 cache-hit tokens，累计费用 0.10373706；`grade-local.py` 公开测试为 1/1。费用异常修复的 provider 真实线上计费仍未评测，以上是新内核本地回归的可复核数据。

### B3：每轮与每节点预算

改动位置：`crates/octos-cli/src/config.rs`、`commands/gateway/gateway_runtime.rs`、`runtime/session.rs`；节点执行逻辑沿用 `crates/octos-arc/src/runner.rs` 的 `--node-budget-seconds` 和 `--node-token-budget`。

serve/stdio 会话新增可选 `[gateway].token_budget`，映射到 agent 的整轮总 Token 上限（包含缓存 Token）；`session_timeout_secs` 提供整轮时间上限。`octos arc` 仍按依赖拓扑逐节点设置 Token/时间预算，节点耗尽时记录 `skipped_budget`，保留此前完成的节点。Agent 的超限返回是失败结果，包含 `budget_exhausted`，不会被当成成功回合继续空转。配置解析和 session bootstrap 回归测试已通过。

### B5：容器实测

改动位置：`.github/workflows/arc-linux-release.yml`、`scripts/container-stdio-smoke.py`。

GitHub Actions Ubuntu runner 的最终运行 `34746031597` 通过了真实容器 smoke：使用 `python:3.13-slim-trixie`、`--security-opt=no-new-privileges`、`--cap-drop=ALL` 和 `--pids-limit=128`，只挂载 `octos` 二进制而不挂载 `octos-sandbox` helper。发布的 `arc-b5-container-smoke` artifact 报告 `container_marker=true`、`helper_absent=true`、`fixture_requests=2`、`turn_ok=true`、`reply=OK`、shell marker 为 `b5-sandbox-exec`、`sandbox_log=true`；滚动日志明确记录 `no sandbox backend found`、`shell commands run WITHOUT isolation` 和 `in_container=true`。这同时验证了 `serve --stdio --solo` 在无后端容器中的自动降级路径，而不是把普通容器执行结果当作沙箱隔离。

## P0-0：stdio/solo 提示与工具面瘦身

改动位置：`crates/octos-cli/src/commands/serve.rs`、`runtime/profile.rs`、`api/ui_protocol_transport.rs`。

`serve --stdio --solo` 启动时默认启用 coding profile，严格保留 12 个编码工具；bundled app-skills/platform-skills 不再自动 bootstrap。空 memory 不生成策略段或 `memory-snapshot`，无 active goal 不生成 `session-goal-snapshot`。panes 树仍只属于 `session/open` 的协议返回，不会追加到模型历史。

验证：CLI 单测 `p0_0_tests`、profile 白名单单测；真实 arm64 二进制用同一 API 配置和同一句 `Reply OK` 运行，得到 5,714 输入 Token、12 工具、成功回复 `OK`。

## P0-1：容器沙箱显式降级

改动位置：`crates/octos-agent/src/sandbox/mod.rs`。

启动时检测 `/.dockerenv` 及 docker/containerd/kubepods/podman/libpod cgroup 标记；容器中无可用隔离后端时记录明确 warning 并按容器策略降级，且不会把仅检测到的 Docker CLI 当成可用的嵌套隔离。探测函数保持纯函数便于测试。新增 dockerenv、cgroup、误判排除和 nested-Docker 降级测试；`sandbox::tests` 44/44 通过。

GitHub Actions Ubuntu runner 已完成上述验证（运行 `34746031597`，对应 artifact `arc-b5-container-smoke`）：容器内 `/.dockerenv` 存在，helper 缺失，fixture 驱动的 shell 命令成功执行，且 data-dir rolling log 捕获了明确的自动降级 warning。该结果覆盖了 P0-1 要求的容器事实与降级证据；本机仍未安装 Docker，因此没有把本机 Docker 当作验证来源。

## P0-2：上下文与 Token 控制

改动位置：`crates/octos-agent/src/compaction_tiered.rs`。

默认每条工具结果最多 8 KiB；完整 compaction pass 对超限结果保留头尾并加入截断标记，旧结果的 oversized-only pass 则替换为结构化一行摘要。过期、重复或不在工作集的结果仍折叠为结构化一行摘要，当前工作集文件读取保持可重读性。新增头尾保留、工作集保护和历史折叠测试。

Counter 事件流的输入 Token 从 64,658 降到 32,423；这是同一平台、最终 arm64 二进制一次成功运行的真实结果，不把它解释成仅由单个改动独立贡献的因果实验。该二进制的第一次 Counter 重跑因生成应用把 `data-testid` 与 JavaScript 的 `id` 查找混用而被公开测试判为 0/1；未修改生成应用，第二次运行 `cmp-arc-opt-final2` 由公开 grader 判定为 1/1。

Counter 的 arc-opt 费用高于官方这次记录（0.09336362 对 0.01474648）；费用字段按事件流原样保留，不能据此推断成本优化。Ticket Booking 的前两次 arc-opt 尝试在骨架首轮中断，当前最终二进制的 `ticket-arc-opt-final` 运行完成，公开测试为 10/10；官方 `grade-local.py` 原始脚本的 Playwright 配置缺少 `baseURL`，本地对照时仅临时补入 `baseURL: process.env.E2E_BASE_URL` 后执行公开测试，随后恢复了脚本。

## P0-3：推理模型空回合恢复

改动位置：`crates/octos-llm/src/context.rs`、`openai.rs`、`crates/octos-agent/src/agent/{detection,llm_call}.rs`。

DeepSeek V4 默认输出上限提高到 provider 级安全值；当 `finish_reason=length` 且无正文、无工具调用时，只重试一次，按配置降低 reasoning effort 或提高 max tokens；再次失败返回明确错误，不计为成功。新增判定与恢复策略测试。

## P1-4：流式失败自动回退

改动位置：`crates/octos-agent/src/agent/{detection,llm_call,mod}.rs`。

识别 SSE/streaming 不支持错误后，同一 session 记录 provider/model，后续请求直接走非流式路径；当前错误回合也自动回退一次。新增错误识别测试。Counter 运行期间平台请求成功，未再依赖适配层的 `OCTOS_DISABLE_STREAMING=1`。
即使当前 LLM 调用处于 FailFast 策略，明确的 SSE 不支持错误仍保留这一次非流式回退；其他传输错误继续按 FailFast 直接返回。该边界由单测覆盖。
provider/model 的 session key 使用不可歧义分隔符，避免 provider 或 model 名称含冒号时错误共享回退状态。

## P1-5：环境事实前置注入

改动位置：`crates/octos-arc/src/runner.rs`。

`octos arc` 每次 session 仅探测一次 node/npm/python、cwd、npm registry 连通性、容器标记和 sandbox 状态，并将短行事实加入稳定基础提示；生产路径同时对 cwd 和整段事实做字符上限保护，保持在 200 Token 量级以内。
单测用隔离的 node/npm/python3 fixture 验证版本、registry 连通性、容器标记和 200 Token 上限。

## P1-6：Playwright 验收 hook

改动位置：`crates/octos-arc/src/runner.rs`。

轮次结束的本地验证会发现显式目录或工作区内的 `*.spec.ts`，从项目及 spec 目录祖先查找 Playwright；存在时运行 `playwright test --reporter=line`，stdout/stderr 的失败断言以 `[hook]` 错误回传。没有 spec 或 Playwright 时记录 skipped 并继续。目录和基址可由 CLI 参数或 `OCTOS_ARC_SPEC_DIR`、`OCTOS_ARC_BASE_URL` 指定。单测用临时 spec 和 fake runner 验证了实际执行、基址注入及 passed/failed 证据记录。

## P2-7：按节点预算

改动位置：`crates/octos-arc/src/runner.rs`。

新增 `--node-budget-seconds`（默认 300）和 `--node-token-budget`（默认 20,000）。每次运行按依赖拓扑逐节点启动 coding turn：进程 deadline 绑定到当前节点，配置中的输出上限绑定到节点 Token 预算；超时或回合失败时记录 `skipped_budget`，继续后续节点并保留已完成部分。预算计划写入 `node-budgets.json`、报告并前置到 coding prompt。依赖排序和预算边界有单测。

## 构建与测试

构建命令：

```text
cargo build --locked -p octos-cli --no-default-features --features api
```

产物为 macOS arm64 Mach-O。`cargo fmt --all -- --check`、完整工作区 `cargo clippy --locked --all-targets -- -D warnings` 和完整工作区 `cargo test --locked` 均通过。当前相关 crate 的测试结果为：`octos-agent` 2,900 passed / 3 ignored，`octos-arc` 23 passed / 1 ignored，`octos-llm` 687 passed / 3 ignored，`octos-pipeline` 355 passed / 1 ignored，`octos-cli` 单元测试 3,680 passed / 10 ignored；工作区测试命令最终退出码为 0。此前本机无 Docker 时出现的 3 个环境相关失败已改为按 Docker 可用性断言，当前不再失败。

最终产物：`octos 2.0.3-rc.11 (a10521e4 2026-09-12)`；SHA-256 为 `4f7ec9437ec86aac0fad663fc5df94e395b537dad50138b5828732cc00e8cb09`，对应 `aarch64-apple-darwin` 和 Homebrew `rustc 1.98.0`。

`runtime_release` 已填入并核验 v2.0.3-rc.11-arc.10 的真实 Linux 产物；`build` 继续记录真实 macOS arm64 产物元数据。尚未创建新的 ARC 云端评测运行，因此线上 Counter 费用/分数仍应标记为“未评测”。

## 独立提交索引

每条优化都有至少一个独立的实现或回归提交，提交信息包含现象和改法：

| 优化 | 提交 |
|---|---|
| P0-0 | `840007d2` |
| P0-1 | `a422b473` |
| P0-2 | `f8cb7efb` |
| P0-3 | `7ccdab08` |
| P1-4 | `3a662235` |
| P1-5 | `9011052c` |
| P1-6 | `37d38bae` |
| P2-7 | `dc0f6be0` |

第二轮提交：B1 `e3bc8242` + `d12c15ad`；B2 `5e6ca223`；B3 `d9fba010`；B5 workflow/driver 修正经 PR #14、#15、#16、#17、#18、#21、#22、#23、#24 合入；最终 B5/Release 运行 `34746031597`，Release 提交为 `ca337e4f`。

## 第三阶段：工作流 B · 真实 agent 成本与缓存

本节对应目标书 2026-09-13 新增的第 7 节。实现已由 PR #31 squash 合入 main（`1de981aebe1915b8aeb9a6645b0fe91a63fb4d0a`）。本阶段没有修改 `legacy`，也没有把本地生成应用当作评测结果。

### 改动

- `crates/octos-cli/src/runtime/profile.rs`：stdio/solo 默认复用 coding 的 12 工具白名单，跳过 bundled app/platform skills；默认系统前缀使用紧凑 worker 指令，显式 system prompt 仍优先。
- `crates/octos-cli/src/runtime/session.rs`：DeepSeek ARC 会话默认 `reasoning_effort=low`，单次 completion 上限为 4,096；显式 profile/model/gateway 配置仍覆盖默认值。
- `crates/octos-llm/src/openai.rs`：ARC-Bench 的 OpenAI 兼容端点保留 DeepSeek V4 的 `reasoning_effort`/`thinking` 字段；其他未知自定义端点继续要求显式 model hints。
- `crates/octos-cli/src/api/context_manager.rs`：模型可见的单条工具输出默认上限为 8 KiB，保留头尾。AppUI 旧观测折叠的默认 rollout 已为 On，继续采用批量语义折叠。

### 真实本机对照

| 任务/请求 | 改前事件流 | 改后事件流 | input tokens | output tokens | cache_hit | 公开测试 |
|---|---|---|---:|---:|---:|---:|
| Counter（旧 8 KiB/8,192 上限） | `arc/arc-output/kernel-v2-baseline-counter/.arc/octos-events.jsonl` | `arc/arc-output/kernel-v2-final2-counter/.arc/octos-events.jsonl` | 20,627 → 21,614 | 8,300 → 6,950 | 235,520 → 186,624 | runner 通过；公开 grader 未重复 |
| Counter 首轮（同一流程） | 同上 | 同上首个 `turn/completed` | 20,627 → 15,674 | 8,300 → 4,262（−48.7%） | 235,520 → 110,336 | 工作流完成，测试结果以完整 runner 产物为准 |
| `Reply exactly OK.` | 官方证据 17,205、62 工具 | 新二进制直接 stdio/solo | 17,205 → 5,420 | — → 2 | — → 0（单轮） | OK |
| 同一 serve 会话第二次 `Reply exactly OK.` | — | 临时协议驱动器事件 | — | 2 | 5,120/5,420（约 94.5%） | OK |

同一 serve 会话的第二次请求采用 append-only 前缀，`turn/completed.cache_hit` 为 5,120；整段 prompt hash 随新增 transcript 改变，因此不能把整段 hash 当作“完全相同”，缓存命中字段才是本项证据。短请求的新二进制实测输入已低于 6,000；Counter 的全流程总量受模型是否额外发起需求回合影响，不能把首轮降幅误报为整题降幅。

Ticket Booking 的 8,192 上限版本曾运行到第二需求节点并被本地工具长尾中止，事件流保留在 `arc/arc-output/kernel-v2-lean-ticket/.arc/octos-events.jsonl`，不作为完成对照。4,096 上限版本已完成 6 个回合：`tokens_in=183,600`、`tokens_out=51,140`、`cache_hit=2,694,144`、费用 `0.13794452`，耗时 911 秒；对照基线为 7 个回合、`321,933/162,988`、`cache_hit=4,435,712`、费用 `0.25352586`、耗时 2,319 秒。官方公开 Playwright spec 重新执行为 10/10（使用 `grade-local.py` 的原始 spec，grader 进程清理采用等价的直接 runner 以避开 macOS 进程组权限错误）。

以上单元测试和本机 agent 运行均不等于 ARC-Bench 云端评测，云端成绩仍为“未评测”。

### 验证

已通过：

- `cargo fmt --all -- --check`；
- `openai::tests::deepseek_v4_thinking_style_downgraded_off_official_endpoint`；
- `runtime::profile::tests::stdio_lean_prompt_uses_compact_worker_instructions_but_honors_override`；
- `context_manager::tests::default_tool_output_policy_keeps_eight_kibibytes_for_model`；
- `cargo build --locked --release -p octos-cli --no-default-features --features api`。

本地 macOS arm64 产物为 `octos 2.0.3-rc.11 (08e10c05 2026-09-13)`，SHA-256：`a856afaf88c1967ad95ec2268259665085aa9a9ef6adb27aef0c29a6558488b1`；rustc 为 `1.98.0 (88d9e12ae 2026-08-18)`。对应 main 的 Linux Release 已由 workflow run `34752867215` 发布为 [v2.0.3-rc.11-arc.11](https://github.com/octos-org/octos-arc/releases/tag/v2.0.3-rc.11-arc.11)，源码提交为 `1de981aebe1915b8aeb9a6645b0fe91a63fb4d0a`，归档 SHA-256 为 `ec7fb4c1c9a4885d4a00d9e4bcd3e0c382779bf449ee0aa20c0bfc4062e59ab8`，解包 Linux `octos` SHA-256 为 `05f41672f0b5fef8c3936aa56b72f1add8525961a928668abb34849d4595124e`；下载地址已同步到 `arc-runtime-lock.json` 和 `arc/main.py`。

Release 后应由 C 使用新适配包重跑 Smoke Counter 与 Smoke Dice，并在日志确认 `2.0.3-rc.11`；本环境未提供跨会话 `send_to_session` 接口，因此对 C（`a63a5b17-2553-4c9d-9647-a5ae3b7d852c`）和 A（`04b003e2-ad06-4aa8-b56c-13c29a0d80c2`）的通知尚未实际发送，不能宣称云端已重跑。云端榜单成绩仍为“未评测”。

## 第四阶段：harness 收编（工作流 D，分支 `wf-kernel-harness`）

目标书：`GOAL-arc-rust-harness.md`（2026-09-14）。现状是策略层在 `arc/*.py`（4,386 行、约 60 个环境变量），Rust 的 `octos arc create/evolve` 与之重复且不在比赛路径上。本阶段把策略收进内核的 `octos arc run`，Python 只剩平台胶水；所有新路径放在 `OCTOS_ARC_ENGINE=rust` 开关后面，默认仍走 Python，直到第 4 节的对等验证全部通过。

### 现有 Python 策略 → Rust 模块 逐条对照表（草稿，随里程碑更新）

「来源」列是 `arc/` 里的唯一权威实现；「为什么存在」列引用 `arc/CHANGELOG.md` 的归因（云端运行编号）；「里程碑」列标注该条在哪个 PR 收编。M1 只做 codegen 闭环，tool 模式（stdio 驱动）在 M2。

#### tree.rs（← requirement_order.py、main.py 的树处理）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| T1 | 读 `requirements.yaml`/`.yml`，剥 `root`/`requirement` 包装，缺 `id` 即报错 | `main.load_requirement_tree` | 平台格式 | `tree::load` | M1 |
| T2 | ATOMIC 平铺：`type=ATOMIC` 或无子节点且非 FOLDER 的节点按文档序 | `requirement_order.flatten_atomic` | 未标类型的叶子也要实现 | `tree::flatten_atomic` | M1 |
| T3 | 依赖序遍历：依赖先出，同层保持文档序；依赖指向 FOLDER 时展开为其 ATOMIC 后代；未知 id、自依赖忽略；成环按文档序打破，保证终止且每节点恰一次 | `requirement_order.topo_order` | A2（ARC 论文消融：DFS 序 −16~−52 pp） | `tree::topo_order` | M1 |
| T4 | 传递祖先（按拓扑序返回）供提示词附祖先设计摘要 | `requirement_order.ancestors_of` | A2 | `tree::ancestors_of` | M1（提示词用在 M2） |
| T5 | 节点指纹 sha256(id/name/description/scenarios/dependencies)[:16] | `requirement_order.node_fingerprint` | A6 Evolution 差异 | `tree::node_fingerprint` | M1（M3 使用） |
| T6 | FOLDER → ATOMIC 后代映射；收尾时按子节点判定给 FOLDER 发 design/implement/test 事件 | `main.folder_descendants`、`Flow.mark_folders` | C 回流 1：平台把 FOLDER 也计入需求数（keep「45 requirements」） | `tree::folder_descendants` + `events` 里的 `folder_verdict` 事件，Python 翻译成 mark_* | M1 |
| T7 | 节点描述文本（ID/Name/Description/Scenarios/Depends on） | `main.describe_node` | 提示词 | `tree::describe_node` | M1 |
| T8 | 上一轮 `.arc/traceability/requirements.json` 与指纹比对得未变节点 | `main.previous_requirement_records`、`unchanged_node_ids` | A6 | `tree::unchanged_from_previous` | M3 |

#### plan.rs（← main.py 的模式判定）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| P1 | 节点数 ≤ `OCTOS_ARC_CODEGEN_MAX_NODES`(2) 且未被 `codegen_blocked` → 单请求 codegen；否则 tool 模式 | `Flow.codegen_mode` | 第五版/round 23：Counter 1 请求 872 token，TB 2 请求 | `plan::Mode::Codegen/Tools`，`policy.mode.codegen_max_nodes` | M1 |
| P2 | `OCTOS_VERIFY_MODE` auto/minimal/full；≤ `OCTOS_SMALL_TASK_NODES`(2) 用最小自验（无 shell、≤8 次写、≤2 次读） | `Flow.minimal_mode`、`verify_text` | 第二阶段 2、v11 | `plan::verify_mode` | M2 |
| P3 | 骨架轮只在 ≥ `OCTOS_SKELETON_MIN_NODES`(3) 或 `OCTOS_SKELETON_ALWAYS=1` | `Flow.run` | 第二阶段 2（小题只跑一轮） | `plan::wants_skeleton` | M2 |
| P4 | 设计轮：`OCTOS_DESIGN_TURN`、≥ `OCTOS_DESIGN_MIN_NODES`(3)、`OCTOS_DESIGN_MODE` inline/separate | `Flow.node_cycle` | A3、R7/R8（inline 省 23% 费用） | `plan::design_mode` | M2 |
| P5 | Evolution 判定 = 输出目录已有 frontend/package.json 与 backend/package.json | `Flow.has_app`、`run` | A6；C：平台不设 ARCBENCH_TEMPLATE_DIR | `plan::is_evolution` | M1（探测在 M3） |
| P6 | 推理档位 auto：待实现节点 ≤1 → none，否则 low；`OCTOS_ARC_IMPLEMENT_REASONING` 只作用于小题的首个实现轮 | `Flow.start_llm_proxy`、`turn` | 第二阶段 1、v17（输出 token 降 3×）、round 23（TB none 首轮 2/6） | `plan::reasoning_for(turn_kind)` → `llm::ReasoningMode` | M1 |
| P7 | 按需求关键词裁剪契约块：session（login/password/…）、data（seed/option/…）；性能契约只在有会话的题 | `Flow.classify_tree`、`ui_contract`、`perf_text` | 第二阶段 3（Counter 提示词 3.5k→2.4k） | `plan::ContractFlags`，规则写进策略文件 `[prompts.keywords]` | M2 |
| P8 | 总预算未显式设置时 = max(3600, 1500 × 节点数) | `Flow.run` | C 回流 3、keep-local-3（480 s/节点截断 16/17 轮） | `budget::global_budget` | M1（预算细节 M2） |

#### codegen.rs（← codegen.py、main.py 的 codegen 路径）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| C1 | `<<<FILE path>>>…<<<END FILE>>>` 解析：路径归一、禁绝对/`..`、去掉包住正文的 ``` 围栏、同路径后者胜 | `codegen.parse_file_blocks` | 第五版 | `codegen::parse_file_blocks` | M1 |
| C2 | HTML 无 charset 时注入 `<meta charset="utf-8">` | `codegen.ensure_charset` | round 28（s5/s10：中文定位器全失败） | `codegen::ensure_charset` | M1 |
| C3 | 整块被 JSON 转义（`\n` 字面）时还原 | `codegen.unescape_flattened` | round 28（s12） | `codegen::unescape_flattened` | M1 |
| C4 | 部分转义的 JS 只在 `node --check` 由失败变通过时改写 | `codegen.repair_flattened_js` | round 28 | `codegen::repair_flattened_js` | M1 |
| C5 | 服务端实现 `<!--NAV-->` 时删除页面里重复的静态 login/register/logout 链接 | `codegen.dedupe_nav_links` | round 28（s2/s3/s15 strict mode） | `codegen::dedupe_nav_links` | M1 |
| C6 | 落盘（建目录、按后缀套用 C2–C4） | `codegen.write_files` | — | `codegen::write_files` | M1 |
| C7 | harness 写死两个 package.json：build 复制 src→dist 并生成无扩展名页面副本；backend `"type":"commonjs"` | `main.CODEGEN_MANIFESTS`、`write_codegen_manifests` | round 26（3e425ce2ebf6 ESM 崩溃）、round 28（s8 `/register` 404） | `codegen::write_manifests` | M1 |
| C8 | 一行 system prompt 替换内核 worker 提示 | `main.CODEGEN_SYSTEM` + `llm_proxy.replace_system_prompt` | round 22 | 直接构造 messages（无工具） | M1 |
| C9 | 紧凑用户提示：需求描述 + spec 全文（含 support helper）+ 固定文件布局 + 规则 + 尺寸规则（单节点 ≤20 行；多节点 NAV/cookie/校验/可见盒/无 HTML5 校验机制） | `main.CODEGEN_PROMPT`、`CODEGEN_SIZE_SMALL/FULL`、`spec_bodies` | round 22/23/27/28 | `arc/prompts/codegen-*.md`，`codegen::implement_prompt` | M1 |
| C10 | spec 默认端口 ≠ PORT 时附双端口契约句（`ARC_EXTRA_PORTS`） | `Flow.codegen_ports_clause`、`main.spec_base_ports` | round 24（3f0124e82113/784402a777c2 0/10） | `acceptance::spec_base_ports` + `codegen::ports_clause` | M1 |
| C11 | Evolution codegen：提示改为「保留现有应用、完整输出改动文件」并内嵌现有 html/js（≤30k 字符） | `Flow.node_cycle` | round 22/23 | `codegen::evolution_prompt` | M1 |
| C12 | codegen 修复：REPAIR_PROMPT + 失败摘要 + 内嵌源码 + 「每个改动文件完整输出」 | `Flow.acceptance_loop` | round 23、v16 | `codegen::repair_prompt` | M1 |
| C13 | 0/N 时一次整体重写（原提示 + 失败 + 内嵌源码） | `Flow.node_cycle.rebuild_prompt`、`OCTOS_ARC_REWRITE_ON_ZERO` | v14/round 23 | `codegen::rewrite_prompt` | M1 |
| C14 | 连续两轮失败摘要（数字归一化后）相同 → 该节点切 tool 模式并注入「换方法」纠正；≥ `OCTOS_ARC_CODEGEN_REPAIRS`(2) 轮仍失败 → tool 模式；`codegen_blocked` 每节点重置 | `Flow.acceptance_loop`、`node_cycle` | 91aaecaf31af、5747e6bcf530、round 23/28 | `loop::CodegenGate` | M1（切换本身 M1；tool 模式落地 M2） |
| C15 | codegen 轮请求上限 3 | `OCTOS_ARC_CODEGEN_REQUESTS` | v13 | `policy.requests.codegen`；单请求无工具，只用于瞬时错误重试上限 | M1 |

#### acceptance.rs（← acceptance.py）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| A1 | spec 文件名前缀 `REQ-x.y` 提取 | `acceptance.spec_node_id` | A1 | `acceptance::spec_node_id` | M1 |
| A2 | spec→节点映射：精确 id → 数量相同按数字序配对 → 父前缀；其余进回归集；别名表 | `acceptance.map_specs_to_nodes` | A1（TB 的 REQ-1.1/1.2 对 REQ-1/2） | `acceptance::map_specs_to_nodes` | M1 |
| A3 | 节点状态镜像到 spec 侧别名 id | `Flow.mark`、`OCTOS_ARC_ALIAS_SPEC_IDS` | A1 | 事件带 `aliases`，Python 翻译时镜像 | M1 |
| A4 | 测试目录定位：ARCBENCH_TESTS_DIR → /workspace/tests → 随包 public-tests（按根名） | `main.locate_acceptance_tests` | A3 | 留在 Python 胶水，经 runner-spec.json 传入 | M1 |
| A5 | 扫描 spec 里 `http://127.0.0.1:PORT` 得默认端口 | `main.spec_base_ports` | round 24 | `acceptance::spec_base_ports` | M1 |
| A6 | Playwright JSON 报告折叠：每 spec 一条结果；错误位置在 helper 时按 spec 文件归属 | `acceptance.summarize_report` | keep-local-4（failing nodes []） | `acceptance::summarize_report` | M1 |
| A7 | 四字段失败摘要（Feature / Failed at / Observation ≤900 字符 / Steps ≤8，超时标注，Call log 兜底） | `acceptance.failure_summaries` | A3、第六版（缺 Expected/Received） | `acceptance::failure_summaries` | M1 |
| A8 | 失败按 spec 文件 basename 归属节点 | `acceptance.nodes_for_failures` | keep-local-4 | `acceptance::nodes_for_failures` | M1（全套用在 M2） |
| A9 | 慢测试阈值 `OCTOS_ARC_SLOW_MS`(3000) → 修复提示附性能规则 | `RunSummary.slow`、`Flow.acceptance_loop` | A4 | `acceptance::RunSummary::slow` | M1 |
| A10 | 先找预装 Playwright：`OCTOS_ARC_PLAYWRIGHT_ROOT`、bundle/local-grader、tests 目录及祖先、/workspace、/app、/runner、/opt/playwright、/ms-playwright、$HOME、`npm root -g` 上级、/usr/local/lib…，再 `find / -maxdepth 6` 25 s | `playwright_candidates`、`find_playwright_root`、`find_playwright_by_search` | 紧急修正（da9a64b32c09：自装污染了平台的 npx 解析） | `acceptance::find_playwright` | M1 |
| A11 | 自装完全隔离：版本钉死（tests 的 lock/package.json，否则 1.63.0；绝不 latest）、私有 npm cache 与 PLAYWRIGHT_BROWSERS_PATH、npmmirror、540 s 上限、结束删除 | `playwright_version_hint`、`isolated_install_env`、`ensure_playwright`、`cleanup_playwright` | 紧急修正、d116ad5e3aa0 | `acceptance::private_install` | M1 |
| A12 | cgroup 内存上限 → worker 数 = min(请求数, 上限/700 MiB)，至少 1 | `container_memory_limit`、`workers_for_memory` | 29c840566f36（512 MiB、4 worker OOM） | `acceptance::workers_for_memory` | M1（M4 复核） |
| A13 | 构建：有依赖且无 node_modules 才 `npm install`；先 mkdir dist；`npm run build` 600 s | `AppServer.build` | c17bc1b44d26（dist 不存在） | `acceptance::AppServer::build` | M1 |
| A14 | 启动：`PORT`=smoke 端口，非 grader-like 时 `ARC_EXTRA_PORTS=0`；45 s 内等端口；启动即探测 | `AppServer.start` | R5 双端口 listen 两次 | `acceptance::AppServer::start` | M1 |
| A15 | 健壮性探测 `/favicon.ico`、未知路径、未知 API 必须有响应且进程活着 | `robustness_probe` | f9f0026819f1（ENOENT 杀进程，8 条 ECONNREFUSED） | `acceptance::robustness_probe` | M1 |
| A16 | grader-like 启动时 spec 默认端口也必须被绑定 | `AppServer.extra_ports_bound` | 3f0124e82113 | `acceptance::extra_ports_bound` | M1 |
| A17 | 停止：killpg SIGKILL、释放 smoke 端口、只杀 cwd 在项目内的额外端口监听者 | `AppServer.stop`、`free_port`、`free_owned_ports` | 运行主机共享 | `acceptance::AppServer::stop` | M1 |
| A18 | 把 tests 复制到 Playwright 根下的私有工作目录，写 config（timeout 10 s、retries 0、fullyParallel 开关、workers、json reporter、expect/action 4 s、navigation 6 s） | `AcceptanceRunner._prepare` | A3（10 s 与平台一致；子超时让失败带定位器） | `acceptance::Runner::prepare` | M1 |
| A19 | 运行：E2E_BASE_URL、CI=1、NODE_PATH、900 s 墙钟；被杀（rc<0/"Killed"）不是判定；收集到 0 条测试是错误不是 0/0 | `AcceptanceRunner.run` | 29c840566f36（OOM）、a6ccc437539f（0/0 盲修） | `acceptance::Runner::run` | M1 |
| A20 | 跑测试前 `git add -A`，跑完 `git checkout -- . && git clean -fdq -e node_modules -e dist -- frontend backend` 还原测试写坏的持久化数据 | `snapshot_worktree`、`restore_worktree` | r5-counter（db.json 被改成 -1 后提交） | `acceptance::with_worktree_snapshot` | M1 |
| A21 | 受保护目录（tests、requirements）快照 + sha256，每轮结束比对还原并把恢复清单作为纠正句 | `tree_digest`、`restore_tree`、`Flow.snapshot_protected/restore_protected` | 第三轮（a6ccc437539f 模型改了 /workspace/tests） | `guard::ProtectedTrees` | M2（codegen 轮不写盘，M1 不需要） |

#### loop.rs（← main.py 主循环）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| L1 | 节点预算 = min(`OCTOS_NODE_TIME_BUDGET`, max(240, 剩余/剩余节点数))；实现轮 ≤ min(`OCTOS_NODE_TIMEOUT`, 0.6×节点预算) | `Flow.node_cycle` | A7、keep-local-3 | `budget::NodeBudget` | M1 |
| L2 | 实现 → 验收循环：round 0 直接验收；通过即 commit 返回 True；通过数上升 commit 并记最优 sha；连续两次下降回滚到最优并注入纠正；两次无提升即停；`attempt == K` 停；剩余 < `OCTOS_MIN_REPAIR_SECONDS`(300) 不开修复轮；结束时不在最优点则回到最优点 | `Flow.acceptance_loop` | A3、f9f0026819f1（4/6↔3/6 振荡）、keep-local-3 | `loop::acceptance_loop` | M1 |
| L3 | 基础设施错误（构建/启动失败）作为「app startup」失败摘要进修复轮；测试进程被杀则无判定 | `Flow.acceptance_loop` | 29c840566f36 | 同上 | M1 |
| L4 | 修复前把 frontend/src 与 backend 源码快照到 `.arc/codegen/<node>-r<n>/` | `Flow.snapshot_sources` | round 27（27de75de0cd0 首轮代码不可追溯） | `loop::snapshot_sources` | M1 |
| L5 | 实现轮结束但缺 package.json 不判失败，进验收循环由构建错误驱动 | `Flow.node_cycle` | v6-counter | `loop::node_cycle` | M1 |
| L6 | 实现轮超时：保留盘上文件交验收；不当瞬时错误重放 | `Flow.node_cycle`、`OctosDriver._transient` | d5f9dccb（r1-tb 白烧 15 分钟） | `llm::is_transient` 排除自身超时 | M1 |
| L7 | 输出被 max_tokens 截断且未落盘 → 新会话按文件重写一次 | `Flow.node_cycle` | 76fb32a69d81 | `loop::node_cycle`（codegen：截断即视为失败进修复） | M1（tool 版 M2） |
| L8 | 单 spec 题在节点已判定时跳过全套；否则全套并行（grader-like、按内存定 worker）、失败按节点分组、同一失败集合即停、≤ `OCTOS_FINAL_REPAIR_ROUNDS`(2)、启动失败 = 全部节点失败、被杀保留逐节点判定 | `Flow.final_acceptance` | R7、第三轮 4、29c840566f36 | `loop::final_acceptance` | M1（单/双 spec 路径）+ M2（修复轮 tool 模式） |
| L9 | 启动演练 3 次（grader-like build+start），失败则 REHEARSAL_REPAIR_PROMPT | `Flow.rehearsal` | R0 | `loop::rehearsal` | M1（演练）/ M2（修复轮） |
| L10 | 骨架轮：≤4 次尝试、无 frontend/backend 时 nudge ≤2；只读 helper 与至多两个 spec | `Flow.skeleton`、`tests_prompt_for(skeleton=True)` | C 回流 2（keep 骨架轮 70 分钟） | `loop::skeleton` | M2 |
| L11 | 无本地判定的节点走终检轮 + 演练判定 | `Flow.run` | A1 | `loop::final_check` | M2 |
| L12 | 每节点提交（implement / accepted / repair n / keep best）、终态提交 | `Flow.commit`、`restore_app` | A3 | `git.rs` | M1 |
| L13 | smoke 端口 = `OCTOS_SMOKE_PORT`(3100)≠web_port；轮内 `PORT`=smoke；watchdog 每 5 s 杀掉自己进程对评测端口的绑定 | `Flow.__init__`、`_port_watchdog` | R0（runner 见到评测端口被占即终止） | `reap::PortWatchdog` | M4（codegen 路径不起服务，M1 不需要） |
| L14 | 收尾：残留进程清理、把嵌套一层的 app 提到根、释放评测端口、写 preview-ready.json | `_reap_stray_processes`、`_postflight_structure_check`、`_free_web_port`、`write_preview_ready` | 0764e8d77c54、R0 | `reap.rs`（M4）；preview-ready 留在 Python 胶水 | M1/M4 |
| L15 | 异常路径：未判定节点补 test_failed、FOLDER 补齐、run_failed，退出码 0 | `Flow.run` except | A1-4 | `run::execute` 的错误路径 + 事件 | M1 |
| L16 | （Python 无）codegen 实现轮回复里没有 `<<<FILE>>>` 块时，带格式提醒重试一次，再判 implementation_failed | — | 本机 rs-tb-4：REQ-1 一次无块回复让节点被跳过，依赖它的 REQ-2 全失败，后续 17 次请求都在补救（最终 2/10） | `flow::node_cycle`（纠正句 `codegen_no_blocks`） | M2 |
| L17 | （Python 无）全套修复轮记录最优状态（通过数上升即 commit），循环结束时最后一轮低于最优则回滚到最优并按最优轮的结果重记判定 | — | 本机 rs-tb-4：全套 0/10 → 修复 1 → 7/10 → 修复 2 → 2/10，Python 与 Rust 都交付了最后一轮；与节点循环「通过即快照、无提升回滚」同一规则 | `flow::final_acceptance` | M2 |

#### budget.rs（← guard.py 的预算部分 + P2-7）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| B1 | 总时间预算、剩余时间、耗尽后节点直接 implementation_failed | `Flow.remaining/time_up`、`run` | A7 | `budget::Global` | M1 |
| B2 | 每轮请求硬上限（实现 20 / 修复 10 / codegen 3），超限剥工具并要求收尾 | `llm_proxy.enforce_turn_budget`、`Flow.turn` | v13（20 次调用的修复轮） | tool 模式：内核会话 `max_iterations`；codegen：重试上限 | M1（codegen）/ M2 |
| B3 | 全局 Token 与回合上限、超限降级为「每节点一次实现 + 一次全套」 | 目标书新增（P2-7 的 `--node-token-budget`） | keep ¥57 | `budget::Guardrails` | M4 |

#### llm.rs（← llm_proxy.py；做进 octos-llm 的 provider 配置）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| M1 | DeepSeek 推理注入：none → `thinking:{type:disabled}`；low/medium/high → `reasoning_effort` + `thinking:enabled`；非 deepseek 模型不动 | `llm_proxy.inject_reasoning` | 第二阶段 1（455→279→132 completion tokens） | `ChatConfig.reasoning_effort` + `ReasoningStyle::EffortAndThinkingToggle`（`api.arc-bench.com` 已被 `openai.rs` 识别） | M1 |
| M2 | `max_tokens` 下限 32768（内核 arc.11 stdio 会话默认 4096） | `llm_proxy.ensure_max_tokens` | 76fb32a69d81（两个实现轮截断） | `ChatConfig.max_tokens = policy.reasoning.max_tokens_min` | M1（tool 模式经 profile `gateway.max_output_tokens` M2） |
| M3 | 去流式：上游一次 JSON 响应 | `llm_proxy.destream_request/to_sse` | cc066e8e11f6（平台把 SSE 逐 chunk usage 累加 12×） | codegen 直接 `chat()` 非流式；tool 模式会话 `OCTOS_DISABLE_STREAMING=1` | M1 |
| M4 | 每请求用量记账：prompt/completion/reasoning/total、cache hit（DeepSeek 与 OpenAI 两种字段）、耗时、请求/响应字节、消息形状 → `.arc/llm-usage.jsonl` | `llm_proxy.usage_record`、`request_shape` | 更正·前缀缓存、cc066e8e11f6 | `llm::UsageLedger`（同字段名，`metrics.py` 直接可读） | M1 |
| M5 | 裁掉与 ARC 无关的 system prompt 段与 6 个工具 schema | `llm_proxy.trim_request`、`DROP_SECTIONS/DROP_TOOLS` | v4；v15 证明与首轮成败无关 | tool 模式用内核 stdio/solo 精简 profile（B 第三阶段），不再代理裁剪 | M2 |
| M6 | 最小自验轮剥掉 shell 工具 | `extra_drop_tools`、`OCTOS_ARC_DROP_SHELL` | v11（无 shell 修复轮仍 41 次调用） | 会话 profile 的工具 deny 列表 | M2 |
| M7 | 瞬时错误重试 3 次（30 s、60 s），自身超时不重放 | `OctosDriver._run_with_retries/_transient` | d5f9dccb | `llm::call_with_retries` | M1 |
| M8 | 启动前原始 chat 探测，等代理恢复最多 10 分钟 | `main.probe_endpoint` | 平台代理抖动 | `llm::probe` | M1 |
| M9 | 请求体转储（调试） | `OCTOS_ARC_PROXY_DUMP` | 前缀分析 | `policy.debug.dump_requests` | M1 |

#### driver.rs（← octos_stdio.py、main.py 的 OctosDriver；tool 模式）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| D1 | 起 `octos serve --stdio --solo --data-dir … --danger-full-access`，NDJSON JSON-RPC：`profile/local/create`（唯一 id）→ 把 hooks 写进 profile 文件 → `profile/llm/upsert` → `session/open` → `turn/start` → 收 `message/delta`、自动批准 `approval/requested`、`turn/completed`/`turn/error` | `OctosStdioSession` | A5、第三轮 1（hooks 只从 profile 自身 config 生效） | `driver::StdioSession`（同一可执行文件 `current_exe()`） | M2 |
| D2 | 会话粒度 `OCTOS_SESSION_SCOPE` turn/node/run（默认 turn） | `OctosDriver` | v8-tb（node 粒度 49 请求 1.1M prompt） | `policy.session.scope` | M2 |
| D3 | 30 s 心跳日志（runner 杀静默进程）；`octos chat` 回退 | `_run_with_heartbeat`、`run_octos` | 平台 | 事件流心跳 | M2 |
| D4 | 全部通知写 `.arc/octos-events.jsonl`；`[arc-mod]` 标记转发 | `_log_event` | metrics.py 口径 | `events.rs` 镜像 turn/completed | M1（codegen 也写）/ M2 |
| D5 | 内核环境：provider 判定与 key 环境变量、config.json（sandbox allow_network、memory refresh 关、`gateway.max_output_tokens` 65536、deepseek `reasoning_effort` low、hooks）、`OCTOS_DISABLE_STREAMING=1`、`OCTOS_DANGER_FULL_ACCESS=1`、npmmirror | `main.build_octos_env`、`write_profile_defaults` | 平台代理拒绝 SSE、容器即沙箱、npmjs 慢 | Python 胶水只留环境变量；内核侧 profile 由 driver.rs 写 | M2 |
| D6 | 二进制定位与下载（gh-proxy 镜像、12 次、curl --http1.1） | `find_octos`、`_download_octos` | 平台到 GitHub 的 HTTP/2 流被杀 | 留在 Python 胶水，M5 加 SHA-256 校验 | M5 |

#### guard.rs（← guard.py、main.py 的保护）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| G1 | 轮内监视：写了文件、结束语宣称完成却没跑 build/start/curl；同一错误连续 ≥3；写保护路径（允许 .arc/design/）；纠正句注入下一轮 | `guard.TurnMonitor`、`OCTOS_GUARD` | A7 | `guard::TurnMonitor`（订阅 driver 事件） | M2 |
| G2 | `before_tool_call` hook 拒绝写 tests/requirements（写进 profile config） | `main.protected_hooks`、`hooks/deny_protected.py` | 第三轮 1 | 内核内置 hook（不再需要 Python 脚本） | M2 |
| G3 | harness 纠正句：同一失败两次、两次下降已回滚、改了官方文件已还原、缺 package.json、实现轮超时、截断重写、并行不干扰、演进回归 | 散布在 `Flow` | 各云端归因 | `arc/prompts/corrections.md`（按键引用） | M1（codegen 用到的）/ M2 |

#### events.rs（← metrics.py、arcbench_agent_runtime 的调用点）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| E1 | 节点事件 design/implement/test 的 started/done/failed（含别名镜像）、FOLDER 推导、run started/completed/failed | `Flow.mark`、`mark_folders`、`events.mark_run_*` | A1 | 内核写 `.arc/octos-arc-events.jsonl` 并在 stdout 逐行输出 `@@arc-event {…}`；Python 翻译成 `mark_*` | M1 |
| E2 | 每条测试结果写 tests 表；设计 JSON 写 node_contracts/interfaces | `Flow.record_tests`、`save_design` | A1-3 | `test_result`、`design` 事件，Python 翻译 | M1 / M2 |
| E3 | 每次 commit 触发 commit_history 刷新信号 | `GitClient.commit` | 平台 UI | `commit` 事件 | M1 |
| E4 | `[usage] provider totals` 收尾行 | `Flow.log_usage_summary` | 平台不导出 .arc 时也有账 | `usage_total` 事件 + 日志行 | M1 |
| E5 | metrics.py 口径（turn/completed、token_cost_update、llm-usage、runner-events、local-grade） | `metrics.py` | 本机对照 | 内核镜像 turn/completed 与 llm-usage 记录，脚本不改 | M1 |

#### reap.rs（← main.py 的进程回收）

| # | Python 策略 | 来源 | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|---|
| R1 | 收尾/异常时：打印 free -m、cgroup memory.*、ps 按 RSS 前 20；杀 chrom/headless_shell/playwright/octos serve/node/npm（排除自身与父进程） | `_reap_stray_processes` | 0764e8d77c54、e60fb3545eae（评测阶段 4 worker 1 秒被 Killed） | `reap::report` + `reap::sweep_all(root)`（只杀能归属本次运行的：cwd/命令行在工作目录内或本进程后代——Python 版在共享宿主上会误杀）；每节点开始（index>1）`reap::sweep_workspace(root)` 回收 cwd 在 frontend/backend 里的 node/npm/npx/sh/bash（#81 的 `should_reap` 语义） | M4 ✔ |
| R2 | 评测端口 watchdog | `_port_watchdog` | R0 | `reap::PortWatchdog`（5 s 轮询；我们的进程杀、外来进程报告一次） | M4 ✔ |

#### 里程碑状态

- M1（PR #76）：对照表中标 M1 的行全部落地。
- M2（本分支）：标 M2 的行全部落地——driver.rs（D1–D5）、guard.rs（G1–G3、A21）、flow.rs 的 tool 模式回合（P2–P4、L7–L11、C14 的切换落地）、内核侧等价物（M5：内核 `OCTOS_DISABLE_STREAMING`、M6：`OCTOS_STDIO_SOLO_TOOLS`、P6 的 tool 模式：`OCTOS_STDIO_REASONING_EFFORT`、B2：profile `gateway.max_iterations`）、hook 改为 `octos arc deny-protected`。M3/M4/M5 的行未动。

#### main 在分支点之后新增的 Python 策略（工作流 A，#79 / #82 / #83；以 main 最新为准，待收编）

| # | Python 策略（arc/CHANGELOG.md 轮次） | 为什么存在 | Rust | 里程碑 |
|---|---|---|---|---|
| N1 | 轮 29：`dedupe_nav_links` 从服务端渲染进占位的锚点推导要删的 href（原来固定 /login|/register|/logout） | 通用性复查 | `codegen::dedupe_nav_links`（`HREF` 推导，单测同 Python 夹具） | M3 ✔ |
| N2 | 轮 30：探测后没有任何节点的 spec 通过现有应用 → frontend/backend 移到 `.arc/template-discarded/`，按新题从头建 | 云端 c30b29eab45b / 10b04d36f704：平台模板是占位脚手架，被当成可演进的应用 | `flow::discard_template`；`probe_count`；RunPlan 以 evolution=false 重建（≥3 节点补骨架轮） | M3 ✔ |
| N3 | 轮 30：codegen 推理档位与 size rule 按 spec 字符数（`OCTOS_ARC_CODEGEN_REASONING_CHARS`=5000）而不是节点数 | 2 节点 Counter 树付了 4.4k 推理 token 还拿到导航/cookie 机制 | `reasoning.codegen_reasoning_chars`；`Flow::codegen_reasoning`、`current_spec_chars`；`CodegenInputs.small_rule` | M3 ✔ |
| N4 | 轮 31：`OCTOS_ARC_DRYRUN=1` 用 `DryRunDriver` 替换内核驱动 | 给本工作流做结构对等 | `octos arc run --dry-run`（M1 已有，语义相同） | — |
| N5 | 轮 32：tiny-spec 档（spec < `OCTOS_ARC_TINY_SPEC_CHARS`=1500）：提示只含 spec 语句 + 一句输出要求，系统提示 "Reply with HTML only."，回复裸 HTML 写成 index.html，harness 写固定静态 server（`TINY_SERVER_JS`），失败回退 compact codegen；`OCTOS_ARC_TINY=0` 关闭 | Smoke 榜前三 ≈250 token/题，我们 ≈845 | `mode.tiny` / `mode.tiny_spec_chars`；`codegen::{strip_code_fences, compact_spec_lines, tiny_server_js}`；`Flow::{tiny_mode, tiny_turn}`、`codegen_turn_with`（system / 无格式段 / 裸 HTML 落盘）；提示词 `tiny-*.md` 从 `main.py` 常量导出 | M3 ✔ |
| N6 | wf-adapter-30（#81，main@fdbcd106）：>2 节点树修复轮 3（显式 `OCTOS_REPAIR_ROUNDS` 例外）、全套 `workers_for_final` 450 MiB/worker、每节点 `reap_workspace_processes`（cwd 在 frontend/backend 里的 node/npm/npx/sh/bash）、费用护栏 `OCTOS_ARC_MAX_TOTAL_TOKENS`=max(6M, 2.5M×节点)、`OCTOS_ARC_MAX_TURNS`=max(24, 4×节点)、`OCTOS_ARC_MAX_TOTAL_TOKENS_ABS`（选配）——按 keep 2224a9013528 标定 ≈3× 健康运行 | keep 云端 32/32、¥16.58、1,152 请求 | `repair.rounds_large_tree` / `large_tree_nodes`；`acceptance.final_memory_per_worker_mib`；`reap::should_reap` / `sweep_workspace`（节点开始时，index>1）；`budget.max_total_tokens` / `max_turns` / `max_total_tokens_abs`（-1 = 树已知后推导），触发后 `[guard] cost guard tripped`，语义同 Python `wound_down` | M4 ✔ |
| N7 | 轮 33（#90）：启动探测改 GET /models（不计费，非 5xx 即可用），失败才发一次 thinking disabled、max_tokens 1 的 chat 请求；tiny 档输出句收紧（最小标记、一个内联脚本、无空行） | 旧探测让模型先推理再答 OK，≈¥0.0008/次 | `llm::probe`（reqwest GET，再最小 chat）、`endpoint_is_up` / `minimal_probe_body`；`tiny-prompt*.md` 从 `main.py` 常量重导 | M4 ✔ |
| N8 | 轮 34（#94）：探测策略按档位——整题都是 tiny 档就不探测（第一个真实请求即探测），dry-run 不探测，其余 GET /models；tiny 回复只要页面标记（无 doctype/head），`looks_like_markup` 接受片段，`ensure_charset` 补 charset 头 | 探测约 ¥0.0008/次，是 tiny 题的三分之一 | `Flow::all_specs_tiny`、探测分支；`codegen::looks_like_markup`；`tiny-prompt*.md` 重导 | M4 ✔ |
| N9 | 轮 35（#95）：任何规模的树每节点都走单请求 codegen（`OCTOS_ARC_CODEGEN_MAX_NODES` 默认 999）；提示引用 `relevant_sources` 选出的现有源码（后端入口优先，其余页面按 spec 词项命中数排序，预算 `OCTOS_ARC_CODEGEN_CONTEXT_CHARS`=90,000 字符减去 spec，其余只列名）；spec 单独超过预算 60% 的节点走 tool 模式；codegen 模式下 harness manifests 取代骨架轮（`OCTOS_SKELETON_ALWAYS=1` 例外） | keep/bookstack tool 模式每节点 ≈36 次请求 | `mode.codegen_max_nodes`=999、`prompts.codegen_context_chars`；`codegen::{spec_terms, relevant_sources}`（替换 codegen 路径的 `inline_sources`）；`Flow::codegen_context_fits`；`RunPlan.wants_skeleton` 含 `!codegen` | M4 ✔ |
| N10 | 轮 36（#96）：L17 回流 Python（全套修复轮交付最优轮，`record_full_suite`） | 本工作流 rs-tb-4 | Rust 原有（M2） | — |

#### 策略文件 `arc/arc-policy.toml`（← 约 60 个环境变量）

全部 `OCTOS_*` / `OCTOS_ARC_*` 开关映射为有默认值、有注释的字段，环境变量仍可覆盖（命令行 > 环境变量 > 策略文件 > 内置默认）。字段清单与对应环境变量见 `arc/arc-policy.toml` 的注释；`policy::tests` 逐字段断言默认值与 Python 一致、环境变量覆盖生效。留在 Python 胶水的变量：`ARCBENCH_*`（平台输入）、`OCTOS_BIN`、`OCTOS_CACHE_DIR`、`OCTOS_RELEASE_URL`、`OCTOS_ARC_ENGINE`。

#### 提示词 `arc/prompts/*.md`

`policy.txt` 与 Python 里的全部模板（APP_SKELETON / NODE / DESIGN / REPAIR / FINAL_CHECK / REHEARSAL / UI_CONTRACT 三块 / PERFORMANCE / ARCHITECTURE / VERIFY 两种 / PORT_RULES / ACCEPTANCE_TESTS / codegen 四段 / 纠正句）逐字搬到 `arc/prompts/`，占位符用 `{name}`。M1 只有 codegen 段进入运行路径；其余在 M2 接入 tool 模式时使用，但文件在 M1 就位，便于对照。

### M1：tree + codegen + acceptance 最小闭环（PR 待编号）

**改动位置**：`crates/octos-arc/src/{policy,prompts,tree,plan,codegen,acceptance,llm,events,git,budget,flow,run,envs}.rs`（新增），`runner.rs`（`octos arc` 增加 `run` 子命令，原 create/evolve 形式不变），`crates/octos-cli/src/commands/mod.rs`（接线），`crates/octos-llm/src/openai.rs`（`OpenAIProvider::with_chat_timeout`：非流式请求原来固定 300 s 超时，整应用单响应生成需要更长），`arc/arc-policy.toml` 与 `arc/prompts/*.md`（策略与提示词，运行时读取），`arc/rust_engine.py`（胶水，仅在 `OCTOS_ARC_ENGINE=rust` 时由 `main.py` 转入），`arc/pack.sh`（打包新增文件），`.gitignore`（`*.md` 全局忽略规则对 `arc/prompts/` 例外）。

**命令**：`octos arc run --spec <output>/.arc/runner-spec.json --policy arc/arc-policy.toml [--dry-run] [--set key=value]`。runner-spec.json 由胶水写出（requirement_path、output_dir、web_port、tests_dir、bundle_dir、model 路由；key 只从环境变量读）。内核把事件写到 `.arc/octos-arc-events.jsonl` 并在 stdout 逐行输出 `@@arc-event {…}`，胶水实时翻译成 ARC 的 `mark_*`、tests 表与 commit 刷新信号；`.arc/llm-usage.jsonl` 与 `.arc/octos-events.jsonl`（turn/completed 与 token_cost_update 镜像）保持 `metrics.py` 口径不变。

**M1 覆盖范围**：对照表中标 M1 的条目全部落地；tool 模式（stdio 驱动、骨架/设计/终检轮、全套修复轮、演练修复轮、守护）在 M2。codegen 修复两轮后或同一失败连续两次时，Python 会切到 tool 模式，M1 在该点保留最优状态并在日志写明；Smoke 两题不触发该路径。`loop.rs` 在 Rust 里叫 `flow.rs`（`loop` 是关键字）。

**通用性自查（0.1 节）**：本 PR 没有含任务名或 REQ 编号的条件分支。相对 Python 逐字搬运的提示词，改写成通用形式的有：UI 契约里「(username, city, date)」「one "Register" link, one "Login" link」「the count is initially 0」「password-strength meters, counters, previews」分别改为「a user name, a chosen option, a date」「specs locate links by href」「a value shown on load」「any live indicator derived from what the user is typing」；性能契约里 scryptSync 的具体参数改为「每请求 CPU 预算 30 ms，选满足预算的哈希参数」；codegen 多节点规则里的 TB 链接文本改为「按 spec 复制导航文本与 href」；契约关键词表去掉「nationalit / 车次 / train」（只对订票类题目有意义），保留 seed/published/fixture/option/select/dropdown/选项/下拉/预置。所有阈值（K=5、codegen 修复 2、慢测试 3 s、10 s 单测超时、700 MiB/worker、32768 max_tokens、1500 s/节点）都来自策略文件且与 Python 默认一致，从容器事实（cgroup）、spec 文本（默认端口）或验收输出（失败摘要）推导。

**M1 对等验证（本机，2026-09-14，同一二进制 `target/release/octos` = main@ea503546 + 本分支，同一模型 `deepseek-v4-flash` 经 `api.arc-bench.com`，`run-task-local.py` 默认配置，`grade-local.py` 公开测试打分；每行一次运行，`billed` 取 `.arc/llm-usage.jsonl`，即平台计费同口径的供应商 usage）**

| 运行 | 路径 | 请求数 | prompt tokens | completion tokens | 耗时 s | 公开测试 | 节点状态 |
|---|---|---:|---:|---:|---:|---|---|
| py-counter-1 | Python | 1 | 509 | 327 | 11 | 1/1 | REQ-1、ROOT PASSED |
| py-counter-2 | Python | 1 | 509 | 349 | 9 | 1/1 | 同上 |
| py-counter-3 | Python | 1 | 509 | 309 | 9 | 1/1 | 同上 |
| py-counter-4 | Python | 1 | 509 | 357 | 9 | 1/1 | 同上 |
| py-counter-dump | Python | 2（首轮 0/1，一次重写） | 1,541 | 409 | 19 | 1/1 | 同上 |
| rs-counter-1 | Rust | 1 | 510 | 374 | 12 | 1/1 | 同上 |
| rs-counter-2 | Rust | 2（首轮 0/1：按钮无脚本，一次重写） | 1,555 | 415 | 21 | 1/1 | 同上 |
| rs-counter-3 | Rust | 1 | 510 | 342 | 11 | 1/1 | 同上 |
| rs-counter-4 | Rust | 1 | 510 | 361 | 12 | 1/1 | 同上 |
| rs-counter-dump | Rust | 1 | 510 | 374 | 13 | 1/1 | 同上 |
| py-dice-1 | Python | 1 | 436 | 307 | 8 | 1/1 | 同上 |
| py-dice-2 | Python | 1 | 436 | 304 | 9 | 1/1 | 同上 |
| rs-dice-1 | Rust | 1 | 437 | 327 | 11 | 1/1 | 同上 |
| rs-dice-2 | Rust | 1 | 437 | 315 | 11 | 1/1 | 同上 |

- 通过率：14/14 次运行公开测试 1/1，节点状态与 FOLDER 状态与 Python 路径一致。
- 请求数：两条路径各有 1 次首轮 0/1（模型采样：Rust 那次首轮的 `index.html` 按钮没有脚本；Python 那次是 `py-counter-dump`），都由「0/N 时一次重写」修好；其余运行都是 1 次请求。
- Token：单请求运行的 prompt 差 1 token（510 对 509、437 对 436）。用 `OCTOS_ARC_PROXY_DUMP=1` 抓的 Python 侧请求体与 Rust 侧逐字段比对：model、`max_tokens: 32768`、`temperature: 0.0`、`stream: false`、`thinking: {type: disabled}`、system 消息完全一致，user 消息只差末尾一个换行（内核会话对回合输入做 trim，Rust 侧原样保留了模板末尾的换行）。已改为同样 trim（见下一行的复测）。completion 是采样波动：Counter 单请求均值 Python 335.5、Rust 362.8（+8.1%），Dice 305.5 对 321（+5.1%）；单请求总 token Counter 844.5 对 872.8（+3.3%）、Dice 741.5 对 758（+2.2%），都在 ≤10% 内。
- `metrics.py` 的 cost 列两条路径估价表不同（Python 路径由内核会话按 deepseek-chat 价估，Rust 路径的账本按 `octos-llm` 的 deepseek-v4 价估），对照以 token 为准；平台计费以其 meter 为准。
- 云端未评测（M5 前不请求云端运行）。
- trim 之后复测（同一二进制重新编译）：rs-counter-5 1 请求 / 509 prompt / 343 completion / 1/1；rs-dice-3 1 请求 / 436 / 309 / 1/1——prompt 与 Python 路径逐 token 相同。

### M2：loop + budget + 端口/CommonJS 契约 — tool 模式与 Ticket Booking（PR 待编号）

**改动位置**：`crates/octos-arc/src/driver.rs`（内核 stdio 会话驱动）、`guard.rs`（守护、受保护目录、`deny_protected`）、`flow.rs`（tool 模式回合与所有轮次）、`runner.rs`（隐藏子命令 `octos arc deny-protected`）、内核 `crates/octos-agent/src/agent/{llm_call,detection}.rs`（`OCTOS_DISABLE_STREAMING=1` 直接非流式）、`crates/octos-cli/src/runtime/{profile,session}.rs`（`OCTOS_STDIO_SOLO_TOOLS` 会话级工具收窄、`OCTOS_STDIO_REASONING_EFFORT`）、`arc/metrics.py`（按 `requests` 字段计数）。

**tool 模式怎么跑**：`octos arc run` 从自己的可执行文件起 `octos serve --stdio --solo --data-dir <临时目录> --danger-full-access`（与 Python 的 `octos_stdio.py` 同一协议），每轮新会话（`session.scope = turn`）。Python 代理承担的四件事的内核侧等价物：去流式 → agent 直接非流式（`OCTOS_DISABLE_STREAMING`，此前内核根本不读这个变量）；推理档位 → `OCTOS_STDIO_REASONING_EFFORT`；裁剪工具（去 `ask_user_question/check/tool_search/update_plan`，最小自验轮再去 shell）→ `OCTOS_STDIO_SOLO_TOOLS`（只能收窄；实测发现 coding profile 里 shell 工具注册名是 `bash`，且会话重绑定 cwd 时重新创建沙箱工具，所以 allow-list 在 profile 与会话两级都应用）；每轮请求上限 → profile 的 `gateway.max_iterations`（内核以 `budget` 错误结束回合，harness 视为「回合被上限终止」，盘上文件照常进验收）；`max_tokens` 下限 → profile 的 `gateway.max_output_tokens`。请求数按内核进度事件的 `iteration` 最大值计（`token_cost_update` 是每回合一条）。写保护 hook 由 `octos arc deny-protected` 承担（不再需要 Python 脚本）。系统提示词不再裁剪：stdio/solo 的精简 worker 提示是内核内置的（第三阶段 B），v15 对照也表明裁剪与首轮成败无关。

**Counter 强制 tool 模式的实测**（`OCTOS_ARC_CODEGEN=0`，同一内核）：修 allow-list 前 11 次工具调用（含 4 次 `bash`）、52,363 prompt（缓存 40,192）；修后 6 次 `write_file`、3 次 LLM 调用、20,948 prompt（缓存 12,032）、2,074 completion、30 s、1/1。

**通用性自查（0.1 节）**：本 PR 没有新增提示词规则；新增的三个内核开关（非流式、工具收窄、推理档位）都是按回合形状而不是按题目取值（节点数 ≤2 的树关 shell，待实现节点 ≤1 关推理），条件只来自需求树的节点数。`deny-protected` 只用运行时传入的目录判定。

**M2 对等验证：Ticket Booking（本机，2026-09-14，同一二进制、同一模型 `deepseek-v4-flash`、`run-task-local.py` 默认配置、`grade-local.py` 公开测试 10 条；每行一次运行；两条路径交替运行，`billed` 取 `.arc/llm-usage.jsonl`）**

同价表估价用 `octos-llm` 的 DeepSeek V4 Flash 价（输入 $0.27/M、缓存命中 0.1×、输出 $1.10/M）；平台拟合估价用 `arc/CHANGELOG.md` 里从平台账单拟合的 ≈¥3/M 输入、≈¥30/M 输出（平台没有观察到缓存折扣）。

| 运行 | 路径 | 请求 | prompt | cache hit | completion | 其中推理 | 同价表估价 $ | 平台拟合估价 ¥ | 耗时 s | 公开测试 | REQ-1 首轮 | REQ-2 首轮 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|
| py-tb-1 | Python | 2 | 13,271 | 6,144 | 42,802 | 35,560 | 0.0492 | 1.324 | 275 | 10/10 | 6/6 | 4/4 |
| py-tb-2 | Python | 2 | 13,243 | 8,192 | 46,024 | 38,690 | 0.0522 | 1.420 | 278 | 10/10 | 6/6 | 4/4 |
| py-tb-3 | Python | 3 | 16,983 | 8,192 | 19,484 | 10,297 | 0.0240 | 0.635 | 154 | 10/10 | 4/6 | 4/4 |
| py-tb-4 | Python | 2 | 12,781 | 8,192 | 13,666 | 4,498 | 0.0165 | 0.448 | 96 | 10/10 | 6/6 | 4/4 |
| rs-tb-1 | Rust（修 L16/L17 前） | 5 | 34,150 | 14,592 | 29,615 | 21,789 | 0.0383 | 0.991 | 250 | 10/10 | 4/6 | 4/4 |
| rs-tb-2 | Rust（同上） | 4 | 32,421 | 18,432 | 27,530 | 18,905 | 0.0346 | 0.923 | 231 | 10/10 | 0/6 | 0/4 |
| rs-tb-3 | Rust（同上） | 3 | 17,945 | 8,192 | 28,185 | 19,503 | 0.0339 | 0.899 | 255 | 10/10 | 1/6 | 4/4 |
| rs-tb-4 | Rust（同上） | 18 | 166,597 | 124,928 | 60,120 | 23,907 | 0.0808 | 2.303 | 530 | **2/10** | 无文件块 | 0/4 |

- **请求体对照**：REQ-1 的 codegen 请求 Python 5,145 prompt tokens、Rust 5,126，user 消息 18,221 对 18,201 字符，差的 20 个字符正好是多节点 codegen 规则里被通用化改写的导航句（Python 原文写死 `登录`/`Register`/`退出登录` 三个 TB 文本；Rust 版改为「文本取测试里该链接正则的第一个候选、href 取测试点击的」），其余逐字相同；两条路径都是 `mode=low`（thinking enabled + reasoning_effort low）。缓存命中两边都恒为 4,096（DeepSeek 前缀缓存的粒度，与谁先跑无关），不能用来判断差异。
- **首轮差异的归因（逐条核对代码快照 `.arc/codegen/REQ-1-r0/`）**：rs-tb-2 的 0/6 是生成代码在异步读文件回调里对 `null` 取 `.name`（`server.js:202`），本机复现：进程在第一个请求上崩溃，之后全部 `ERR_CONNECTION_REFUSED`；rs-tb-3 的 1/6 是「下一步」按钮 4 s 内不可点击（页面脚本问题）；rs-tb-1 的 4/6 是退出登录后没有出现「登录」链接（`e2e.ts:63`），这一条与导航句的改写有关；rs-tb-4 的 REQ-1 回复 10,023 completion（6,843 推理）却没有文件块。py-tb-3 的 4/6 是两条校验用例。也就是说 4 次 Rust 首轮失败里 3 次是与提示词无关的模型采样（崩溃 bug、脚本 bug、无文件块），1 次可能与改写的导航句有关。
- **导航句的处理**：保留通用规则但恢复具体的 HTML 形状——`<a href="/login">LOGIN</a> <a href="/register">REGISTER</a>` / `<span>USERNAME</span> <a href="/logout">SIGN_OUT</a>`，其中大写占位是「测试里该链接正则的第一个候选」，href 是测试点击的路径。规则本身对任何带会话的 Web 题成立（登录/注册/退出三条路由是测试用 `a[href=…]` 定位的约定），不再含任何题目文本。
- **循环层的两条通用改进（L16、L17）**：rs-tb-4 暴露的是 Python 与 Rust 共有的两个洞，都不是提示词问题。L16：codegen 回复没有文件块时 Python 直接判 implementation_failed 并跳过节点，依赖它的节点必然全失败，rs-tb-4 后面的 17 次请求（其中 tool 模式修复一轮 10 次）都在补救；改为带格式提醒重试一次。L17：全套修复轮 Python 与 Rust 都是「每轮修复后 commit、交付最后一轮」，rs-tb-4 里修复 1 把 0/10 提到 7/10、修复 2 又降到 2/10 并被交付；改为记录最优轮并在结束时回滚到最优（与节点循环同一规则）。两条都只依赖运行时事实（回复里有没有文件块、全套通过数的升降）。
- **修 L16/L17 与导航句之后的复测**（同一二进制重新编译，4 次连续运行）：

| 运行 | 路径 | 请求 | prompt | cache hit | completion | 其中推理 | 同价表估价 $ | 平台拟合估价 ¥ | 耗时 s | 公开测试 | REQ-1 首轮 | REQ-2 首轮 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|
| rs-tb-5 | Rust（修后） | 2 | 13,458 | 6,144 | 42,412 | 35,521 | 0.0488 | 1.313 | 253 | 10/10 | 6/6 | 4/4 |
| rs-tb-6 | Rust（修后） | 2 | 12,870 | 8,192 | 14,355 | 7,295 | 0.0173 | 0.469 | 106 | 10/10 | 6/6 | 4/4 |
| rs-tb-7 | Rust（修后） | 13 | 135,269 | 109,312 | 36,704 | 21,193 | 0.0503 | 1.507 | 298 | 10/10 | 5/6 | 4/4 |
| rs-tb-8 | Rust（修后） | 2 | 12,434 | 8,192 | 16,063 | 10,201 | 0.0190 | 0.519 | 120 | 10/10 | 6/6 | 4/4 |

- **M2 结论**：修后 Rust 4/4 次 10/10（修前 3/4），Python 4/4 次 10/10。同价表估价均值 Rust 修后 $0.034（0.017–0.050）对 Python $0.035（0.017–0.052）；平台拟合估价均值 ¥0.95 对 ¥0.96。请求数：Rust 修后 2/2/13/2 对 Python 2/2/3/2——rs-tb-7 的 13 次是 REQ-1 首轮 5/6、codegen 修复一轮后同一失败再现，按 Python 同样的规则切到 tool 模式修复（一轮 10 次 LLM 调用、13 次工具调用、118,718 prompt 里 101,120 是缓存命中、82 s）后 6/6；这一轮在同价表下 $0.011，在平台拟合价（无缓存折扣）下 ¥0.58，是该题 tool 模式修复的固有成本，Python 路径遇到同样的首轮结果会走同一条路（py-tb-3 的 4/6 一轮 codegen 修复就过了，所以没触发）。REQ-1 首轮 6/6 的比例：修后 3/4，与 Python 的 3/4 相同。「请求数与费用不高于 Python」：费用成立（均值持平、区间重合），请求数在 4 次里 3 次持平、1 次因 tool 模式修复更高。
- 云端未评测（M5 前不请求云端运行）。

**本机运行与云端窗口的重叠（如实记录）**：本工作流的克隆基于 main@ea503546，本地 `docs/results.md` 还没有「窗口」段；2026-09-14 09:01 UTC 起 C 的 Web 全量串行（bookstack 17fad96c6235 起）占用 key 的公告（c64addcf）在合并 main 时才看到。09:01 UTC 之后本机用该 key 的运行：rs-counter-tool/tool2（09:27、09:36）、py/rs-tb-1…8（09:40–10:54）、py/rs-evo-1/2（10:55–10:56），合计 prompt 559,017 + completion 382,296 token，按平台拟合价约 ¥13.2（同价表 $0.57），会计入正在跑的云端账单，归档时需按此扣除；逐条时间与 token 见 PR。此后本工作流不再用 key，只用 `--dry-run` 做结构验证，直到统筹宣布窗口结束。

### M3：Evolution 探测（PR 待编号；代码随 M2 分支，实测数据如下）

**改动位置**：`arc/rust_engine.py`（`snapshot_previous_requirements`：在平台运行时用新树覆盖 `.arc/traceability/requirements.json` 之前把模板自带的表复制到 `.arc/previous-requirements.json`，路径写进 runner-spec 的 `previous_requirements`）、`crates/octos-arc/src/run.rs`（字段）、`flow.rs`（`previous_requirement_records` 优先读快照）。探测本身（fingerprint → 对未变节点跑 spec 当回归 → 只对未通过节点调模型）在 M1 就已按 Python 搬入。

**M3 对等验证（本机，2026-09-14，模板 = `py-counter-4` 的交付目录，`run-task-local.py --template`，公开测试 2 条）**

| 运行 | 路径 | 请求 | prompt | completion | 耗时 s | 公开测试 | 路径是否正确 |
|---|---|---:|---:|---:|---:|---|---|
| py-evo-1 | Python | 1 | 1,054 | 181 | 18 | 2/2 | unchanged [REQ-1]，to implement [REQ-2] |
| py-evo-2 | Python | 1 | 1,054 | 181 | 18 | 2/2 | 同上 |
| rs-evo-1 | Rust（修前） | 1 | 1,088 | 178 | 29 | 2/2 | **错**：unchanged {REQ-1, REQ-2}，REQ-2 被当成「未变节点的回归」修了一轮 |
| rs-evo-2 | Rust（修前） | 1 | 1,088 | 178 | 29 | 2/2 | 同上 |
| rs-evo-dry | Rust（修后，`--dry-run`） | 0 | 0 | 0 | — | 1/2（REQ-2 无模型） | **对**：unchanged {REQ-1}，to implement [REQ-2]；REQ-1 回归 1/1 |

- 修前结果表面上与 Python 一致（2/2、1 次请求、prompt 相差 34 token），但走错了路：胶水在起内核之前调用 `store_requirement_tree(tree)`，把新树写进了追溯表，内核随后读到的「上一轮的表」就是本轮的树，于是每个节点都「未变」；REQ-2 的 spec 跑出 0/1 被当成回归，走的是回归修复提示而不是实现提示。Python 的 `main.py` 是先读旧表再存新树。修法是先快照旧表；`--dry-run` 已验证路径正确。修后的真实运行（目标 2/2、1 次请求）等 key 窗口结束后补跑。
- 通用性自查：本节没有新增规则；快照只是顺序修正。
- 待收编的 Python 新策略见上文「main 在分支点之后新增的 Python 策略」表：N1–N3、N5 已在 M3 分支落地（下），N6 归 M4。

**M3 分支（`wf-kernel-harness-m3`，叠在 #84 上）追加**：

- 顺序修正收紧：胶水总是写 `.arc/previous-requirements.json`（模板没有表就写 `{}`），内核有快照路径时只读快照。此前模板没有表时会回退去读运行时刚覆盖的表，脚手架模板场景下所有节点都被判「未变」（dry-run 首次复现：unchanged {REQ-1, REQ-2}，没有探测、没有弃用）。
- N2 弃用模板、N3 档位按 spec 大小、N5 tiny 档、N1 NAV 去重推导，以及三份契约提示词与 `main.py` 常量逐字节对齐（round 29 通用性复查后的文本）。`--dry-run` 的回复改成与 Python `DryRunDriver` 一致（codegen 提示返回占位文件块、tiny 提示返回裸页面），结构对等运行能走完 tiny → 回退 → 修复。
- dry-run 证据（无模型、不用 key）：
  - `rs-counter-dry`：REQ-1 implement (tiny) 写出 index.html → tiny 档 0/1 → compact codegen → 重写 → 修复循环 → 全套两轮，退出 0。
  - `rs-evo-dry2`（真实模板 py-counter-4）：unchanged {REQ-1}，probe REQ-2 0/1，regression REQ-1 1/1，REQ-2 走 tiny(evolution，引用当前页面) → 回退 compact。
  - `rs-evo-scaffold-dry`（占位脚手架模板：欢迎页 + 静态 server）：unchanged {}，probe REQ-1 0/1、REQ-2 0/1 → 「existing app passes no spec; moved [frontend, backend] to .arc/template-discarded and building fresh」→ fresh build（2 节点树骨架折进首节点）→ REQ-1 tiny → 回退。
- 通用性自查：弃用模板只看探测结果；档位与 tiny 的阈值来自 spec 字符数；tiny 静态 server 无任务逻辑、端口来自 spec；NAV 去重的 href 从服务端源码推导；无任务名 / REQ 编号分支。
- 真实模型对等（Evolution 2/2、1 次请求；Smoke 两题 tiny 档 token）等 key 窗口结束后与 Python 同题同配置各跑 2 次再补。

### M4：reap + cgroup worker 选择 + 全局护栏，keep 走通（PR 待编号；分支 `wf-kernel-harness-m4`，叠在 M3 上）

**改动位置**：`crates/octos-arc/src/reap.rs`（新：`report`、`sweep_all`、`sweep_workspace`、`select_victims`/`descendants_of` 纯函数、`PortWatchdog`）、`flow.rs`（每节点后回收、看门狗起停、收尾报告与回收、`note_turn` 护栏、降级分支：节点循环不修复、全套 0 轮、演练不修复）、`policy.rs` / `arc-policy.toml`（`budget.max_total_tokens`、`budget.max_total_turns`，默认 0=关）、`acceptance.rs`（进程辅助函数对 crate 可见）。cgroup worker 选择在 M1 已落地（`container_memory_limit` + `workers_for_memory`），本里程碑核对无改动。

**护栏的设计**：与 A 在 #81 落地的费用护栏同名同义（`OCTOS_ARC_MAX_TOTAL_TOKENS` / `OCTOS_ARC_MAX_TURNS` / `OCTOS_ARC_MAX_TOTAL_TOKENS_ABS`）：树已知后推导默认值（tokens max(6M, 2.5M×节点)、turns max(24, 4×节点)，按 keep 标定 ≈3× 健康运行），每个模型回合后核对账本的 `total_tokens` 与回合数，超限一次性切到「不再修复、余下节点各一次实现、全套一轮、演练不修复」并记 `guardrail` 事件；0 关闭。规则输入只有账本与节点数。

**keep 结构对等（dry-run，无模型、不用 key）**：Python 与 Rust 都在骨架轮中止（dry-run 的 tool 回合写不出 frontend/backend）：design running/completed 各 13、implement running 13、test failed 45、runner running/failed 各 1，signal 88 对 89；Rust 6 s，Python 124 s。中止路径原来多发 13 条 FOLDER implement/completed，已对齐。护栏 dry-run（Counter，`OCTOS_ARC_MAX_TOTAL_TURNS=2`）：两回合后触发降级，REQ-1 不修复、全套一轮、演练通过，`guardrail` 事件 1 条。

**通用性自查**：回收与看门狗按进程归属（cwd / 命令行 / 父子关系）判断；护栏阈值来自账本；无任务名 / REQ 编号分支。Python 的收尾回收按命令行关键词杀全机 chrom/node/npm/octos serve，在共享宿主（本机就有用户自己的 Chrome 与 `octos serve`）会误杀——Rust 版只杀能归属本次运行的进程，这是有意的行为差异，云端容器里两者等价。

**深 dry-run（Rust 独有开关 `debug.dry_run_tool_files` / `OCTOS_ARC_DRYRUN_FILES=1`，tool 回合写占位应用而不是只回一句话）**：keep 32 节点全程走完，543 s，退出 0：骨架轮写出占位应用 → 32 个节点各 round 0 + 2 次修复（同一失败即停）→ 全套 0/32 两轮（同一失败集合即停）→ 演练通过 → 45 个需求（32 ATOMIC + 13 FOLDER）design/implement running+completed 各 45、test failed 77（32 节点在节点循环与收尾各标一次，Python 同）→ 收尾回收报告。这是 M4 在没有模型时能做到的最完整结构验证；Python 的 dry-run 在 tool 模式不写文件，所以这项没有 Python 对照。云端基准（C，2026-09-14）：keep 32/32、¥16.58、9,038 s、1,152 次请求、缓存命中 91%。

**未做**：keep 的真实运行（32/32 或与 Python 持平）等 key 窗口结束；届时先估费用（Python 路径 keep 云端 ¥16.58，本机单次上限 ¥5 意味着本机不能整跑 keep，只能云端由 C 跑或本机 `--set repair.rounds=…` 缩短——需统筹决定）。

### 通用性自查总表（0.1 节；统筹 2026-09-14 补充要求后的复查）

复查方法：`grep` Rust 源码（非测试代码）里的任务名 / REQ 编号 / 中文题目文本，`grep` `arc/prompts/*.md` 里的枚举例子与具体参数，逐条核对目标书 0.1 列出的四处。

| 项 | 现状 | 从哪个运行时输入推导 | 对哪些题生效 |
|---|---|---|---|
| 「Live indicators (password-strength meters, counters, previews)」 | 已是「any element the spec reads back after typing」（A 轮 29 文本，`ui-contract-core.md`） | spec 文本（被读回的元素） | 任何带实时反馈的页面 |
| scryptSync 具体参数 | 已是「每请求 CPU 预算 30 ms，哈希每次几毫秒，不用默认代价 KDF / 原生模块」（`performance-contract.md`） | 容器事实（慢 CPU、并行 4 浏览器） | 任何有密码的题 |
| 「counter at -1 seed」 | Rust 注释改为「测试会改动持久化状态；只有需求要求跨会话保持的数据才可能被提交，所以每次测试后还原工作树」（`flow.rs`），行为本身（`snapshot_worktree` / `restore_worktree`）对任何题一样 | 需求文本（是否要求持久化） | 所有题 |
| 关键词裁剪的契约块 | 触发词在 `arc/arc-policy.toml` 的 `[prompts] session_keywords / data_keywords`（可改，不用编译）；Rust 里的默认值只是同一份的内置镜像，文件优先 | 需求树文本 | 有账号/会话或列出选项与预置数据的题 |
| 本次复查另改的提示词例子 | `ui-contract-core.md`「the count is initially 0」「the element contains \`0\`」→「需求说明加载即显示的值」；去掉云端运行号；`ui-contract-data.md`「nationalities, seat classes」→「enumerated categories」；`ui-contract-session.md`「"Sign out" link」→「测试期望文本的退出链接」；`corrections.md`「a counter that every browser session shares」→「a value that every browser session shares」 | — | 这些句子相对 Python 常量是有意差异，Python 侧由 A 决定是否同步 |
| 任务名 / REQ 编号分支 | Rust 非测试代码无；提示词无 | — | — |

本工作流新增（Python 没有）的规则与阈值：

| 规则/阈值 | 默认 | 推导输入 | 生效范围 |
|---|---|---|---|
| L16 codegen 回复无文件块重试一次 | 固定 1 次 | 回复文本 | 所有 codegen 模式（≤2 节点）的题 |
| L17 全套修复轮保留最优、结束回滚 | — | 全套验收通过数 | 所有多 spec 的题 |
| `budget.max_total_tokens` / `max_total_turns` 护栏 | 0（关） | 本次运行账本 | 开启后所有题同一条降级规则 |
| `reap::sweep_workspace` 每节点回收 | — | 进程 cwd 在工作目录内 | 所有题 |
| `reap::sweep_all` 只杀可归属进程 | — | cwd / 命令行 / 父子关系 | 所有题（Python 版按关键词杀全机，是有意差异） |
| `PortWatchdog` 5 s | 5 s（Python 同） | 评测端口 + 进程 cwd | 所有题 |
| tiny 档 `mode.tiny_spec_chars` 1500、档位 `codegen_reasoning_chars` 5000 | 与 Python 同 | spec 字符数 | Smoke、Evolution、任何小 spec 的节点 |
| 弃用模板 | — | 探测结果（探测过且 0 节点通过） | 所有 Evolution 题 |

**轮 35 对 M3/M4 的影响**：从 main@34f9d7e2 起 Python 对 keep 这类多节点树也走每节点单请求 codegen（不再是 tool 模式 + 骨架轮），M4 的对照基准因此从「tool 模式 1,152 请求」变成 A 下一空窗要跑的「每节点单请求」新版本；Rust 已同步（keep dry-run：骨架跳过、32 个节点各一次 codegen 请求、全套 + 演练、退出 0，见下）。tool 模式仍是每节点的兜底（spec 超预算、codegen 修复两次后、同一失败两次）。

**轮 35 之后的 keep dry-run 对照（2026-09-14，同配置各一次）**：结构完全一致——runner-events 分布 design running/completed 45/45、implement running/completed 45/45、test failed 77、runner 1/1、signal 295 对 295；每节点轮次相同（32 个 round 0，11 个节点各 2 次修复，其余受本机 3600 s 预算的 240 s 节点下限限制不修复）；Rust 450 s、Python 418 s，两边都是骨架跳过 → 32 次 codegen → 全套两轮 → 演练通过 → 退出 0。TB 与 Evolution 的 dry-run 同样退出 0。

### 第三阶段 §1.1：N 节点树每节点单请求 codegen（Rust 引擎；规则以 `arc/arc-policy.toml` 为准）

统筹 2026-09-14 的优先级调整：把两条路径（单请求 codegen / 内核 tool 会话）的选择改成按节点判定。Rust 侧在 #88（ac9adb85、14b85ad8）落地，与 A 的轮 35 同一规则：

| 判定 | 输入 | 策略键 | 结果 |
|---|---|---|---|
| tiny 档 | 该节点 spec 字符数 < 1,500 | `mode.tiny`、`mode.tiny_spec_chars` | 只含 spec 语句的提示，回一页标记，harness 写静态 server；specs 不过进下一档 |
| 单请求 codegen | spec 字符数 < 60% × 90,000 | `mode.codegen`、`mode.codegen_max_nodes`(999)、`prompts.codegen_context_chars` | 提示 = spec + `relevant_sources`（后端入口优先，其余页面按 spec 词项命中数排序，预算 = 90,000 − spec，其余只列名）+ 格式段；一次输出整文件；spec < 5,000 字符时 thinking off + 紧凑规则（`reasoning.codegen_reasoning_chars`） |
| tool 模式 | spec 超预算，或该节点 codegen 已被阻断 | — | 内核 stdio 会话回合（NODE 提示） |
| 失败处理 | round 0 = 0 → 一次 codegen 重写；之后 ≤2 次 codegen 修复；同一失败两次或修复用尽 → tool 模式修复 | `repair.rewrite_on_zero`、`repair.codegen_repairs`、`repair.rounds` / `rounds_large_tree`、`stall_limit` | `codegen_repairs = 0` 即「首轮失败直接回退 tool 模式」 |
| 骨架轮 | codegen 模式且非 Evolution | `mode.skeleton_always` | harness manifests 取代骨架轮 |

- 与树大小无关：`codegen_max_nodes` 只是上限开关（999），选择只看节点的 spec 大小与现有源码；源码引用按 spec 词项重叠排序、按预算裁剪。
- 目标：每节点 ≤5 次请求、≤¥0.2。codegen 路径每节点最多 1 + 1 + 2 = 4 次单请求；tool 模式修复每回合最多 `requests.repair`(10) 次调用，是超出目标的唯一来源，靠「同一失败两次才切」与费用护栏兜住。
- dry-run 证据：keep 32 节点 Rust 与 Python 事件分布逐项相同（上文）；TB 两条路径同样逐项相同（design/implement running+completed 各 5、test failed 9、signal 37，且都在「同一失败两次」处切到 tool 模式）；Evolution 退出 0。
- 费用估算（A 的离线量：keep 工作区 5 个源文件 86k 字符，REQ-2.5.2 引用 ≈72k 字符 ≈21k token）：每请求输入 ≈20–26k token（平台拟合 ≈¥2–3/M → ≈¥0.05–0.08），每节点 1–3 次 → ≈¥0.1–0.2，整题 keep ≈¥3–6 对旧基准 ¥16.58。
- 真跑对等（空窗）：keep 一题 Rust 路径 1 次（云端，需要 arc.12 Release 与 `OCTOS_ARC_ENGINE=rust`），与 A 同版本 Python 的 keep 对照；预计 1–3M token。

### Release `v2.0.3-rc.11-arc.12`（Rust 引擎的 Linux 二进制；`arc/main.py` 默认地址与 `arc-runtime-lock.json` 不动）

统筹从 main@6e2065c7（#88 合入）触发 `arc-linux-release.yml`；首次触发因 checkout 不接受 8 位短 SHA 失败（`A branch or tag with the name '6e2065c7' could not be found`），用完整 SHA 重新触发后成功（run 34848583262）。本机核对（2026-09-14）：

| 项 | 值 |
|---|---|
| 地址 | https://github.com/octos-org/octos-arc/releases/download/v2.0.3-rc.11-arc.12/octos-bundle-x86_64-unknown-linux-gnu.tar.gz |
| archive sha256（本机重算 = 发布的 bundle.sha256） | `9a60e32a38687b40f5c08e4b4428265077bae4e70381b2fded2737c621552903` |
| binary sha256（解出的 `octos` = 发布的 octos.sha256） | `1855f612d7b5155dcd3c99e9a002a38a7737bdace0c2d1359d62f6dc18d1c0d2` |
| source_commit | `6e2065c76e09485af3b334b3fa45413fe1cdf5ce` |
| binary_version | `octos 2.0.3-rc.11 (6e2065c7 2026-09-14)`，rustc 1.98.0 |
| 包内 | octos、octos-sandbox、model_catalog.json 与随包技能二进制；`octos` 内含 `arc run` / `deny-protected` 子命令 |

云端让某次运行走 Rust 引擎：运行环境加 `OCTOS_ARC_ENGINE=rust` 和 `OCTOS_RELEASE_URL=<上面的地址>`（胶水 `_download_octos` 读这个变量；默认地址仍是 arc.11），bundle 用 main 上的 `arc/pack.sh` 打包（含 `rust_engine.py`、`arc-policy.toml`、`prompts/`）。M5 之前不改默认地址与 lock。
