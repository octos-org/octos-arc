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

# 3. 用平台原版 Playwright 测试打分（首次会自动装 Playwright）
python3 arc/grade-local.py arc/arc-output/try1 smoke--counter

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
| `main.py` | 平台入口：读需求、驱动 Octos、整理产物、上报进度 |
| `octos_stdio.py` | 通过 `octos serve --stdio` 驱动内核 |
| `public-tests/<题目>/` | 平台公开的 Playwright 验收测试（会自动喂给模型） |
| `tasks/<题目>/` | 各题需求文件的离线副本 |
| `run-task-local.py` / `grade-local.py` / `pack.sh` | 本机做题、打分、打包 |

已知平台细节：容器里 `/workspace/tests` 有验收测试；订票题的测试默认连 3301 端口而平台起在 3000，`main.py` 会要求后端两个端口都监听；容器到 npmjs 很慢，提示词要求零依赖并走 npmmirror。
