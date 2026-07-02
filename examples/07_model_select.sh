#!/usr/bin/env bash
# 用途: /model 式统一模型选择器。设置默认 provider+model 并持久化到配置,
#   之后 generate 不带 --provider/--model 就用这个默认。
# 需要的 key 环境变量: 无(model 选择只写配置, 不打网络、不消耗额度)。
#   注意: 选中缺 key 的模型会提示, generate 时仍需先配置该 provider 的 key。
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入。
set -euo pipefail

# 1) 只看目录(不改默认): --list 强制只打印, 无 TTY 时也自动降级为打印。
imagecli model --list

# 2) 非交互直设默认: 给 <provider/model> 或 <alias>。
imagecli model agnes/agnes-image-2.1-flash

# 3) 交互菜单(仅在 TTY 下, 方向键选、回车确认、Esc 取消):
#   imagecli model

# 4) 查某 provider 的 model 清单(含估算成本/是否有 key), 也可加 --json:
#   imagecli models --provider fal
#   imagecli model --list --json
