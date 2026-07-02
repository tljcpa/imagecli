# DECISIONS · imagecli (append-only)

> 每条:做了什么决定 / 为什么 / 否决了什么 / 何时该改。随决策真实发生的当下记录,不回溯补叙。
> (注:本文件 2026-06-26 曾因磁盘写满[ENOSPC]被 Edit 截断,后由对话 context 完整重建。)

## D-001 · 语言选 Rust(否决 Go / TypeScript)
- **决定**:用 Rust 实现。
- **为什么**:项目内核是"正确性敏感的并发异步任务状态机"(N provider × submit/poll/download × 重试/限流/取消),评判标准是失败路径与并发下的正确性,不是吞吐。Rust 的 enum sum-type 状态机 + 穷尽 match + `Result`/`?` + ownership(编译期杜绝数据竞争)正是这类系统的工程天花板;单二进制利于长期分发与维护。对标 OpenAI Codex CLI 从 TS 重写为 Rust。
- **否决了什么**:Go——开发快、有官方 `replicate-go`、与即梦同栈,但在"不考虑开发难度"前提下相对 Rust 无工程净胜项,sum-type/错误处理弱于 Rust。TypeScript/Node——能做最像 Claude Code 的 ink 体验,但拖 Node 运行时、并发与正确性模型弱。
- **何时该改**:若重心转向 agentic 对话式 UX 且需极快迭代,重评 TS;若 Rust 异步生态对某关键 provider SDK 缺位且自写成本过高,局部重评。
- **前提**:用户明确指示"别考虑开发难度",本决策在此约束下成立。

## D-002 · 即梦不作为接入后端,仅作能力对标基线
- **决定**:项目不接入即梦后端,即梦 CLI 仅用于"它会什么、我至少要覆盖什么"的对标。
- **为什么**:即梦无公开 API;"自造即梦客户端"只剩逆向私有签名鉴权一条,属未授权访问、违反 ToS、且字节风控使其不可持续。
- **否决了什么**:(a) shell 调官方 dreamina CLI 当 provider——合法但用户明确不想依赖;(b) 逆向 `/dreamina/cli/v1/*`——划红线不做。
- **何时该改**:若即梦/字节放出正规公开 API(如 BytePlus 上的 Seedance/Seedream),作为正规 provider 接入。

## D-003 · provider 抽象分两维:能力 × transport
- **决定**:Provider 契约按"能力(Capability)"与"传输方式(transport)"两维抽象。transport 分三类:http-queue(异步队列,fal/replicate)、http-sync(同步,OpenAI Images/Gemini)、subprocess(预留)。
- **为什么**:不同 provider 执行模型本质不同,把执行方式也抽象掉,上层编排器只面对统一 Job。
- **何时该改**:出现第四类传输模型(如 websocket 流式、webhook-only)时扩展。

## D-004 · 首个落地 provider 选 fal.ai
- **决定**:MVP 第一个实现 fal,验证 http-queue transport 与整体抽象。
- **为什么**:fal 的 Queue API 是异步范本;一 key 调数百模型(含 Seedance),性价比最高、顺带覆盖视频。
- **否决了什么**:把 Replicate 作为第一个——留作第二个 provider,验证抽象可复用。

## D-005 · 异步任务统一为 Submit + Poll 双方法
- **决定**:Provider 接口核心是 `submit` 与 `poll`。同步 provider 的 `submit` 直接返回终态、`poll` no-op;异步走真轮询。
- **为什么**:用最小接口归一化同步/异步差异。借鉴 Replicate 的 Run vs CreatePrediction+Wait、fal 三态。
- **何时该改**:若引入 webhook-only provider,需补回调归一化路径。

## D-006 · 项目差异化定位在工程编排层,模型能力不作为竞争维度
- **决定**:imagecli "超过即梦" 严格限定在工程层(多 provider 路由与故障转移、批量与 bounded 并发、退避重试与幂等重提、自动下载与产物归档、成本预检与预算护栏[Decimal]、多 profile/多 key 配置、稳定 `--json` 与退出码契约、可观测性追踪、agent-first 的 SKILL.md/MCP)。模型画质由各家公开 API 提供,本项目对模型层职责只有"接得全、接得对"。
- **为什么**:模型能力由 API 提供方决定,imagecli 是消费方;竞争点放模型上不可控也无意义。项目成败标准在编排工程。
- **否决了什么**:以"画质更强"作为卖点或验收标准。
- **何时该改**:除非未来自训/自托管模型(当前非目标),否则不变。

## D-007 · 任务状态用 SQLite 跨进程持久化;Provider::poll 改为接收完整 Job
- **决定**:(a) 新增编排层 JobStore,用 SQLite(rusqlite bundled,单二进制)持久化每个任务句柄与状态,落 XDG data dir(`~/.local/share/imagecli/jobs.db`)。(b) `poll(&self, job_id)` 改为 `poll(&self, job: &Job)`:provider 无状态化,从 Job.raw_meta 还原句柄,不再内部用内存 Mutex<HashMap>。
- **为什么**:跨进程 status/download/list 是公共需求(内存句柄换进程即失效);store 集中编排层,避免每个 provider 各搞一套;SQLite 支持 list 按 status/capability/时间过滤;即梦官方本地 task store 同样是 SQLite。
- **否决了什么**:内存 Mutex<HashMap>(跨进程失效)、一任务一 JSON 文件(并发与过滤弱)、sled/KV(list 过滤需手写遍历)。
- **与 D-005 关系**:细化,Submit+Poll 不变,Poll 入参从 job_id 升级为完整 Job。
- **何时该改**:若引入 webhook-only provider,store 需补回调写入路径。

## D-008 · 第一个真实联调 provider 改为 Google Gemini(图像生成)
- **决定**:用 Google Gemini `gemini-2.5-flash-image`(generateContent REST),走 http-sync transport,鉴权 header `x-goog-api-key`,key 经环境变量注入(`GEMINI_API_KEY`/`GOOGLE_API_KEY`/`IMAGECLI_GOOGLE_KEY`),不落项目文件。
- **为什么**:用户已有 AI Studio key 且有免费额度,绕开海外支付;同步 REST 最简单,验证 http-sync 路径。
- **否决了什么**:把 fal 作为第一个真实 provider(需海外支付,推迟;架构地位不变)。
- **暴露的架构债(已处理)**:Gemini 产物是响应内 base64 inline 字节而非 URL。已扩展 Asset 承载 inline 字节、download 统一处理 URL/inline/local 三种来源。
- **何时该改**:若主要用 Imagen `:predict` 或 Vertex AI(鉴权/请求结构不同),为 Google 增第二 transport 或子 provider。

## D-009 · 接 agnes 作为 OpenAI 兼容 provider 模板,并作为第一个真实端到端联调
- **决定**:接入 agnes(新加坡 Agnes AI,免费),走 http-sync + **标准 OpenAI images 协议**(`POST https://apihub.agnes-ai.com/v1/images/generations`,`Authorization: Bearer <key>`,图像模型 `agnes-image-2.1-flash`)。实现成可复用的"**OpenAI 兼容 provider 模板**":base_url + 模型名 + key 三者参数化,任何 OpenAI images 兼容服务(中转站、SiliconFlow、DeepSeek 等)只需配置即可复用。agnes 同时作为项目第一个真实出图 provider。
- **为什么**:(a) OpenAI 协议兼容 → 一次实现接一片;(b) 完全免费(cost=0,用户 2026-06-19 实测)→ 真实联调零成本;(c) 带视频(异步轮询)→ 后续验证异步视频;(d) RPM≈30/并发<=25 免费层限速 → 压测 runner 限流+退避。
- **否决了什么**:把 Google Gemini 作为模板——协议特化,模板价值低,作并列 provider(D-008);其 Asset inline 扩展是公共基础设施,agnes 复用之。
- **凭证**:key 经环境变量注入(`AGNES_API_KEY`,或运行时从 `~/agnes/pool.json` 读),不落项目文件,已脱敏。
- **何时该改**:agnes 是 A 轮初创免费层,自警随时可能收紧/下线 → 不作生产硬依赖;若下线,凭模板无缝换任一 OpenAI 兼容服务。

## D-010 · 大陆支持走各厂商正规公开 API(不接消费产品壳);即梦能力经火山引擎获取
- **决定**:大陆生图/视频能力一律走**厂商正规公开 API**,覆盖多家而非一家:字节(火山引擎 Volcengine/方舟 Ark 的 Seedream/Seedance)、阿里(通义万相 wanx/DashScope)、智谱(CogView/CogVideoX)、腾讯(混元生图)、百度(文心一格)、快手(可图 Kolors)、MiniMax、阶跃,以及 SiliconFlow 等聚合平台。其中 OpenAI 兼容的(SiliconFlow 及部分厂商)直接复用 D-009 的 OpenAI 兼容模板(复制 agnes.rs 改三个值);其余写各自 provider。
- **为什么**:这些都是合法、独立、国内可付费(人民币)的正规 API;"即梦的能力"本质是字节 Seedream/Seedance,经火山引擎即可正规获取,无需碰即梦产品壳。满足用户"大陆也得支持、且不止字节一家"。
- **否决了什么**:接即梦/可灵等**消费产品壳**(无公开 API,只能逆向[红线]或 subprocess 依赖官方 CLI);D-002 中"是否 subprocess 调即梦官方 CLI"的纠结就此作废——不需要了。
- **与 D-002 的关系**:修正而非推翻。D-002"即梦产品本身只做对标、不接入"仍成立;本条补充:即梦底层模型(Seedream/Seedance)的能力经火山引擎正规 API 接入,与即梦产品壳无关。
- **何时该改**:某厂商 API 下线、或新增值得接的厂商时增减;若某厂商转为 OpenAI 兼容,迁到模板。

## D-011 · 海外 provider 尽量全接;提供 "/model 式" 统一模型选择器
- **决定**:(a) **海外也尽量全接**:在 fal 之外扩 Replicate、OpenAI、Stability、Black Forest Labs(Flux)、Ideogram、Recraft、Kling、Runway、Luma、Google Veo 等(详见已有海外全景调研);OpenAI 兼容的复用 D-009 模板,其余写各自 provider/transport。(b) **统一模型选择器**:提供 `imagecli model` 交互式命令,列出所有 provider×模型(带能力/成本/可用性标记),选中即设为默认并持久化到配置文件(`~/.config/imagecli/config`);`generate` 未显式指定时用该默认。对标 Claude Code 的 `/model` 体验;也支持 `imagecli model <provider/model>` 非交互直设。
- **为什么**:多 provider 的价值只有靠"统一目录 + 易切换默认"才能兑现——否则几十个模型用户记不住、记不准。这是把"接得全"转成"用得顺"的关键 UX。海外全接是"通用 CLI"应有之义。
- **设计**:模型目录由各 provider 声明(name/capability/估算成本/可用性[有无 key]);选择器读目录;默认 provider+model 存配置,与 key(env/keyring)分离。
- **时序**:实现排在工程硬化(批量+预算,改 cli)之后,避免 cli/registry 文件并发冲突。
- **何时该改**:provider/模型增减自动反映在目录;若交互式选择器在无 TTY(agent/CI)环境,需降级为非交互列表+直设。

## D-012 · provider 适配器按"OpenAI 兼容程度"分三类(大陆调研导出)
- **决定**:接入实现按三类适配器组织,而非"OpenAI 模板一把梭":
  - **(A) OpenAI-images drop-in**:请求/返回都对齐 OpenAI(`size`/`data[]`)。火山 Seedream、阶跃 StepFun、智谱 CogView、PPIO。直接复用 D-009 模板,改 base_url/model/key。
  - **(B) OpenAI-ish 同路径异 schema**:端点同为 `/v1/images/generations` 但字段不同。SiliconFlow(`size`→`image_size`、`batch_size`、返回 `images[]` 而非 `data[]`)。模板需抽出"方言"映射(请求字段名 + 返回解析)的可配置分支。
  - **(C) async-task 提交+轮询**:复用 fal 的 http-queue 骨架(提交拿 task_id → 轮询 → 取结果)。覆盖所有视频(Seedance/可灵/CogVideoX/万相视频/混元/MiniMax)及阿里万相图像、腾讯、百度;各家只换鉴权(Bearer/AK-SK 签名/JWT/access_token)与字段映射。
- **为什么**:调研实证三档差异真实存在,硬套单一模板会在 B 类解析返回时炸、C 类根本不是同步。分类后每类一套骨架,新厂商归类即可,最大化复用。
- **首接顺序**:火山 Seedream(A,即梦同源,最值)→ StepFun/CogView/PPIO(A)→ SiliconFlow(B,加方言)→ 火山 Seedance 视频(C,即梦同款视频)→ 可灵/MiniMax/万相等(C)。
- **何时该改**:某厂商改协议则换类;C 类的"异步轮询骨架"应做成各家共享、只注入鉴权与字段映射。

## D-013 · 向优秀项目借鉴设计(学思想、自实现、不抄代码)
- **决定**:持续从成熟项目借鉴**设计思想**,用 Rust 自己实现,注释可注明灵感来源,**绝不复制粘贴其代码**。借鉴清单(已批判筛选,只取适合"多 provider 任务编排 CLI"的):
  - **Claude Code(/root/CLI仓库 还原源码)**:① 配置分层级联 + "叠加 vs 覆盖"语义(权限类用并集、默认选择类用覆盖)——做多 profile/项目级配置时用;② **配置/状态写入防损坏:文件锁 + 原子写(临时文件 rename)+ 时间戳备份**——直接对症本项目磁盘满把 DECISIONS 截断的事故;③ 命令/工具注册表 + 延迟加载;④ 优雅中断(对应我们 tokio cancellation);⑤ 启动并行预取(配置/catalog 预热)。
  - **fal genmedia CLI**:异步任务"提交→轮询→下载"模板、文件名占位符、provider schema 自描述。
  - **Replicate**:prediction 状态机归一化。
  - **charmbracelet/mods、aichat**:Rust/Go 单二进制 CLI 的工程骨架与 TUI 交互(dialoguer/ratatui 选型)。
- **不借鉴的(批判)**:Claude Code 的 LLM agent 主循环、上下文压缩、firstParty-only 的 provider enum——那是 agentic LLM CLI 的场景,与我们任务编排内核不同(参见 D-012 我们 provider 异构必须有抽象层)。
- **为什么**:站在巨人肩上少踩坑;但"学不抄"既是版权/原创底线,也因为别人的实现绑定其场景,照抄会把不适合的假设也搬进来。
- **优先级**:②配置/台账写入防损坏 最该先做(已被磁盘满坑过一次,有切肤之痛)。
- **何时该改**:发现新的优秀实践就追加;某条借鉴被证明不适配就剔除。

## D-014 · 火山两条线区分;即梦 visual 另接独立 provider;视频建通用 async-task 骨架
- **背景**:核实发现火山有两条独立产品线,底层同为 Seedream/Seedance 但 API 不同:① **方舟 Ark**(`ark.cn-beijing.volces.com/api/v3`,Bearer,OpenAI drop-in,图像同步)——现有 `volcengine` provider 接的就是它;② **即梦 visual**(`visual.volcengineapi.com`,AK/SK V4 签名 HMAC-SHA256,火山 Action 风格,异步轮询,`req_key=jimeng_t2i_v40`,响应 `image_urls`/`binary_data_base64`)。
- **决定**(用户拍板):
  - 保留 `volcengine`=方舟 Ark Seedream(图像,已接)。
  - **另接独立 `jimeng` provider** 对接即梦 visual(85621):需实现火山 AK/SK V4 签名 + Action 异步轮询 + 自有字段映射。不混进 volcengine。
  - 视频能力:建**通用 async-task 骨架**(提交→拿 task_id→轮询→取产物 URL,鉴权方式可注入),覆盖 Ark Seedance(Bearer)、可灵(本地 HS256 JWT:iss=AK/exp=+1800/nbf=-5/SK 签名)、即梦 visual(AK/SK V4 签名)三种鉴权。先用 Seedance(Bearer,最简单)验证骨架,再并行加可灵/即梦。
- **为什么**:三家都是异步任务,共用骨架避免各写一套;即梦 visual 协议与 Ark 差异大(签名+异步+字段全不同),独立 provider 才不脏(对齐 D-012 C 类)。
- **何时该改**:某家协议变更则改其适配;骨架应让"鉴权注入 + 字段映射"可配。
