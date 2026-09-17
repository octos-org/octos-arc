import argparse
import json
import os
from pathlib import Path
import socket
import sys
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("task", nargs="?"); parser.add_argument("--check", action="store_true"); parser.add_argument("--name", default=None)
arguments = parser.parse_args()
root = Path(__file__).resolve().parent
adapter = root
import shutil
_cands = [os.environ.get("OCTOS_BIN"), root.parent / "target" / "release" / "octos", root.parent / "target" / "debug" / "octos", shutil.which("octos")]
binary = next((Path(c).resolve() for c in _cands if c and Path(c).is_file()), None)
if binary is None:
    sys.exit("找不到 octos 二进制：设 OCTOS_BIN，或先 cargo build --release -p octos-cli --no-default-features --features api")
python = Path(sys.executable)
task = Path(arguments.task).resolve() if arguments.task else root / "arc-counter-task"
key = os.environ.get("ARCBENCH_API_KEY") or os.environ.get("OPENAI_API_KEY")
if not key:
    sys.exit("请设置 ARCBENCH_API_KEY（ARC 平台个人页的 API key）。")
config = {"base_url": os.environ.get("OPENAI_BASE_URL", "https://api.arc-bench.com/v1"), "model": os.environ.get("MODEL", "deepseek-v4-flash")}
for required in (binary, python, task / "requirements.yaml", adapter / "main.py"):
    if not required.is_file():
        sys.exit(f"缺少文件：{required}")
environment = os.environ.copy()
for name in ("OCTOS_HOME", "OCTOS_CONFIG_DIR", "ARCBENCH_TEMPLATE_DIR", "ARCBENCH_TASK_DIR", "OCTOS_INSTANCE_DATA_DIR", "OCTOS_DANGER_FULL_ACCESS"):
    environment.pop(name, None)
environment.update(
    OCTOS_BIN=str(binary),
    OPENAI_API_KEY=key,
    OPENAI_BASE_URL=config["base_url"],
    MODEL=config["model"],
    OCTOS_MODEL=config["model"],
    OCTOS_PROVIDER="custom",
    OCTOS_MAX_ITERATIONS=os.environ.get("OCTOS_MAX_ITERATIONS", "60"),
    OCTOS_NODE_TIMEOUT=os.environ.get("OCTOS_NODE_TIMEOUT", "1200"),
    OCTOS_TIME_BUDGET=os.environ.get("OCTOS_TIME_BUDGET", "2400"),
    OCTOS_SMOKE_PORT="43101",
)
environment["PATH"] = os.environ.get("NODE_BIN", "/opt/homebrew/opt/node@24/bin") + ":" + environment.get("PATH", "")
print(f"运行时：{binary}", flush=True)
print(f"需求：{task / 'requirements.yaml'}", flush=True)
print(f"模型：{config['model']}；密钥：已读取（不显示）", flush=True)
if arguments.check:
    sys.exit(0)
for port in (43100, 43101):
    with socket.socket() as listener:
        try:
            listener.bind(("127.0.0.1", port))
        except OSError:
            sys.exit(f"端口 {port} 已占用，未启动；不会终止已有服务。")
output = root / "arc-output" / (arguments.name or f"{task.name}-{uuid.uuid4().hex[:8]}")
print(f"交付目录：{output}", flush=True)
os.chdir(adapter)
os.execve(str(python), [str(python), "main.py", str(task), "--output-dir", str(output), "--type", "web", "--web-port", "43100"], environment)
