#!/usr/bin/env bash
# 用途: 图生图(image2image), 以一张本地图为基础按提示词改写。
#   --input 可填本地路径(会读字节 base64 内联)或 http(s) URL。
# 支持 i2i 的 provider: jimeng(即梦)/ volcengine(火山 Seedream)/ google(Gemini Nano Banana)。
# 需要的 key 环境变量(按所选 provider 二选一):
#   volcengine: ARK_API_KEY (或 VOLC_API_KEY / IMAGECLI_VOLC_KEY)
#   google:     GEMINI_API_KEY (或 GOOGLE_API_KEY / IMAGECLI_GOOGLE_KEY)
#   jimeng:     JIMENG_ACCESS_KEY + JIMENG_SECRET_KEY (火山 AK/SK V4 签名, 需两个)
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入。
set -euo pipefail

# 待改写的本地图路径(换成你自己的图)。
INPUT="${INPUT:-./local.png}"
: "${ARK_API_KEY:?请先 export ARK_API_KEY=你的火山方舟key (本例用 volcengine)}"

# 用火山 Seedream 做图生图: capability 显式指定 image2image, model 省略走该 provider 默认。
imagecli generate \
  --provider volcengine \
  --capability image2image \
  --input "$INPUT" \
  --prompt "把这张图改成赛博朋克霓虹夜景风格"

# 换 google(Gemini)做图生图(需 GEMINI_API_KEY):
#   imagecli generate --provider google --capability image2image \
#     --input "$INPUT" --prompt "turn this into a watercolor painting"
