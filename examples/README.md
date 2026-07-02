# examples

一组可直接跑的 imagecli 用法示例脚本。每个脚本头部有中文注释, 说明用途与需要的 key 环境变量。

前置: 先构建并把 `imagecli` 放进 PATH(见项目根 README 的"安装"), 再按脚本注释 export 对应 provider 的 key。
所有脚本都用环境变量注入 key, 不内嵌任何真实 key。

| 脚本 | 演示 | 需要的 key |
| ---- | ---- | ---------- |
| `01_text2image.sh` | agnes 免费文生图(零成本上手) | `AGNES_API_KEY` |
| `02_image2image.sh` | 本地图/URL 图生图(volcengine/google/jimeng) | 见脚本内注释 |
| `03_text2video.sh` | 文生视频(seedance/kling/fal) | 见脚本内注释 |
| `04_upscale.sh` | 超分放大(fal upscale_factor / replicate scale) | `FAL_KEY` 或 `REPLICATE_API_TOKEN` |
| `05_batch_and_budget.sh` | 批量 + `--dry-run` 预估 + `--max-cost` 护栏 | `AGNES_API_KEY` |
| `06_fallback.sh` | 主 provider 失败自动切备用 | `GEMINI_API_KEY` + `AGNES_API_KEY` |
| `07_model_select.sh` | /model 式选择器: 列表 / 直设 / 交互 | 无(仅写配置) |
| `08_mcp.sh` | 启动 MCP server + initialize/tools/list JSON-RPC 调用 | 无(握手) |

跑法示例:

```bash
chmod +x examples/*.sh
AGNES_API_KEY=你的key ./examples/01_text2image.sh
```
