# 即梦 CLI 体验指南

> 最新修改时间为 06月24日

> ✅ 这份指南面向第一次使用即梦 CLI 的用户。评论排查问题前请先给出执行的命令，报错的描述位于 ~/.dreamina_cli/logs/ 下的日志。优先更新 CLI 并重试命令，你的问题很可能在新版本已经解决。业务相关问题、功能提需请找 🟠 用户1474

---

## 一、即梦 CLI 是什么

即梦 CLI 是面向 Agent 和自动化工作流的本地命令行工具。安装后，你可以在终端或 Agent 环境里调用即梦的图片生成、视频生成、结果查询、任务历史和账户查询能力。

### 👍 适合使用它的场景

- 让 Agent 帮你批量生成图片或视频
- 把即梦生成能力接入脚本、自动化流程或测试流程
- 保存 submit_id，稍后继续查询异步任务结果

### 👍 使用前需要知道

- 生成任务会消耗账户权益或积分，目前仅供高级会员以上可用
- 您使用即梦 CLI 生成内容所需消耗的积分，与即梦网页端 Agent 模式下相同生成能力所消耗的积分标准一致，具体以产品规则及积分消耗记录为准
- 大多数生成任务是异步任务，提交和查询是两个步骤

---

## 🔥 更新日志（Update）

更新方式：终端运行命令 `curl -fsSL https://jimeng.jianying.com/cli | bash`

CLI 入口移动至页面左下 CLI 图标处

**【v1.4.8 | 2026-06-18】🆕**
- 新增：支持 seedance 2.0 mini 模型
- 完善了即梦 CLI 配套的 Skill 文件

**【v1.4.4 | 2026-06-03】**
- 新增：支持 seedream 4.7 模型

**【v1.4.3｜2026-05-07】**
- 新增：支持 seedance 2.0 vip 模型以 1080p 分辨率生视频

**【v1.4.1｜2026-04-17】**
- 安全：登录方式更新

**【v1.4.2｜2026-04-22】**
- 修复：修复上传多图时的超时问题

**【v1.3.5｜2026-04-16】**
- 新增：支持多对话工作空间（Session）的增删查改、搜索，并可在指定会话中执行生成任务

**【v1.3.4｜2026-04-10】**
- 优化：优化生图，全能参考命令帮助文案，现在支持 linux arm64 平台了

**【v1.3.3｜2026-04-07】**
- 优化：修复了超清图片任务一直处于排队的问题

**【v1.3.2｜2026-04-05】**
- 新增：支持 seedance2.0fast_vip 以及 seedance2.0_vip 通道提速，畅快生成

**【v1.3.1｜2026-04-04】**
- 新增：自动更新检测能力（CLI 启动时提示新版本）
- 优化：登录流程稳定性提升，新版本下载后可以自动提示更新了

---

## 二、快速上手

**给 Agent 的一句话指令：** 你可以把下面这段直接发给 Agent，让它代你完成安装和登录流程。

> 可复制给 Agent（Plain Text）：
>
> ```
> 请帮我安装并登录即梦 CLI：先执行 curl -fsSL https://jimeng.jianying.com/cli | bash，然后运行 dreamina -h 确认命令可用，再运行 dreamina login。登录时请把终端输出的 verification_uri 和 user_code 发给我，并等登录命令结束后告诉我是否成功。
> ```

**如果你只想最快跑通，从这里开始：** 下面的命令默认安装官方发布版本，安装后的命令名是 `dreamina`。

| 步骤 | 命令 | 成功标准 |
| --- | --- | --- |
| 安装 | `curl -fsSL https://jimeng.jianying.com/cli \| bash` | 终端提示安装完成，并展示可执行文件名 dreamina |
| 确认可用 | `dreamina -h` | 能看到命令帮助和子命令列表 |
| 登录 | `dreamina login` | 按终端提示完成 OAuth 授权，看到登录成功或复用登录态 |
| 自检 | `dreamina user_credit` | 能返回当前账户信息和积分信息 |

> 💡 如果安装后提示 `dreamina: command not found`，通常是 PATH 还没在当前终端生效。请重启终端，或按安装脚本提示临时执行 export PATH 命令后再试。

---

## 三、登录与账户检查

**标准登录流程：**

1. 运行 `dreamina login`。
2. 打开终端输出的 `verification_uri`。
3. 按页面提示输入 `user_code` 并确认授权。
4. 回到终端等待命令结束。
5. 运行 `dreamina user_credit` 做登录自检，预期会返回剩余积分、user id、会员等级信息。

**登录相关命令：**

| 命令 | 用途 | 说明 |
| --- | --- | --- |
| `dreamina login` | 登录或复用已有登录态 | 终端会打印 verification_uri、user_code、device_code，并等待你完成授权 |
| `dreamina login --headless` | 只输出授权材料，不阻塞等待 | 适合 Agent 或无交互环境；随后用 checklogin 查询授权结果 |
| `dreamina login checklogin --device_code=设备码 --poll=30` | 检查 headless 登录是否完成 | poll 是最多等待秒数；0 表示只检查一次 |
| `dreamina relogin` | 清除本地 OAuth 登录态并重新登录 | 切换账号时使用 |
| `dreamina logout` | 退出登录 | 只清除本地 OAuth 登录态，不删除任务记录和配置文件 |

---

## 四、常用生成命令

生成命令通常会先提交任务。如果加上 `--poll=30`，CLI 会提交后最多等待 30 秒查询结果；如果超时仍未完成，会返回 querying 状态和 submit_id，你可以稍后用 query_result 查询。

> ❗ **提交前请先确认：** 生成任务可能消耗积分；命令里的本地图片、视频、音频路径必须是当前机器能访问到的文件路径；不确定参数时优先运行 `dreamina 子命令 -h`。

| 任务 | 命令 | 关键参数 |
| --- | --- | --- |
| 文生图 | `text2image` | --prompt、--ratio、--resolution_type、--model_version |
| 图生图 | `image2image` | --images、--prompt、--ratio、--resolution_type |
| 文生视频 | `text2video` | --prompt、--duration、--ratio、--video_resolution |
| 图生视频 | `image2video` | --image、--prompt、--duration、--video_resolution |
| 多帧视频 | `multiframe2video` | --images、--prompt；3 张以上图片可使用 transition 参数 |
| 全能参考视频 | `multimodal2video` | --image、--video、--audio、--model_version |
| 图片超清 | `image_upscale` | --image、--resolution_type；4k 和 8k 需要 VIP |

### 图片生成示例

文生图：

```bash
dreamina text2image --prompt="一只戴墨镜的橘猫" --ratio=1:1 --resolution_type=2k --poll=30
```

图生图：

```bash
dreamina image2image --images ./input.png --prompt="改成水彩风格" --resolution_type=2k --poll=30
```

### 视频生成示例

文生视频：

```bash
dreamina text2video --prompt="镜头推进，一只橘猫从沙发上跳下来" --duration=5 --ratio=16:9 --video_resolution=720p --poll=30
```

图生视频：

```bash
dreamina image2video --image ./first_frame.png --prompt="镜头慢慢推近" --duration=5 --poll=30
```

多帧视频：

```bash
dreamina multiframe2video --images ./a.png,./b.png --prompt="角色从白天走到夜晚" --duration=3 --poll=30
```

全能参考视频：

```bash
dreamina multimodal2video --image ./input.png --audio ./music.mp3 --prompt="生成一段电影感短片" --model_version=seedance2.0fast --duration=5 --poll=30
```

### 查询和下载结果

查询异步任务：

```bash
dreamina query_result --submit_id=你的_submit_id
```

查询并下载图片或视频：

```bash
dreamina query_result --submit_id=你的_submit_id --download_dir=./downloads
```

查看历史任务：

```bash
dreamina list_task --gen_status=success
```

---

## 五、Session：把生成任务放进不同会话

Session 可以理解为生成任务所属的工作空间。默认 session 是 0；如果你要按项目隔离任务，可以创建新的 session，并在生成命令里传 `--session`。

| 目标 | 命令 |
| --- | --- |
| 创建 session | `dreamina session create "项目名"` |
| 查看最近 session | `dreamina session list` |
| 搜索 session | `dreamina session search "关键词"` |
| 重命名 session | `dreamina session rename session_id "新名字"` |
| 删除 session | `dreamina session delete session_id` |

在指定 session 中生成：

```bash
dreamina text2image --session=123456 --prompt="一张产品海报" --ratio=16:9 --poll=30
```

---

## 七、本地文件和日志

CLI 会在用户主目录下维护一些本地文件，用于任务记录、日志、版本检查和 Agent skill。

| 路径 | 说明 |
| --- | --- |
| `~/.dreamina_cli/tasks.db` | 本地任务记录数据库，用于 list_task 和后续查询 |
| `~/.dreamina_cli/logs/` | 运行日志目录；排查问题时优先提供这里的相关日志 |
| `~/.dreamina_cli/version.json` | 安装脚本写入的版本信息，用于更新检测 |
| `~/.dreamina_cli/dreamina/SKILL.md` | 安装脚本下载的 Agent 使用说明 |

> ❌ 不要把日志、配置或终端输出中的敏感信息随意发到公开渠道。排查问题时，请优先提供执行命令、错误描述、CLI 版本和相关日志片段。

---

## 八、常见问题

### 1. 安装后找不到 dreamina 命令

- 重新打开终端后再运行 `dreamina -h`。
- 如果安装脚本提示了临时 PATH 命令，先执行那条 export PATH 命令。
- 仍失败时，重新执行安装命令。

### 2. 登录一直没完成

- 确认你打开的是终端输出的 `verification_uri`。
- 确认输入的是同一轮登录输出的 `user_code`。
- 如果使用 `--headless`，请用 `dreamina login checklogin --device_code=设备码 --poll=30` 检查结果。
- 如果过期，重新执行 `dreamina login` 或 `dreamina relogin`。

### 3. 生成命令提示未登录或无权限

- 先运行 `dreamina user_credit`。如果这个命令失败，先处理登录或账号权限问题。
- 如果返回 `AigcComplianceConfirmationRequired`，请先去即梦 Web 端完成对应授权确认，再重试 CLI 命令。

### 4. 任务长时间没有结果

- 提交命令建议带上 `--poll=30`，先等待一段时间。
- 如果返回 querying，请保存 submit_id，稍后运行 `dreamina query_result --submit_id=你的_submit_id`。
- 可以用 `dreamina list_task` 查看最近保存的任务。

### 5. 如何反馈问题

请一次性提供下面信息，研发同学会更容易定位。

- 你执行的完整命令
- 终端返回的错误信息
- CLI 版本：运行 `dreamina version`
- 相关日志：位于 `~/.dreamina_cli/logs/`
- 如果是生成任务，请附上 submit_id

---

## 九、推荐工作流

新用户建议先按最小闭环跑通，再逐步尝试复杂能力。

1. 安装并确认 `dreamina -h` 可用。
2. 执行 `dreamina login` 完成登录。
3. 执行 `dreamina user_credit` 确认账户可用。
4. 先跑一个低成本的 `text2image` 测试。
5. 保存返回的 submit_id，并用 `query_result` 查询或下载。
6. 确认流程稳定后，再尝试视频、多帧、全能参考或批量任务。

> 💡 如果你已经能完成安装、登录、user_credit、一次 text2image 和一次 query_result，说明即梦 CLI 的基础链路已经跑通，可以开始让 Agent 接入更复杂的创作流程了。
