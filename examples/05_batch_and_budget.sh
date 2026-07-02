#!/usr/bin/env bash
# 用途: 批量生成 + 预算护栏。展示三件事:
#   1) 多个 --prompt (或 --prompts-file) fan-out 成多个任务并发跑;
#   2) --dry-run 只预估任务数与成本合计, 不调用任何 provider、不消耗额度;
#   3) --max-cost 超预算直接拒绝执行(非零退出), 防止批量误烧额度。
# 需要的 key 环境变量: AGNES_API_KEY (本例用免费 agnes)。
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入。
set -euo pipefail

: "${AGNES_API_KEY:?请先 export AGNES_API_KEY=你的key}"

# 准备一个 prompts 文件(一行一个, 空行与 # 注释行忽略)。
PROMPTS_FILE="$(mktemp)"
cat > "$PROMPTS_FILE" <<'EOF'
# 每行一个 prompt, 本行是注释会被忽略
a serene mountain lake at dawn
a neon-lit tokyo street in the rain
a cozy bookstore cafe in autumn
EOF

# 1) 先 dry-run 预估: 打印将提交的任务数与预估成本合计, 不真跑。
imagecli generate \
  --provider agnes \
  --prompts-file "$PROMPTS_FILE" \
  --prompt "an extra one-off prompt from the command line" \
  --dry-run

# 2) 加预算护栏真跑: 预估超过 --max-cost(USD) 就拒绝执行。并发上限 --concurrency。
imagecli generate \
  --provider agnes \
  --prompts-file "$PROMPTS_FILE" \
  --max-cost 1.00 \
  --concurrency 3

rm -f "$PROMPTS_FILE"
