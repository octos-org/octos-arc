<div align="center">

<pre>
 ██████╗  ██████╗████████╗ ██████╗ ███████╗
██╔═══██╗██╔════╝╚══██╔══╝██╔═══██╗██╔════╝
██║   ██║██║        ██║   ██║   ██║███████╗
██║   ██║██║        ██║   ██║   ██║╚════██║
╚██████╔╝╚██████╗   ██║   ╚██████╔╝███████║
 ╚═════╝  ╚═════╝   ╚═╝    ╚═════╝ ╚══════╝
</pre>

</div>

# Octos ARC-Bench 参赛版

**这是 [Octos](https://github.com/octos-org/octos) 参加 ARC-Bench 的比赛版仓库：
固定版本的内核，加上参赛需要的全部外围——平台适配包、公开验收测试、本机做题与打分、
打包上传——都在这一个仓库里。**

参加 ARC-Bench 只需要这一个仓库。

[三步参赛](#三步参赛) · [仓库里有什么](#仓库里有什么) ·
[固定基底](#固定基底) · [内核文档](#内核文档) · [English](README.md)

> **只是想找一个编码 Agent 日常使用？** 这个仓库不是。这里是比赛用的参赛包。
> 日常使用请装 [Octoscode](https://github.com/octos-org/octoscode)，
> 或直接嵌入上游的 [`octos-org/octos`](https://github.com/octos-org/octos) 内核。

---

## 三步参赛

下面是最短路径。完整流程——编排器的每个开关、evolution 模式、模型分档路由，以及
每次改动的改前改后数据——在 **[`arc/README.md`](arc/README.md)**，那是接下来要读的文档。

### 1. 准备（一次）

```sh
pip install -r arc/requirements.txt     # pyyaml、arcbench-runtime
export ARCBENCH_API_KEY=ak_...          # arc-bench.com 个人页的 API key

# 拿一个 octos 二进制：用发布版，或者自己编
export OCTOS_BIN=/path/to/octos         # 不设则用 ../target/release/octos
cargo build --release -p octos-cli --no-default-features --features api
```

### 2. 本机做题，然后打分

跑 smoke 题大约五分钟，花费不到一分钱：

```sh
python3 arc/run-task-local.py arc/tasks/smoke--counter --name try1
python3 arc/grade-local.py arc/arc-output/try1 smoke--counter   # 平台原版 Playwright 测试
python3 arc/metrics.py arc/arc-output/try1                      # 轮数 / Token / 费用 / 耗时
```

然后改一处——`arc/main.py` 里的提示词、`arc/octos_stdio.py` 的启动参数，
或者 `crates/` 里的内核——改完把上面两条再跑一次，比较数字。

### 3. 打包上传

```sh
sh arc/pack.sh                          # 得到 octos-arc-bundle.zip
```

到 arc-bench.com 对应比赛页的 **New submission** 上传。

> **改了 `crates/` 的话，这段必须看。** 平台运行时会按 `arc/main.py` 里的
> `OCTOS_RELEASE_URL` 现场下载 Octos。改了内核之后，必须编译 Linux x86-64 版、
> 在本仓库发一个 Release、把 `OCTOS_RELEASE_URL` 指向那个不可变地址、再重新
> `pack.sh`，改动才会真正生效。否则平台跑的仍然是原版二进制，改了等于没改。

## 仓库里有什么

| 路径 | 是什么 |
| --- | --- |
| [`arc/`](arc/README.md) | **从这里开始。** 适配包、编排器、公开验收测试、本机做题/打分/打包脚本 |
| `arc/tasks/` | 各题需求文件的离线副本（smoke、订票题、六道 web 题） |
| `arc/public-tests/` | 平台公开的 Playwright 验收测试 |
| `crates/` | Octos 内核源码，从固定的上游基底分出 |
| `book/`、`docs/` | 内核文档，按固定基底随仓库带入——见[内核文档](#内核文档) |
| `arc-runtime-lock.json` | 发布与提交的版本约束：源码提交、目标平台、必须记录的 SHA-256 |
| [`ARC_BASELINE.md`](ARC_BASELINE.md) | 固定了什么，以及改动固定值的规则 |

分支：**`main`** 是魔改版全量内核加 `arc/`；**`adapter`** 只有适配包，
给想搭配官方 Octos 或别的 Agent 使用的人。

## 固定基底

这个仓库不跟随上游 `main`，而是固定在：

| | |
| --- | --- |
| 上游仓库 | [`octos-org/octos`](https://github.com/octos-org/octos) |
| 提交 | [`8558a3bf`](https://github.com/octos-org/octos/commit/8558a3bff41f43838130808a1fa6cf0299e0bc40) |
| 标签 | `arc-base-20260910` |

提交能够复现，靠的就是这个固定值，所以出现会移动的引用要当成 bug 处理。
不要把 `latest` 发布地址、分支引用，或 `PATH` 里未经校验的二进制选作比赛运行时。
完整策略和提交魔改运行时之前的步骤，见 [`ARC_BASELINE.md`](ARC_BASELINE.md)。

## 内核文档

`book/` 和 `docs/` 是按固定基底带入的副本，描述的是这个仓库实际编译出的内核，
不是当前的上游 `main`。

- [架构](docs/ARCHITECTURE.md) · [CLI 参考](book/src/cli-reference.md) ·
  [配置](book/src/configuration.md) · [模型提供者](book/src/providers.md)
- [OUP 协议规范](api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md)
- [`CLAUDE.md`](CLAUDE.md) —— 给在这个仓库里干活的 Agent 看的内核架构说明

已经走在固定基底之后的上游文档，请看
[文档站](https://octos-org.github.io/octos/zh/)，或固定提交处的
[上游 README](https://github.com/octos-org/octos/blob/8558a3bff41f43838130808a1fa6cf0299e0bc40/README-zh.md)。
两者说法不一致时，以本仓库带入的副本为准——它描述的才是这里实际跑的东西。

## 许可证

Apache-2.0，见 [LICENSE](LICENSE)。
