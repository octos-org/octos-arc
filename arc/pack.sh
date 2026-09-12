#!/bin/sh
# 把 arc/ 打成 ARC 平台要的提交包（main.py 必须在 zip 根目录）
set -e
cd "$(dirname "$0")"
rm -f ../octos-arc-bundle.zip
zip -qr ../octos-arc-bundle.zip main.py octos_stdio.py requirements.txt arcbench_agent_runtime public-tests -x '*/__pycache__/*' '*.pyc'
echo "打包完成：$(cd .. && pwd)/octos-arc-bundle.zip"
shasum -a 256 ../octos-arc-bundle.zip
