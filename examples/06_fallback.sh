#!/usr/bin/env bash
# 用途: 故障转移(fallback)。主 provider 因限流/配额/网络失败时, 自动按候选链切到备用 provider。
#   每个 prompt 各自独立走 "主 + fallback" 候选链(与批量 fan-out 正交)。
#   --json 里会记 provider_used / fallback_from / attempts 供观测。
# 需要的 key 环境变量(主 + 备各自的 key):
#   google: GEMINI_API_KEY (或 GOOGLE_API_KEY / IMAGECLI_GOOGLE_KEY)
#   agnes:  AGNES_API_KEY (免费层, 适合当兜底备用)
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入。
set -euo pipefail

: "${AGNES_API_KEY:?请先 export AGNES_API_KEY=你的key (作为 fallback 备用)}"

# 主 google, 失败自动切 agnes; --retries 控制可重试错误(429/5xx/超时)的每 provider 重试次数;
# --verbose 把 submit/poll/fallback/重试事件打到 stderr(不污染 --json 的 stdout)。
imagecli generate \
  --provider google \
  --fallback agnes \
  --retries 2 \
  --verbose \
  --prompt "a lighthouse on a stormy cliff at dusk"

# 多个备用可逗号分隔或重复 --fallback:
#   --fallback agnes,replicate   或   --fallback agnes --fallback replicate
