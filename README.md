# imagecli

一个通用的、多 provider 的图像生成命令行工具：一套命令统一调度 fal、Google Gemini、
agnes（OpenAI 兼容）等有公开 API 的生成模型，把"提交—轮询—下载"的异步编排做成统一内核。

## 为什么做它

以字节即梦官方 CLI（`dreamina`）为**能力对标基线**。即梦 CLI 只能调单一后端，且工程上缺口很多：
无批量、无并发、`--poll` 是裸忙等无退避、无重试策略、无自动下载、无预算护栏、无稳定 `--json` 契约。

imagecli 的差异化**严格限定在工程编排层**（见 DECISIONS D-006）：多 provider 路由、批量与有界并发、
指数退避重试、自动下载与产物归档、SQLite 任务持久化、稳定的 `--json` 与退出码契约、agent-first 的 SKILL.md。
**模型画质由各家公开 API 提供，本项目不把模型能力作为竞争维度**——职责只有"接得全、接得对"。

## 安装 / 构建

```bash
source ~/.cargo/env        # 确保 cargo 在 PATH 中
cargo build                # 调试构建，产物在 target/debug/imagecli
cargo build --release      # 发布构建，产物在 target/release/imagecli
cargo test                 # 跑单元/集成测试
```

单二进制，无运行时依赖（SQLite 经 rusqlite bundled 静态链接）。

## 快速上手

### 1. 配置密钥（环境变量）

密钥按 provider 命名空间从环境变量读取，**项目专用变量优先**，再回退到系统 keyring。
绝不要把 key 写进项目文件。

| provider | 环境变量（优先级从高到低） |
| -------- | -------------------------- |
| fal      | `IMAGECLI_FAL_KEY`、`FAL_KEY` |
| google   | `IMAGECLI_GOOGLE_KEY`、`GEMINI_API_KEY`、`GOOGLE_API_KEY` |
| agnes    | `AGNES_API_KEY`、`IMAGECLI_AGNES_KEY` |

```bash
export AGNES_API_KEY=你的key      # 示例：agnes 免费层
```

### 2. 生成一张图

默认 provider 是 `fal`（需海外付费 key）。零成本上手建议用 `agnes`（免费层）或 `google`（Gemini 免费额度）：

```bash
imagecli generate --provider agnes --prompt "a red fox in snow"
```

默认会下载产物到 `./out`（用 `--out-dir` 改目录，`--no-download` 跳过下载）。
加 `--json` 拿稳定的机器可解析输出。

### 3. 看产物

```bash
ls ./out
```

## 已支持 providers

| provider | 传输方式 | 能力 | 默认 model | key 变量 | 备注 |
| -------- | -------- | ---- | ---------- | -------- | ---- |
| fal      | http-queue（异步） | text2image | `fal-ai/flux/dev` | `IMAGECLI_FAL_KEY` / `FAL_KEY` | 一 key 调数百模型；需海外付费 |
| google   | http-sync（同步）  | text2image | `gemini-2.5-flash-image` | `GEMINI_API_KEY` 等 | 产物为响应内 base64，自动落盘 |
| agnes    | http-sync（同步）  | text2image | `agnes-image-2.1-flash` | `AGNES_API_KEY` 等 | 新加坡 Agnes AI 免费层，限速约 30 RPM |

**接新 provider**：任何 OpenAI images 兼容服务（中转站、SiliconFlow、DeepSeek 等）只需
复制 `src/providers/agnes.rs` 改三个值（base_url / 默认 model / key 候选变量）即可接入，无需写协议代码（D-009）。
**大陆方向**（火山引擎 Seedream/Seedance、通义万相、智谱 CogView、SiliconFlow 等正规公开 API）规划中（D-010）。

## 命令一览

| 命令 | 作用 |
| ---- | ---- |
| `generate` | 提交一个或多个任务，轮询到终态，默认下载产物 |
| `status <job_id> --provider <p>` | 从本地 store 读出任务并向 provider 刷新一次（跨进程可用） |
| `download --job-id <id>` / `--url <URL>` | 下载已成功任务的产物，或直接下载给定 URL |
| `list [--status] [--capability] [--limit] [--offset]` | 列出本地 store 里的任务，支持过滤分页 |
| `providers` | 列出已注册 provider 及其能力 |
| `models --provider <p>` | 列出某 provider 的默认/已知 model |

`generate` 常用参数：`--provider` `--capability`（默认 text2image）`--model` `--prompt`
`--input <URL>`（可重复）`--param key=value`（可重复，value 先按 JSON 解析）`--out-dir`
`--concurrency`（默认 4）`--no-download`。全局 `--json` 在任意子命令可用。
具体以 `imagecli <command> -h` 为准。

## 设计概览

- **provider × transport 两维抽象（D-003）**：能力维（Capability，如 text2image）与传输维
  （transport：http-queue 异步队列 / http-sync 同步 / subprocess 预留）正交。上层编排器只面对统一的 Job。
- **Submit + Poll 归一（D-005）**：异步 provider 走真轮询；同步 provider 的 `submit` 直接返回终态、`poll` 为 no-op。
- **SQLite 任务持久化（D-007）**：每个任务的句柄与状态落 `~/.local/share/imagecli/jobs.db`
  （可用 `IMAGECLI_DB_PATH` 覆盖），所以 `status`/`download`/`list` 换个进程也能续查。provider 自身无状态，句柄随 Job 流转。
- **编排内核**：有界并发（Semaphore）、指数退避 + full jitter 轮询、单任务超时（默认 600s）。
- **退出码契约**：`0` 表示全部成功；非零表示至少一个任务失败/出错（含 provider 直接返回终态 `failed`）。
  脚本/agent 应看退出码 + `--json` 的 `status` 字段，而非 stdout 文字。

## 当前状态（MVP，诚实标注）

已实现：fal/google/agnes 三个 provider 的 **text2image** 端到端闭环、异步编排内核、
SQLite 持久化、`--json` 契约、退出码契约。

**尚未实现（规划中）**：

- 视频能力（text2video / image2video / framestovideo）——`capability` 枚举里有，但暂无 provider 提供。
- 图生图（image2image）本地图输入已支持：`--input` 现接受本地图片路径或 URL，本地图会按各家能力 base64 内联进请求（即梦 jimeng / 火山 Seedream volcengine 走 i2i；可灵 kling 的 image2video 同样可喂本地图）。仍未覆盖：fal / replicate 的本地图需先自传成 URL（storage upload 尚未实现）。
- 超分（upscale）。
- 大陆 provider（火山引擎 / SiliconFlow 等）。
- 成本预检 / 预算护栏 / `--dry-run`——尚未实现，大批量生成前请先小批量确认。

设计与取舍详见 `DECISIONS.md`，范围与验收见 `REQUIREMENTS.md`，测试覆盖见 `TEST-LOG.md`。
