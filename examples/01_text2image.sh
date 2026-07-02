#!/usr/bin/env bash
# 用途: 用 agnes 免费层文生图, 零成本上手 imagecli 的第一张图。
# 需要的 key 环境变量: AGNES_API_KEY (或回退 IMAGECLI_AGNES_KEY)。
#   agnes 是新加坡 Agnes AI 免费层(约 30 RPM), 不指定 provider 时也是内置默认。
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入(export 或运行时前缀)。
set -euo pipefail

# 若尚未在 shell 里 export, 可临时前缀注入(下面这行仅示意, 请换成你自己的 key):
#   AGNES_API_KEY=你的key ./01_text2image.sh
: "${AGNES_API_KEY:?请先 export AGNES_API_KEY=你的key}"

# 最小文生图: capability 默认就是 text2image, 可省; 产物默认下载到 ./out。
imagecli generate \
  --provider agnes \
  --prompt "a red fox sitting in fresh snow, soft morning light"

# 想要机器可解析输出时加全局 --json(放在 generate 前):
#   imagecli --json generate --provider agnes --prompt "a red fox in snow"
