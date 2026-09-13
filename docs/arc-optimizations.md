# ARC-Bench 内核优化记录

当前交付分支：`wf-kernel`（第一轮运行记录保留原始 `arc-opt` 证据）。所有数字均来自本机事件流、测试输出或 GitHub Actions 产物；评测分数与单元测试结果分开记录。

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

新增唯一的 ARC 专用 `workflow_dispatch` 工作流。它按输入的精确 ref 构建 `x86_64-unknown-linux-gnu` runtime 和 bundled tools，生成 `octos-bundle-x86_64-unknown-linux-gnu.tar.gz`，并在 Release 中上传 bundle SHA-256、`octos` 二进制 SHA-256、源码提交、rustc 和版本信息。工作流已在 `origin/main` 注册并成功运行 `34741413085`，创建了 [v2.0.3-rc.11-arc.2](https://github.com/octos-org/octos-arc/releases/tag/v2.0.3-rc.11-arc.2)。源码提交为 `78394e5032cfe0ed8387c0b226250c229d9fbfa8`，rustc 为 `1.98.0 (88d9e12ae 2026-08-18)`；bundle SHA-256 为 `9b2f8a34831148a4650e9f91b862fec0e0bf1f9ea3bc965573e60f23cd0d4f2d`，`octos` SHA-256 为 `bca911a3690be992da01b0d116ab21064c673d65303aa1b79bc16d22c3a00afb`。下载后使用两份发布的 checksum 文件核验均为 `OK`，并确认归档内 `octos` 是 Linux x86-64 ELF；`arc-runtime-lock.json.runtime_release` 已回填真实值。

### B2：DeepSeek 费用异常

改动位置：`crates/octos-llm/src/openai.rs`、`pricing.rs`、`crates/octos-core/src/ui_protocol.rs`、`crates/octos-cli/src/api/ui_protocol_transport.rs`。

OpenAI 兼容响应现在解析 DeepSeek 顶层 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`，并将其归一化为不重复计入的 input/cache-read 口径；DeepSeek cache hit 使用 0.1 的输入价格折扣。`turn/completed` 同时暴露 `cache_hit`，便于将计费数据与请求数据逐轮核对。新增 8 个 `octos-llm` cache/pricing 测试及 CLI 完成事件回归测试；尚未再次调用付费 ARC 任务，因此本轮没有声称线上费用下降，实际评测应标记为“未评测”。

本轮本地 Counter 运行 `wf-kernel-counter`（事件流：`/Users/mac/Desktop/octos-official-demo/arc-output/wf-kernel-counter/.arc/octos-events.jsonl`）为 3 轮、37,691 input、22,556 output、658,176 cache-hit tokens，累计费用 0.10373706；`grade-local.py` 公开测试为 1/1。费用异常修复的 provider 真实线上计费仍未评测，以上是新内核本地回归的可复核数据。

### B3：每轮与每节点预算

改动位置：`crates/octos-cli/src/config.rs`、`commands/gateway/gateway_runtime.rs`、`runtime/session.rs`；节点执行逻辑沿用 `crates/octos-arc/src/runner.rs` 的 `--node-budget-seconds` 和 `--node-token-budget`。

serve/stdio 会话新增可选 `[gateway].token_budget`，映射到 agent 的整轮总 Token 上限（包含缓存 Token）；`session_timeout_secs` 提供整轮时间上限。`octos arc` 仍按依赖拓扑逐节点设置 Token/时间预算，节点耗尽时记录 `skipped_budget`，保留此前完成的节点。Agent 的超限返回是失败结果，包含 `budget_exhausted`，不会被当成成功回合继续空转。配置解析和 session bootstrap 回归测试已通过。

### B5：容器实测

本机 `docker` 命令不可用，未把容器实测写成已完成。可在 Ubuntu runner 或 ARC 容器中执行以下无特权复核：

```bash
docker run --rm --security-opt=no-new-privileges --cap-drop=ALL \
  -v "$PWD":/src -w /src rust:1.98.0-bookworm \
  bash -lc 'cargo build --locked -p octos-cli --no-default-features --features api'
docker run --rm --security-opt=no-new-privileges --cap-drop=ALL \
  -v "$PWD":/src -w /src rust:1.98.0-bookworm \
  bash -lc '/src/target/debug/octos --version'
```

随后向 `serve --stdio --solo` 发送一次 `shell`/`exec` 请求，记录容器标记、sandbox warning 和结构化工具结果；预期是明确降级执行，或错误中包含 `sandbox denied` 及 `--danger-full-access` 建议。

## P0-0：stdio/solo 提示与工具面瘦身

改动位置：`crates/octos-cli/src/commands/serve.rs`、`runtime/profile.rs`、`api/ui_protocol_transport.rs`。

`serve --stdio --solo` 启动时默认启用 coding profile，严格保留 12 个编码工具；bundled app-skills/platform-skills 不再自动 bootstrap。空 memory 不生成策略段或 `memory-snapshot`，无 active goal 不生成 `session-goal-snapshot`。panes 树仍只属于 `session/open` 的协议返回，不会追加到模型历史。

验证：CLI 单测 `p0_0_tests`、profile 白名单单测；真实 arm64 二进制用同一 API 配置和同一句 `Reply OK` 运行，得到 5,714 输入 Token、12 工具、成功回复 `OK`。

## P0-1：容器沙箱显式降级

改动位置：`crates/octos-agent/src/sandbox/mod.rs`。

启动时检测 `/.dockerenv` 及 docker/containerd/kubepods/podman/libpod cgroup 标记；容器中无可用隔离后端时记录明确 warning 并按容器策略降级，且不会把仅检测到的 Docker CLI 当成可用的嵌套隔离。探测函数保持纯函数便于测试。新增 dockerenv、cgroup、误判排除和 nested-Docker 降级测试；`sandbox::tests` 44/44 通过。

本机未运行 Docker 容器验证：本机 Docker 不可用，因此未声称完成 Linux 容器实测。可在 Linux 主机或 ARC 容器中按以下步骤复核：

```bash
docker run --rm --security-opt=no-new-privileges --cap-drop=ALL \
  -v "$PWD":/src -w /src rust:1.98.0-bookworm \
  bash -lc 'cargo build --locked -p octos-cli --no-default-features --features api'
docker run --rm --security-opt=no-new-privileges --cap-drop=ALL \
  -v "$PWD":/src -w /src rust:1.98.0-bookworm \
  bash -lc 'test -f /.dockerenv; cat /proc/1/cgroup; \
    /src/target/debug/octos --version'
```

在第二个容器内再驱动一次 `serve --stdio --solo` 并执行 `shell`/`exec`，应看到容器标记和明确的降级 warning；若隔离后端仍不可用，结果必须是无沙箱执行或包含 `sandbox denied` 与 `--danger-full-access` 建议的结构化错误。

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

`runtime_release` 仍为 `null`，因为没有 GitHub Release；锁文件的 `build` 只记录真实产物元数据。

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

第二轮提交：B1 `e3bc8242` + `d12c15ad`；B2 `5e6ca223`；B3 `d9fba010`；B5 文档与锁文件 `a10521e4`。
