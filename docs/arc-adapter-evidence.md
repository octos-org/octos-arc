# 适配层给内核组的证据（工作流 A）

## DeepSeek 前缀缓存命中（更正 2026-09-13）

此前适配层报告「cache_hit 恒为 0」是记录错误：arc-bench 端点（`https://api.arc-bench.com/v1`）按 OpenAI 风格在
`usage.prompt_tokens_details.cached_tokens` 报告命中，而 `arc/llm_proxy.py` 只读 DeepSeek 风格的 `prompt_cache_hit_tokens`。

- 直接探测：同一 4,014 token 的 system 前缀连发三次（`thinking: disabled`，max_tokens 20）：`cached_tokens` = 0 / 2,048 / 3,840。
- 真实运行（arc.11 内核 `b2134836`，本机 `arc/arc-output/w3-counter/.arc/llm-usage.jsonl`）：两次请求 prompt 3,192 / 4,906，第二次 `cached_tokens` = 2,048。

结论与 B 的「同会话样本约 94%」不冲突。适配层默认每轮新会话，命中主要来自 system prompt + 工具 schema 的固定前缀。

## 每次请求的固定前缀（arc.10 内核，`arc/arc-output/v4-counter/.arc/llm-requests/request-01.json`）

system prompt 24,978 字符 + 15 个工具 schema 13,735 字符；适配层代理在传输中裁掉与 ARC 无关的段落与工具（`arc/llm_proxy.py` 的 `DROP_SECTIONS` / `DROP_TOOLS`），一次请求 44,676 → 15,504 字节。arc.11 已在内核侧精简（Reply OK 5,420 Token）；代理裁剪对不含这些标题的 system prompt 是空操作。

## 输出上限

arc.11 给 DeepSeek ARC 端点设 `max_tokens` 4096。一个需求节点的文件合计 10–25k 输出 token，云端 76fb32a69d81 两个实现轮均 `output_truncated`。适配层代理把每个请求的 `max_tokens` 抬到 ≥ 32,768（`OCTOS_ARC_MAX_TOKENS`）；建议内核默认值对 coding 场景不低于 16k。
