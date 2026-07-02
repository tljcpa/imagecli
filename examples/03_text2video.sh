#!/usr/bin/env bash
# 用途: 文生视频(text2video), 一句提示词生成一段短视频。
# 支持 t2v 的 provider: fal / seedance(火山方舟) / kling(可灵) / replicate。
# 需要的 key 环境变量(按所选 provider):
#   fal:      FAL_KEY (或 IMAGECLI_FAL_KEY)
#   seedance: ARK_API_KEY (或 IMAGECLI_ARK_KEY / IMAGECLI_SEEDANCE_KEY)
#   kling:    KLING_ACCESS_KEY + KLING_SECRET_KEY (本地 HS256 JWT, 需两个)
# 注意: 视频生成耗时较长且消耗额度; 先生成一条确认再批量。
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入。
set -euo pipefail

: "${ARK_API_KEY:?请先 export ARK_API_KEY=你的火山方舟key (本例用 seedance)}"

# seedance 文生视频: duration 走 --param 透传(seedance 是整数秒)。
imagecli generate \
  --provider seedance \
  --capability text2video \
  --prompt "a cat surfing on a big ocean wave, cinematic" \
  --param duration=5

# 换 kling(可灵, 需 AK+SK): 注意可灵 duration 是字符串 "5"/"10", aspect_ratio 走 --param。
#   imagecli generate --provider kling --capability text2video \
#     --prompt "a cat surfing" --param 'duration="5"' --param 'aspect_ratio="16:9"'

# 换 fal(需 FAL_KEY), model 省略走 fal 默认 t2v endpoint:
#   imagecli generate --provider fal --capability text2video \
#     --prompt "a cat surfing" --param duration=5
