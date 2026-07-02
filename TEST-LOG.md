# TEST-LOG · imagecli

> 测了什么 / 方法 / 结果 / 没覆盖什么(边界) / 复现。测试一跑完就记,不回溯编造。

| 日期 | 测什么 | 方法 | 结果 | 未覆盖(边界) | 复现 |
|---|---|---|---|---|---|
| 2026-06-26 | 编译 | `cargo build` | 通过, 0 error, 0 warning(crate 级 allow(dead_code) 覆盖前瞻 API 表面) | 无 release profile / 无交叉编译验证 | `cargo build` |
| 2026-06-26 | 静态检查 | `cargo clippy --all-targets -- -D warnings` | 通过, 0 warning(修掉 manual-ok / unwrap_or_default / doc 缩进; 占位死代码用 crate 级 allow 收口) | clippy pedantic/nursery 组未开 | `cargo clippy --all-targets -- -D warnings` |
| 2026-06-26 | 单元测试 | `cargo test` | 18 passed; 0 failed。覆盖: fal 状态映射(三态+大小写+ERROR/未知->Failed)、GenRequest->fal 请求体、fal 结果->Asset 解析、文件名模板、退避上限、密钥 env 优先级 | 全部为纯函数/离线 mock; 真实 fal 端到端(submit/poll/download)未测, 需真实 FAL_KEY | `cargo test` |
| 2026-06-26 | CLI 冒烟 | 跑 `providers` / `generate --help` / `providers --json` | 通过, 正常输出; 列出 fal(text2image), help 完整, json 契约成立 | 仅烟测, 未覆盖 status/download/models 全部分支 | `cargo run -- providers`; `cargo run -- generate --help` |
| 2026-06-26 | 无 key 失败路径 | `env -u FAL_KEY -u IMAGECLI_FAL_KEY cargo run -- generate --prompt ...` | 通过, 无 panic; 输出中文指引"未找到 fal 的 API key...", 退出码 1 | 未验证 keyring 后端真实读写(无 keyutils 权限环境) | `env -u FAL_KEY -u IMAGECLI_FAL_KEY cargo run -- generate --prompt "x"` |
| 2026-06-26 | D-007 重构后编译 | `cargo build` | 通过, 0 error/0 warning。改动: fal 无状态化、Provider::poll/cancel 收 `&Job`、runner 注入 `Arc<JobStore>`、CLI 接 store、拆出 src/lib.rs 供集成测试复用 | 无 release/交叉编译验证 | `source ~/.cargo/env && cargo build` |
| 2026-06-26 | D-007 重构后静态检查 | `cargo clippy --all-targets -- -D warnings` | 通过, 0 warning(修掉两处 manual `ok` 的 match->`.ok()`) | pedantic/nursery 组未开 | `cargo clippy --all-targets -- -D warnings` |
| 2026-06-26 | 单元测试(增量) | `cargo test` | 24 passed(原 18 + store 3 + fal 句柄还原 3); 0 failed。新增覆盖: JobStore save/get/update/list(SQL WHERE 过滤)往返、fal 句柄经 raw_meta 序列化往返、句柄缺失给中文错、poll 入参 Job 可还原句柄 | 均为离线/本地 SQLite; 真实 fal submit/poll/download 端到端仍未测, 需 FAL_KEY | `cargo test` |
| 2026-06-26 | 跨进程持久化(自动) | 集成测试 tests/cross_process.rs: 测试进程(A)用 JobStore 写一条 succeeded 记录到临时 db, 再以子进程启动编译好的 imagecli(B) 跑 list/status/list --status failed | 通过。B 进程的 list 与 status 都读到 A 写的任务; status=failed 过滤正确排除该 succeeded 任务 | 子进程读的是终态记录, 未走真实 provider.poll 网络(终态不轮询) | `cargo test --test cross_process` |
| 2026-06-26 | 跨进程持久化(手动) | 进程A: `IMAGECLI_DB_PATH=/tmp/x.db cargo test --test cross_process seed_for_manual_demo -- --ignored --exact` 播种; 进程B(全新 `cargo run`): `cargo run -- list` / `list --json` / `status manual-demo-001 --json` / `list --status failed` | 通过。B 进程(独立 cargo run)读到 A 写的 manual-demo-001(succeeded, 含产物 URL), json 契约成立, 状态过滤正确返回空 | 同上, 手动 demo 用人造终态记录, 未打真实 fal | `export IMAGECLI_DB_PATH=/tmp/x.db; cargo test --test cross_process seed_for_manual_demo -- --ignored --exact; cargo run -q -- list` |
| 2026-06-26 | 无 key 路径(store 接入后) | `env -u FAL_KEY -u IMAGECLI_FAL_KEY cargo run -- generate --prompt "a red fox"` | 通过, 无 panic; submit 在缺 key 处返回中文指引, generate 整体退出码 1 | submit 失败发生在 store.save 之前, 故无脏记录入库(符合预期) | `env -u FAL_KEY -u IMAGECLI_FAL_KEY cargo run -- generate --prompt "a red fox"` |
| 2026-06-26 | google provider 全套(D-008) | `cargo build` / `cargo clippy --all-targets -- -D warnings` / `cargo test`(TMPDIR 指向 /root 避免 /tmp tmpfs 满) | 全绿。build 0 warning; clippy 0 warning(修一处 doc_lazy_continuation); test 34 单元(原 24 + google 5 + keys 3 + download 2 内含 inline 落盘异步测 + provider inline 构造)+ 1 跨进程集成, 0 failed | 真实 Gemini 端到端(submit 打网络/落盘/解码出图)未测, 需真实 GEMINI_API_KEY; image2image 未接线(CLI 仅接受 URL 输入, 无本地字节) | `export TMPDIR=/root/bigtmp; cargo test` |
| 2026-06-26 | CLI 冒烟: providers 含 google | `cargo run -- providers` 与 `--json providers` | 通过, 同时列出 fal(text2image)与 google(text2image), json 契约成立 | 未冒烟 google models/status 分支 | `cargo run -q -- providers` |
| 2026-06-26 | google 无 key 失败路径 | `env -u GEMINI_API_KEY -u GOOGLE_API_KEY -u IMAGECLI_GOOGLE_KEY cargo run -- generate --provider google --prompt "a red fox in snow"` | 通过, 无 panic; 输出中文指引"未找到 Google Gemini 的 API key。请设置环境变量 GEMINI_API_KEY..."; 退出码 1; 缺 key 在 submit 内 store.save 之前返回, list 仍为空(无脏记录) | 未验证 keyring 后端真实读写 | `env -u GEMINI_API_KEY -u GOOGLE_API_KEY -u IMAGECLI_GOOGLE_KEY cargo run -- generate --provider google --prompt "x"` |
| 2026-06-26 | 内联字节产物落盘 | download.rs 单测 `inline_asset_writes_decoded_bytes_with_mime_ext`: 用 1x1 png base64 解码后构造 InlineBytes 素材 -> download_asset 落盘 | 通过, 文件名扩展名由 mime 推断为 png, 落盘字节与解码字节逐字节一致 | 仅离线; 与 Gemini 真实响应的端到端贯通未测 | `cargo test inline_asset_writes` |
| 2026-06-26 | OpenAI 兼容模板+agnes(D-009) build/clippy/test | `source ~/.cargo/env; export TMPDIR=/root/bigtmp; cargo build / clippy --all-targets -D warnings / test` | 全绿。build 0 warning; clippy 0 warning(修一处 doc_lazy_continuation); test 41 单元(原 34 + openai_compat 6 + keys agnes 1)+ cross_process 1 + exit_code 2, 0 failed | 真实 agnes 端到端(submit 打网络/出图)未测, 需真实 AGNES_API_KEY; b64_json 内联落盘的真实响应贯通未测(默认 url 路) | `export TMPDIR=/root/bigtmp; cargo test` |
| 2026-06-26 | OpenAI 兼容请求体构造 | openai_compat 单测: model+prompt+n 必有; size/response_format 随 config 有无增减; 用户 --param 覆盖默认 | 通过 | 仅离线纯函数 | `cargo test build_body` |
| 2026-06-26 | OpenAI 兼容响应两路解析 | openai_compat 单测: data[].url -> Asset::Url; data[].b64_json -> 解码走 Asset::Inline(PNG 魔数校验); 空 data -> 空; error.message 可抽出 | 通过 | 未与 agnes 真实响应贯通 | `cargo test parse_` |
| 2026-06-26 | agnes key 解析(仿 google) | keys 单测: AGNES_API_KEY 优先于 IMAGECLI_AGNES_KEY; 两者缺 -> None; 缺 key 中文指引点名两变量 | 通过 | 未验证 keyring 后端真实读写 | `cargo test agnes_key` |
| 2026-06-26 | 退出码契约修复(D-006) | 修 cmd_generate: Ok(Job) 分支补判 job.status==Failed -> had_error(此前同步 provider 返回终态 Failed 会 exit=0)。集成测试 tests/exit_code.rs 子进程跑无 key agnes/google generate 断言退出码非零 | 通过, 退出码=1; 实证 `env -u AGNES_API_KEY -u IMAGECLI_AGNES_KEY cargo run -- generate --provider agnes --prompt x; echo exit=$?` -> exit=1, 输出中文缺 key 指引 | 未构造"submit 真返回 Ok(Failed 终态)"的真实 provider 响应(当前 google/fal/agnes 缺 key 走 Err 路径); 该分支由单步代码审查保证 | `cargo test --test exit_code` |
| 2026-06-26 | providers 含 agnes 冒烟 | `cargo run -- providers` 与 `--json providers` | 通过, 列出 agnes/fal/google 三个(均 text2image), json 契约成立 | 未冒烟 agnes models/status 分支 | `cargo run -q -- providers` |
| 2026-06-26 | agnes 真实端到端出图(首次真图) | 主控用 ~/agnes/pool.json key 经 env 注入跑 generate --provider agnes | 成功:exit 0, 落盘 PNG 1024x1024 RGB 1.6MB;先因 response_format 报 HTTP400, 去该字段(default_response_format=None)后成功 | 仅 text2image+url 路;b64_json 路/视频/image2image 真实未测;size 字段被接受 | source ~/.cargo/env; AGNES_API_KEY=<key> cargo run -- generate --provider agnes --prompt "..." --out-dir ./out |
| 2026-06-26 | 批量+预算护栏(pricing/CLI) build/clippy/test | `source ~/.cargo/env; export TMPDIR=/root/bigtmp; cargo build / clippy --all-targets -D warnings / test` | 全绿。build 0 warning; clippy 0 warning(修 nonminimal_bool/useless_conversion/ptr_arg 三处); test 50 单元(原 41 + pricing 5 + cli 批量/护栏纯函数 4)+ budget 3 + cross_process 1(+1 ignored)+ exit_code 2 = 56 passing, 0 failed | 真实出图未测(护栏全在 submit 前短路, 离线); 超免费额度后 google/fal 真实计费阶梯未建模(pricing 非零值为粗估占位) | `export TMPDIR=/root/bigtmp; cargo test` |
| 2026-06-26 | pricing 单价表(Decimal) | pricing 单测: agnes/google=0(含 estimate_total 100 任务仍为 0); fal 文生图 0.025/视频 0.50; estimate_total=单价×N 精确(0.025×4=0.100=0.1); --max-cost 边界(等于不拒/略低拒/略高不拒); 未知 provider 非 0 | 通过 | 单价为粗估占位, 未与厂商实时计费对齐 | `cargo test -p imagecli pricing` |
| 2026-06-26 | prompts-file 解析 + fan-out 请求构造 | cli 单测: parse_prompts_content 忽略空行/空白行/# 注释行、保留行内 #; build_requests 多 prompt 等量 fan-out(顺序/model/inputs/params 共享)、空 prompt 退回单个 None 请求 | 通过 | 纯函数离线; 未测文件读失败的 IO 错误路径 | `cargo test parse_prompts_content; cargo test build_requests` |
| 2026-06-26 | --dry-run 不触发网络/不写库 | tests/budget.rs 子进程: 无 key 跑 `generate --provider agnes --prompt a --prompt b --dry-run --json` | 通过, exit 0; 输出 dry_run/task_count=2/estimated_cost="0"(agnes 免费); 无缺 key 报错; 同 db 复跑 list --json 为空(未产生 store 记录) | 未覆盖 dry-run 与真实 provider 的成本一致性(无真实计费对照) | `cargo test --test budget dry_run` |
| 2026-06-26 | --max-cost 超预算拦截 | tests/budget.rs 子进程: 无 key 跑 `generate --provider fal --prompt a..d --max-cost 0.01`(预估 0.10) | 通过, exit=1; 输出中文"预估总成本 0.100 USD 超过 --max-cost 0.01 上限, 已拒绝执行..."; 拒绝在开库前, list 为空(无脏记录); 另测 --max-cost 1.00 充足时放行(配 --dry-run exit 0) | 未测"恰好等于上限"的进程级行为(单测已覆盖 total<=max 不拒) | `cargo test --test budget max_cost` |
| 2026-06-26 | 冒烟实证: dry-run/max-cost/prompts-file | `cargo run -- generate ...` 三条 | dry-run --json(agnes 无 key): exit 0, estimated_cost="0", prompts=[a,b], task_count=2; max-cost 拦截(fal 4 prompt max 0.01): exit 1 + 中文拒绝提示; prompts-file(3 有效行/含注释空行)dry-run: "提交 3 个任务, 预估 0.075 USD" exit 0 | 仅冒烟; 真实提交出图路径由有 key 时另测 | 见上方各命令 |
| 2026-06-26 | /model 选择器+catalog+配置持久化(D-011) build/clippy/test | `source ~/.cargo/env; export TMPDIR=/root/bigtmp; cargo build / clippy --all-targets -D warnings / test` | 全绿。build 0 warning; clippy 0 warning(修两处 doc_lazy_continuation); test 62 单元(原 50 + catalog 6 + settings 4 + cli 回退链 2)+ budget 3 + cross_process 1(+1 ignored)+ exit_code 2 = 68 passing, 0 failed | 交互式 dialoguer Select 路径(TTY)未自动测(无 TTY 环境), 仅测了无 TTY 降级与纯函数; 真实出图未测 | `export TMPDIR=/root/bigtmp; cargo test` |
| 2026-06-26 | catalog 聚合/解析/渲染(纯函数) | catalog 单测: assemble 用 has_key 覆盖 available(agnes/google 有 key=true、fal 无 key=false); resolve_selection alias 大小写不敏感、provider/model(model 含 '/')首 '/' 切分、裸 model_id 唯一匹配、未知/空返回 None; catalog_to_json est_cost 为字符串+available 布尔 | 通过 | 仅离线; 真实 env/keyring 的 has_key 由集成冒烟覆盖 | `cargo test -p imagecli catalog` |
| 2026-06-26 | 配置读写往返(Settings/toml) | settings 单测: 空配置往返仍空(skip_serializing_if 不写空字段); 写默认 provider+model->读回逐字段一致; save_to 真实临时文件->load_from 读回一致; 缺文件->空 Settings 不报错 | 通过 | 未测并发写(单条用户偏好, 非并发场景) | `cargo test -p imagecli roundtrip` |
| 2026-06-26 | generate 默认回退链(纯函数) | cli 单测: resolve_effective_provider flag>cfg>内置 agnes(非 fal); resolve_effective_model flag>同 provider 的 cfg model>None(异 provider 不串味) | 通过 | 纯函数; 进程级由下方冒烟覆盖 | `cargo test -p imagecli effective_` |
| 2026-06-26 | 冒烟实证: model --json 无 TTY + available 反映 key | `cargo run -- model --json`(管道=无 TTY, 全清 key) / 同命令带 `AGNES_API_KEY=dummy` | 无 key 时三 provider(agnes/fal/google)available 全 false; 设 AGNES_API_KEY 后 {agnes:True, fal:False, google:False}, 证明 available 跟随 has_key; est_cost 为字符串(agnes/google 0, fal 0.025) | 仅 dummy key 形状判定(has_key 只看取不取得到值, 不验真伪) | `env -u ... cargo run -q -- model --json` |
| 2026-06-26 | 冒烟实证: 无 TTY 降级为列表 | `cargo run -- model`(管道, 无 selector, 非 --json) | 不进交互; 按 provider 分组打印目录 + 提示 "用 imagecli model <provider/model> 设置默认模型"(D-011 无 TTY 降级) | 交互 TTY 路径未自动测 | `cargo run -q -- model` |
| 2026-06-26 | 冒烟实证: 设默认->持久化->generate 走默认 | `IMAGECLI_CONFIG_PATH=/root/bigtmp/x.toml` 下 `model agnes/agnes-image-2.1-flash` 写配置, 再 `generate --prompt x --dry-run --json`(不带 --provider) | 配置文件写出 default_provider="agnes"/default_model="agnes-image-2.1-flash"; dry-run 解析出 provider=agnes/model=agnes-image-2.1-flash, 证明默认生效; 另测: 无配置无 flag->内置 agnes(非 fal); alias "flux" 设默认->走 fal; --provider google flag 覆盖配置默认 | 真实提交出图未测(dry-run 在 submit 前) | `export IMAGECLI_CONFIG_PATH=/root/bigtmp/x.toml; cargo run -q -- model agnes/agnes-image-2.1-flash; cargo run -q -- generate --prompt x --dry-run --json` |

## 2026-06-26 · 接入大陆 5 家 OpenAI 兼容 provider + 模板方言扩展(D-010/D-012)

- 测了什么: 扩展 openai_compat 模板支持方言(size 字段名 / 返回数组字段名 / catalog 别名),新接火山 Seedream / StepFun / 智谱 CogView / PPIO(A 类 drop-in)+ SiliconFlow(B 类方言)。
- 方法: 离线单测(纯函数 fixture,不打真实网络)+ 子进程退出码契约 + 冒烟。
- 结果:
  - build 绿;clippy `--all-targets -D warnings` 绿。
  - cargo test 全绿,73 passed(基线 68,新增 5:openai_compat 2 个方言单测、keys 1 个、catalog 1 个 8 家聚合、exit_code 1 个 5 家无 key)。
  - 方言单测: drop-in 请求用 `size`、解析 `data[]`;SiliconFlow 请求用 `image_size`、解析 `images[]`,且字段名混用取不到(证明确实参数化)。
  - catalog 聚合: 默认注册表含 8 家(fal/google/agnes + volcengine/stepfun/zhipu/ppio/siliconflow),别名 seedream/cogview/kolors 解析命中对应 provider。
  - 无 key: 5 家 generate 均非零退出 + 中文 key 指引(子进程,清空所有 key env)。
  - 冒烟 `cargo run -- model --json`(无 TTY): 8 家条目齐全,available 全 false(测试环境无 key);设 `ARK_API_KEY=dummy` 后 volcengine.available 翻 true,证明 available 跟随 has_key。
  - 冒烟 `cargo run -- providers`: 列出 8 家。
- 没覆盖什么(边界): 未打真实网络,未验证各家真实出图;PPIO images 端点路径前缀(/v3/openai 下是否再带 /v1)未离线证实,标 #uncertain,真实联调若 404 改 base_url 即可;各家 model id 取自 WebFetch/WebSearch 核实的当时值,可能随厂商更新漂移。
- 复现: `source ~/.cargo/env && export TMPDIR=/root/bigtmp && cargo clippy --all-targets -- -D warnings && cargo test`。

## 2026-06-26 · 配置写入防损坏加固(D-013 优先项② / atomic_write+备份+损坏防护)
- 测了什么: (1) atomic_write 往返/自动建父目录/无 .tmp 残留/覆盖完整; (2) 备份生成与轮转(写 N+2 次只剩 N=5 个, 保留最新时间戳); (3) prune 不误删无关文件与非数字后缀伪备份; (4) 损坏防护(坏 toml 加载返回 Err 且原文件零修改); (5) settings.save_to 走原子写+备份, 覆盖留备份、无临时残留。
- 方法: cargo clippy --all-targets -- -D warnings 全绿; cargo test 全绿; 新增单测 12 个(atomic.rs 8 + settings.rs 4)。
- 结果: TOTAL passed 73 -> 85, 0 failed; clippy 0 warning。
- 手动演示: IMAGECLI_CONFIG_PATH 指临时目录, 跨秒连续 `imagecli model agnes/agnes-image-2.1-flash` 写 7 次 -> 目录留 config.toml + 恰好 5 个 config.toml.bak.<unix秒>(最旧 2 个被轮转删), 验证主文件+轮转上限。
- 没覆盖什么(边界): (a) 真实 ENOSPC/掉电中断未做故障注入(靠 sync_all+rename 的语义保证, 非实测); (b) 同一 unix 秒内多次写只产生 1 个备份(秒级时间戳粒度, 同秒同内容, 设计如此); (c) 跨文件系统 rename 未测(atomic_write 刻意同目录, 不触发 EXDEV); (d) 文件锁未做(单用户 CLI, 并发写同一 config 罕见; 原子 rename 已保证不出半截文件, 仅"最后写者赢")。
- 复现: source ~/.cargo/env && export TMPDIR=/root/bigtmp && cargo test config::atomic && cargo test config::settings
## 2026-06-26 19:40 · 接入海外 provider: OpenAI 官方 + Replicate (D-011)
- 测了什么: 新增 openai.rs(gpt-image-1 drop-in 模板) + replicate.rs(C 类异步 prediction)。
- 方法: cargo build / clippy -D warnings / test 全绿; cargo run -- providers 与 model --json 冒烟。
- 结果: lib 测试 73 -> 88(+15, 只增不减), 全部 0 failed; clippy 0 warning; providers 列出 10 家; model --json 含 openai/gpt-image-1(alias gpt-image) 与 replicate/black-forest-labs/flux-schnell(alias flux-schnell)。
- 覆盖单测: OpenAI b64_json 走模板既有解析(parse_b64_json_path_to_inline_asset); Replicate 状态映射(starting->Queued/processing->Running/succeeded->Succeeded/failed,canceled,unknown->Failed)、请求体 input 包裹、output 三形态(字符串/字符串数组/对象数组)抽取、句柄 raw_meta roundtrip; keys 候选优先级(OPENAI_API_KEY/REPLICATE_API_TOKEN 优先)。
- 没覆盖(边界): 未打真实网络(无 key, 真实联调需主控注入 env); Replicate 视频模型 kind 仍按 Image; 本地图片输入需先上传(MVP 仅接受 URL 输入)。
- WebFetch 核实: Replicate(docs/reference/http)确认 Bearer 鉴权/官方模型 /v1/models/{owner}/{name}/predictions/status 五态/output 为 HTTPS URL; OpenAI(openai.com 403)经 Azure OpenAI 官方文档交叉验证: gpt-image-1 不支持 response_format 且永远返回 b64_json, size 取 1024x1024/1024x1536/1536x1024。


## 2026-06-26 · 视频地基: 通用 async-task 骨架 + video capability + Ark Seedance(D-014)

### 测了什么 / 方法 / 结果
- build: `cargo build` 绿。
- clippy: `cargo clippy --all-targets -- -D warnings` 绿(修了 6 处 doc 列表续行缩进告警, 见下"没覆盖")。
- test: `cargo test` 全绿, 116 passed / 0 failed / 1 ignored(原 95, 只增不减)。
  分布: lib 107, budget 3, cross_process 1(+1 ignored 旧有), exit_code 5。
- 冒烟(真实二进制, 全程离线无 key):
  - `imagecli providers` -> 11 家; seedance 标 `text2video, image2video`。
  - `imagecli --json model --list` -> seedance 两条目: t2v(alias seedance)/i2v(alias seedance-i2v), capabilities 正确。
  - `generate --provider seedance --capability text2video --prompt x`(无 key)-> 中文 key 指引(ARK_API_KEY/IMAGECLI_ARK_KEY/IMAGECLI_SEEDANCE_KEY), EXIT=1。
  - `generate --provider volcengine --capability text2video`(图像 provider 请求视频)-> "不支持能力 text2video。它支持: text2image" 清晰中文错误, EXIT=1。

### 新增单测(离线, 纯函数)
- transport/async_task: BearerAuth 头、StatusMapping 穷尽映射(大小写不敏感、未知->Failed)、TaskHandle raw_meta 往返与缺 query_url 报错、extract_urls_at(嵌套 pointer/数组/对象/兜底空)。
- providers/seedance: 能力声明(只 video 不 image)、状态映射穷尽、请求体构造(t2v 文本 content / i2v 追加 image_url / params 透传)、产物解析 content.video_url->Video、error.message 抽取、submit/task URL 拼装。
- config/keys: SEEDANCE 候选优先级(ARK_API_KEY>IMAGECLI_ARK_KEY>IMAGECLI_SEEDANCE_KEY)+ 全缺 None + 中文 hint。
- cli: ensure_capability_supported(支持通过/不支持报"不支持能力"列真实能力)、default_model_for seedance t2v/i2v 路由。
- catalog: 默认注册表聚合 11 家含 seedance, seedance alias 命中且声明 text2video。
- 集成(exit_code.rs): seedance text2video 无 key 非零退出+中文指引; 不支持能力组合非零退出+"不支持能力"。

### 没覆盖什么(边界, 诚实声明)
- 未打真实 Ark 网络: submit/poll/cancel 的真实 HTTP 往返、真实 task_id 形态、succeeded 后 content.video_url 实际字段路径均未联调(需 ARK_API_KEY 真跑一条 t2v 验证)。
- Seedance model id 的确切日期后缀(-250428)未与控制台核对, 以方舟控制台为准, 可 --model 覆盖。
- 视频产物 24h 过期: 仅靠"成功即走正常 download 落盘"覆盖, 未测过期后重下行为。
- DELETE 取消未联调(仅本地构造, 真实取消语义未验)。
- async-task 骨架的 JWT(可灵)/AK-SK V4(即梦)鉴权实现尚未落地, 仅留 TaskAuth 扩展点。

## 2026-06-26 · 接入可灵 Kling(JWT) + 即梦 jimeng(火山 V4 签名)

### 测了什么 / 方法 / 结果
- `cargo build`: 绿(新增 hmac/sha2/hex 已在 Cargo.toml, 无新增 crate)。
- `cargo clippy --all-targets -- -D warnings`: 绿(修了 jimeng 文档列表缩进 1 处)。
- `cargo test`: lib 134 passed(原 116, +18), 集成 budget 3 / cross_process 1 / exit_code 5 全绿, 0 failed。
- `providers --json`: 13 家; kling=[text2video,image2video], jimeng=[text2image]。
- 无 key 报错 + 退出码: kling/jimeng generate 无 key 各自中文 hint, exit=1。
- 双 key 校验: kling 仅给 AK 缺 SK -> 报 SecretKey 缺失 hint; has_key=false(models 标"缺 key")。
- `models --provider kling/jimeng`: 条目/alias/能力/成本标记正确。

### 离线单测覆盖(关键)
- kling JWT: 三段结构 + payload 解码 iss/exp/nbf; 固定 ak/sk/now 锁定完整 JWT 串(已知向量);
  状态映射 succeed->Succeeded 且 succeeded(多 ed)落 Failed; task_result.videos[] 解析; model_name 字段。
- jimeng V4: 完整签名比对 python 参考已知向量(Authorization + X-Content-Sha256);
  kSigning 中间值锁定; canonical request 哈希锁定; format_x_date 多时刻; query 排序编码;
  req_key 提交体 / 查询体; image_urls 与 binary_data_base64(inline 字节)双产物路径; 状态映射。

### 没覆盖什么(边界)
- 两家均未真实联调(无 key)。即梦 V4 仅离线锁定签名算法; 真实 canonical query 仅 Action+Version 两参,
  多参/特殊字符编码顺序未经真实端验证。可灵 i2v 默认 model(kling-v1-6)是否支持 i2v 未经真实验证。
- jimeng poll 走 submit_task(POST) 而非骨架 query_task(GET), 因即梦 GetResult 是 POST; 未联调验证响应解析。

## 2026-06-26 · D-006 工程加固: 多 provider 路由+故障转移 / 重试策略 / 可观测性

### 测了什么 / 方法
- 新增模块 `src/core/retry.rs`(错误重试分类)与 `src/core/route.rs`(候选链路由+故障转移+重试编排)。
- transport(http_sync/http_queue/async_task)非 2xx 改发结构化 `HttpError{status}`,供分类 downcast。
- CLI 新增 `--fallback`(逗号/可重复)、`--retries N`(默认 2)、`--verbose/-v`。
- 全程离线单测(假 provider 注入错误,fast RunConfig 极小退避);整机 build/clippy(-D warnings)/test 全绿。

### 结果
- 测试数: 由 143 增至 **159**(lib 150 + budget 3 + cross_process 1[+1 ignored] + exit_code 5)。
- `cargo clippy --all-targets -- -D warnings`: 通过(0 warning)。
- 路由/重试单测(core::route::tests, 7 条):
  - primary_fails_falls_back_to_secondary: 主 nonretryable 失败→切备成功, provider_used=备, fallback_from=[主]。
  - nonretryable_is_not_retried: 不可重试错误主只 submit 1 次即切备(retries=3 也不浪费)。
  - retryable_retries_n_times_then_fails: retries=2 → 共 3 次 submit。
  - retryable_then_succeeds_no_fallback: 前 2 次 503 后成功, 不切 fallback。
  - poll_failure_retries_poll_not_submit: submit 仅 1 次、poll 3 次(幂等: poll 失败重试 poll 不重 submit)。
  - all_candidates_fail_reports_last_and_fallback_chain。
- 分类单测(core::retry::tests, 6 条): 429/408/5xx 可重试; 401/403/4xx 不可重试; HttpError 经 context 包裹仍可 downcast; 文本启发式; 配额检测。

### 实证(子进程二进制, 离线/真实混合)
1. 故障转移 `generate --provider google --fallback agnes --prompt x`(均无 key)→ --json:
   attempts=2, fallback_from=["google"], provider_used="agnes", model_used="agnes-image-2.1-flash", exit=1。
2. verbose(stderr)`--fallback agnes,replicate --verbose`: 打印候选链
   `google -> agnes -> replicate`、每阶段 [trace] provider/model/phase/attempt/耗时/(不可重试) 与 fallback 切换事件;
   --json stdout 不被污染。
3. 重试分类实证: agnes dummy key 真实请求 → HTTP 401 → 判"不可重试" → attempt=1(不浪费重试), 证明分类生效。
   (可重试路径 429/5xx/超时/网络的退避重试由 route 单测确定性覆盖。)
4. 能力跳过: t2i 链 `--fallback seedance,nope,agnes` → skipped_fallbacks=[seedance:不支持该能力, nope:未注册], 最终用 agnes。

### 没覆盖什么(边界)
- 未打真实成功出图的端到端(无可用付费/免费 key 的真实成功响应);可重试错误的真实网络退避未在 CLI 层做 e2e(靠单测覆盖)。
- elapsed_ms 在无网纯失败路径常为 0(未引入 mock 时延)。
- 取消(cancel)与重试/路由的交互未覆盖(取消仍为尽力而为, 本次未触及)。

## 2026-06-26 · 本地图 i2i 输入(--input 接受本地路径)

### 做了什么
- `--input` 现判别本地路径 vs URL: URL 维持 from_url; 本地文件读取字节按扩展名推断 mime 存为 inline 字节素材(load_input_asset)。
- 新增 `Asset::as_input_image() -> InputImage{Url|Bytes}` 归一喂图形态; `InputImage::as_raw_base64`(即梦/可灵用)与 `to_image_field_string`(URL 原样 / 字节拼 data URI, Seedream 用)。
- jimeng: 加 Image2Image 能力 + i2i 默认 model(同 jimeng_t2i_v40); build body 把图片输入分流 image_urls[URL]/binary_data_base64[本地]。
- kling: image2video 的 image 字段扩展为接受 raw base64(本地图), URL 仍透传。
- openai_compat: 新增 supports_i2i 配置项(volcengine=true 其余 false); true 时 caps 加 Image2Image、catalog 体现、build body 在 capability=Image2Image 时塞 image(单图字符串/多图数组, 远程 URL 原样 / 本地拼 data URI)。
- cli default_model_for: jimeng/volcengine 的 Image2Image 默认 model 接通。

### 测试(离线)
- 方法: cargo test / clippy -D warnings / build, 全绿。测试数 159 -> 174(+15)。
  - provider.rs: as_input_image URL/inline/local-path-None + to_image_field_string/as_raw_base64(4)。
  - cli: is_remote_url 判别 + mime_from_path + load_input_asset(URL/本地/缺文件)(5)。
  - jimeng: caps 含 i2i + 本地->binary_data_base64 + URL->image_urls(3)。
  - kling: 本地->raw base64(1)。
  - openai_compat: i2i caps 声明 + 本地->data URI image + supports_i2i=false 忽略输入(共 +新增, t2i 用例加 image 缺失断言)。
- 结果: test 174 passed / 0 failed; clippy 0 warning; build ok。

### 冒烟(无 key, 证明本地图被读取并路由到 i2i)
- `generate --capability image2image --input /root/bigtmp/smoke.png(69B 1x1 png) --provider jimeng --prompt 改成水彩`
  -> "未找到即梦 visual 的 AccessKeyId"(本地图已 load 成 inline 素材, 能力/默认 model 路由通过, 止于缺 key)。
- volcengine(Seedream) i2i 本地图 -> 止于 "未找到火山引擎方舟的 API key"。
- kling image2video 本地图 -> 止于 "未找到可灵 Kling 的 AccessKey"。
- 缺文件: `--input ./nope.png` -> "输入素材既不是 http(s) URL, 本地也不存在该文件"。
- 不支持 i2i 的家(agnes): "provider agnes 不支持能力 image2image。它支持: text2image"。
- catalog/providers: jimeng、volcengine 均显示 capabilities=[text2image, image2image]。

### 没覆盖的边界
- 三家 provider 的 submit 都"先查 key 再 build body", 故冒烟止于缺 key、未真正打网络验证 body 被服务端接受; body 编码由纯函数单测覆盖。
- fal/replicate 本地图未覆盖(仍需用户自传成 URL); 这两家走 storage upload 路径, 本轮未实现(Uploader trait 仍为占位)。
- 真实带 key 的端到端出图未验证(离线环境无 key)。

## MCP server (stdio JSON-RPC 2.0) · 2026-06-26

### 测了什么
- 新增 `imagecli mcp` 子命令: stdio 上的 JSON-RPC 2.0 MCP server, 自实现(未引 rmcp)。
- 暴露 6 工具: generate_image / generate_video / list_providers / list_models / get_job / list_jobs。

### 方法与结果
- build: `cargo build` 绿(新增 tokio io-std/io-util 特性以支持 stdin/stdout)。
- clippy: `cargo clippy --all-targets -- -D warnings` 全绿, 0 warning。
- test: `cargo test` 全绿。lib 单测 165 -> 180(+15 MCP 单测), 集成新增 tests/mcp.rs(+1)。
  总计 190 passed / 0 failed / 1 ignored(原网络测试), 对比基线 174 passed 只增不减。
- MCP 单测覆盖: initialize 回显 protocolVersion + serverInfo; tools/list 含 6 工具且 schema 合法
  (type=object/有 properties/生成类描述含"消耗"+"环境变量"/prompt 必填); ping 返回空 result;
  未知方法 -> -32601; 通知(无 id)无响应; 坏 JSON -> -32700; 未知工具/缺 job_id -> -32602;
  list_providers/list_models(按 provider 过滤)/list_jobs(空库)/get_job(未命中 found=false) 路由正确;
  build_generate_args 纯函数(默认能力/必填 prompt/provider/model/size/input/params/out_dir/dry_run 透传/能力覆盖)。
- 手动冒烟(stdio 握手): `printf 三行 JSON-RPC | cargo run -- mcp` ->
  initialize 返回 {serverInfo:{name:imagecli,version:0.1.0}, capabilities:{tools:{listChanged:false}}, protocolVersion 回显 2025-06-18};
  tools/list 返回 6 工具完整 schema。
  tools/call generate_image(dry_run=true) -> isError=false, structuredContent.dry_run=true, estimated_cost=0(离线、零额度);
  tools/call list_providers -> providers[] 带 available。
- 集成冒烟 tests/mcp.rs: 子进程启动 `imagecli mcp`, 清空所有 key, 喂 initialize+通知+tools/list+generate_image(dry_run),
  断言正好 3 条响应(通知不回应)、6 工具齐全、dry_run 调用 isError=false。

### 没覆盖什么(边界)
- 未与真实 MCP 客户端(Claude Code / Cursor)端到端联调; 仅以原始 JSON-RPC 字节验证协议层。
- 生成类工具的真实出图/出视频链路未在 MCP 路径下打网络(沿用 cmd_generate 子进程, 其本身已有联调记录);
  本轮 MCP 测试一律 dry_run / 只读, 不消耗额度。
- 未测超大请求分帧/并发多请求交错(server 串行处理, MCP 不要求并发)。

### 复现
- `source ~/.cargo/env && export TMPDIR=/root/bigtmp`
- `cargo clippy --all-targets -- -D warnings && cargo test`
- 握手冒烟: `printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | cargo run -q -- mcp`
