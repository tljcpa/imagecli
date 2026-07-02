# REQUIREMENTS · imagecli

## 做什么
一个通用的、多 provider 的图像/视频生成命令行工具。一套命令统一调度多家**有公开 API**的生成模型(fal.ai、Replicate、OpenAI Images、Google、自托管 ComfyUI 等),提供图像/视频的生成、编辑、超分等能力,并把"提交-轮询-下载"的异步任务编排做成统一内核。

## 为什么
字节即梦官方 CLI(`dreamina`)只能调单一后端,且工程上缺口很多:无批量、无并发、`--poll` 是裸忙等无退避、无重试策略、无自动下载、无预算护栏、无稳定 `--json` 契约、参数模型自相矛盾。本项目以即梦 CLI 为**能力对标基线**,做一个能力更全、更独立、agent-first 的统一工具。

## 范围(MVP)
1. provider 抽象层:能力(capability) × transport(传输方式)两维。
2. 异步任务编排内核:Submit -> Poll -> Download 的统一 Job 模型,含并发、退避重试、限流、可取消。
3. 首个落地 provider:fal.ai(Queue API)。至少 text2image 能力端到端闭环。
4. CLI 子命令:generate / status / download / providers / models。
5. 配置与密钥:keyring + 环境变量回退,按 provider 命名空间。
6. 稳定的 `--json` 机器输出契约。

## 非目标(明确划线)
- **不逆向即梦或任何私有签名鉴权**;不接入需要逆向才能用的后端。
- **不薅免费额度、不接码轮换账号、不绕过付费墙**。
- 不通过逆向方式接入没有公开 API 的产品(Midjourney、Sora2 等)——只在有正规 API 时接,否则文档标注缺口。
- 即梦本身不作为接入后端(详见 DECISIONS D-002)。
- MVP 不做富 TUI 交互、不做自托管模型的运维编排(后续迭代)。

## 验收标准(MVP)
- `cargo build` 与 `cargo clippy` 零报错通过。
- provider trait + 三种 transport 的类型骨架成立,能编译。
- fal provider 实现完整 submit/poll/download 逻辑;在提供真实 key 时,`imagecli generate --provider fal --capability text2image --prompt "..."` 能跑通并落盘一张图。
- `--json` 输出可被脚本稳定解析;退出码有契约。
- 三件套台账随开发持续 append。

## 范围变更(append-only)
- (暂无)

## 范围变更追加(2026-06-26)
- 海外 provider 尽量全接(D-011);新增 "/model 式" 统一模型选择器需求(D-011)。
