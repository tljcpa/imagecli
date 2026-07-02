# imagecli

![CI](https://github.com/tljcpa/imagecli/actions/workflows/ci.yml/badge.svg)

一个通用的、多 provider 的**图像 / 视频**生成命令行工具:一套命令统一调度 13 家有公开 API 的生成模型(fal、Google Gemini、OpenAI、Replicate、火山 Seedream、即梦、可灵、Seedance、智谱、阶跃、SiliconFlow、PPIO、agnes),把"提交—轮询—下载"的异步编排做成统一内核,并做成 agent 可直接调用的 MCP server。

## 为什么做它

以字节即梦官方 CLI(`dreamina`)为**能力对标基线**。即梦 CLI 只能调单一后端,且工程上缺口很多:无批量、无并发、`--poll` 是裸忙等无退避、无重试、无自动下载、无预算护栏、无稳定 `--json` 契约。

imagecli 的差异化**严格限定在工程编排层**(见 `DECISIONS.md` D-006):多 provider 路由与故障转移、批量与有界并发、指数退避重试、自动下载与产物归档、SQLite 跨进程任务持久化、配置原子写防损坏、稳定的 `--json` 与退出码契约、agent-first 的 SKILL.md + MCP server。**模型画质由各家公开 API 提供,本项目不把模型能力作为竞争维度**——职责只有"接得全、接得对"。

## 安装

```bash
# 构建(单二进制,无运行时依赖;SQLite 经 rusqlite bundled 静态链接)
cargo build --release            # 产物 target/release/imagecli

# 装到 PATH(任意目录可敲 imagecli)
sudo cp target/release/imagecli /usr/local/bin/    # 或 cargo install --path .
```

## 快速上手

```bash
# 1. 配 key(按 provider 命名空间从环境变量读;绝不写进项目文件)
export AGNES_API_KEY=你的key       # 示例:agnes 免费层, 零成本上手

# 2. 生成一张图(不指定 provider 时用配置默认;内置默认是免费的 agnes)
imagecli generate --provider agnes --prompt "a red fox in snow"

# 3. 交互式选默认 provider/model(像 Claude Code 的 /model)
imagecli model                    # TTY 下交互菜单; 非 TTY 打印目录
imagecli model agnes/agnes-image-2.1-flash   # 直接设默认

# 4. 生成视频
imagecli generate --provider fal --capability text2video --prompt "a cat surfing" --param duration=5
```

产物默认下载到 `./out`(`--out-dir` 改目录,`--no-download` 跳过)。加 `--json` 拿稳定的机器可解析输出。

## 已支持 providers(13 家)

能力:`t2i`=text2image · `i2i`=image2image · `t2v`=text2video · `i2v`=image2video

| provider | 能力 | 鉴权 | key 环境变量 |
| -------- | ---- | ---- | ------------ |
| **agnes** | t2i | Bearer(OpenAI 兼容) | `AGNES_API_KEY` | 新加坡 Agnes AI 免费层, ~30 RPM |
| **fal** | t2i · t2v · i2v · upscale | Bearer(http-queue 异步) | `FAL_KEY` / `IMAGECLI_FAL_KEY` |
| **google** | t2i · i2i | API key header | `GEMINI_API_KEY` / `GOOGLE_API_KEY` |
| **openai** | t2i | Bearer | `OPENAI_API_KEY` |
| **replicate** | t2i · t2v · i2v · upscale | Bearer(prediction 异步) | `REPLICATE_API_TOKEN` |
| **volcengine** | t2i · i2i | Bearer(方舟 Ark, OpenAI 兼容) | `ARK_API_KEY` | 字节 Seedream, 即梦同源 |
| **jimeng** | t2i · i2i | 火山 AK/SK V4 签名 | `JIMENG_ACCESS_KEY` + `JIMENG_SECRET_KEY` | 即梦 visual API |
| **kling** | t2v · i2v | 本地 HS256 JWT(AK/SK) | `KLING_ACCESS_KEY` + `KLING_SECRET_KEY` | 快手可灵 |
| **seedance** | t2v · i2v | Bearer(方舟 Ark, async-task) | `ARK_API_KEY` | 字节 Seedance 视频 |
| **siliconflow** | t2i | Bearer(OpenAI-ish, 方言) | `SILICONFLOW_API_KEY` | 托管 Kolors/FLUX 等 |
| **stepfun** | t2i | Bearer(OpenAI 兼容) | `STEPFUN_API_KEY` | 阶跃星辰 |
| **zhipu** | t2i | Bearer(OpenAI 兼容) | `ZHIPU_API_KEY` | 智谱 CogView |
| **ppio** | t2i | Bearer(OpenAI 兼容) | `PPIO_API_KEY` | 派欧云聚合 |

model 列表与估算成本用 `imagecli model --json` 查。**接新的 OpenAI images 兼容服务**只需复制 `src/providers/agnes.rs` 改三个值(base_url / model / key 候选),无需写协议代码(D-009)。

## 命令一览

| 命令 | 作用 |
| ---- | ---- |
| `generate` | 提交一个或多个任务,轮询到终态,默认下载产物 |
| `model [<provider/model>]` | 交互式选择器 / 直设默认 provider+model(持久化到配置) |
| `providers` | 列出已注册 provider 及能力 |
| `models [--provider <p>]` | 列出 model 目录 |
| `status <job_id>` | 从本地 store 读任务并向 provider 刷新(跨进程可用) |
| `download --job-id <id>` / `--url <URL>` | 下载产物 |
| `list [--status] [--capability] [--limit]` | 列本地任务, 支持过滤分页 |
| `mcp` | 启动 MCP server(stdio), 供 Claude Code/Cursor 等 agent 调用 |

`generate` 常用参数:`--provider` `--capability`(t2i/i2i/t2v/i2v) `--model` `--prompt`(可重复=批量) `--prompts-file` `--input`(本地图或 URL) `--param k=v`(可重复) `--out-dir` `--concurrency`
工程 flag:`--fallback p2,p3`(故障转移) `--retries N`(可重试错误重试) `--dry-run`(只估成本不跑) `--max-cost <金额>`(超预算拒绝) `--verbose`(可观测) 全局 `--json`。以 `imagecli <command> -h` 为准。

## 作为 MCP server(agent-first)

`imagecli mcp` 在 stdio 上跑 JSON-RPC MCP server, 暴露 6 个工具:`generate_image` / `generate_video` / `list_providers` / `list_models` / `get_job` / `list_jobs`。让 Claude Code / Cursor 等直接把 imagecli 当图像/视频生成工具用(key 走 server 进程的环境变量)。

## 设计概览

- **provider × transport 两维抽象(D-003)**:能力维(Capability)与传输维(http-queue 异步 / http-sync 同步 / async-task 提交轮询 / subprocess 预留)正交。五种鉴权范式:Bearer、OpenAI 兼容 drop-in+方言、prediction、本地 HS256 JWT(可灵)、火山 AK/SK V4 签名(即梦)。
- **Submit + Poll 归一(D-005)**:异步走真轮询;同步 `submit` 直接返回终态、`poll` no-op。
- **SQLite 跨进程持久化(D-007)**:任务句柄+状态落 `~/.local/share/imagecli/jobs.db`,provider 无状态、句柄随 Job 流转,换进程也能续查。
- **多 provider 路由 + 故障转移**:主 provider 失败(限流/配额/网络)自动切候选链下一家并重试;`--json` 记 `provider_used`/`fallback_from`/`attempts`。
- **健壮性**:有界并发(Semaphore)+ 指数退避 full jitter、可重试 vs 不可重试错误分类、预算护栏(Decimal, 非浮点)、配置原子写(临时文件 rename)+ 时间戳备份防损坏。
- **退出码契约**:`0`=全部成功;非零=至少一个任务失败/出错。脚本/agent 应看退出码 + `--json` 的 `status`,而非 stdout 文字。

## 当前状态(诚实标注)

- **能力**:t2i(11 家) · i2i(google/jimeng/volcengine, 本地图 base64 内联) · t2v/i2v(fal/replicate/seedance/kling) · upscale(fal/replicate)。
- **真实验证**:agnes(多次真实出图 + 批量 + 故障转移)与 google(链路通、卡免费配额)已真打网络;**其余 provider 代码完整 + 离线测试通过,真实出图/出片待各自 key 联调坐实**。
- **测试**:200+ 通过(单元 + 集成);三平台 CI(ubuntu/windows/macos)绿,Windows 可编可测已坐实。
- **尚未做**:fal/replicate 本地图自动上传(现需先传成 URL)、per-model 参数 schema 校验。

设计与取舍详见 `DECISIONS.md`(D-001~D-014),范围与验收见 `REQUIREMENTS.md`,测试覆盖见 `TEST-LOG.md`。

## 用法示例

`examples/` 下有一组可直接跑的示例脚本(文生图/图生图/文生视频/超分/批量与预算/故障转移/model 选择/MCP),每个脚本头部注明用途与所需 key 环境变量。见 `examples/README.md`。
