#!/usr/bin/env bash
# 用途: 超分(upscale), 把一张图放大到更高清。
# 支持 upscale 的 provider: fal(clarity-upscaler) / replicate(real-esrgan)。
# 需要的 key 环境变量(按所选 provider):
#   fal:       FAL_KEY (或 IMAGECLI_FAL_KEY)
#   replicate: REPLICATE_API_TOKEN (或 IMAGECLI_REPLICATE_KEY)
# 注意: fal/replicate 暂不支持本地图自动上传, 输入建议用 http(s) URL。
#   缩放参数名各家不同: fal 用 upscale_factor, replicate 用 scale。
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入。
set -euo pipefail

# 待放大的图 URL(换成你自己的可公开访问 URL)。
IMG_URL="${IMG_URL:-https://example.com/small.png}"
: "${FAL_KEY:?请先 export FAL_KEY=你的fal key (本例用 fal)}"

# fal 超分: 输入图走 --input(URL), 放大倍数用 --param upscale_factor。
imagecli generate \
  --provider fal \
  --capability upscale \
  --input "$IMG_URL" \
  --param upscale_factor=2

# 换 replicate(real-esrgan, 需 REPLICATE_API_TOKEN): 缩放参数名是 scale。
#   imagecli generate --provider replicate --capability upscale \
#     --input "$IMG_URL" --param scale=2
