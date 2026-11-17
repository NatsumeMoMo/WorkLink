# 浏览器、桌面与 MCP（v0.2）

WorkLink 可以从 B 操作 A 的专用浏览器、使用 A 的网络加载网页、提取回答，并操作 A 当前用户桌面的 UI Automation 控件。两个方向使用已有 TLS 双向认证长连接和同一个公网端口；无需开放 CDP、WebDriver、RDP 等公网入口。

## 授权与运行方式

在目标电脑给发起设备分别授权：

```powershell
.\WorkLink.exe peer allow B --browser true --desktop true
```

旧配置升级后两项权限默认关闭。GUI 的设备页也可修改；撤回后执行中的任务会停止等待，已发送的问题或已经发生的点击不会回滚。

系统服务负责连接，按需以配置所有者的普通用户身份启动同一个 EXE 的会话助手。headless 浏览器同样保留所有者会话，避免 Chromium 在 Session 0 中出现渲染超时。桌面和可见浏览器需要所有者有活动会话；桌面操作另需解锁。助手不自动提升 UAC，不操作安全桌面，也不读取密码控件。

浏览器默认可见；`--background` 使用另一个独立的 headless 配置。两种模式的任务和登录状态独立。服务自动创建后台助手仍需要一个现存的所有者登录令牌；不能据此宣称冷启动后无人登录时能新建浏览器。SSH、SYSTEM 原生任务的能力继续由原有服务和授权决定。

使用已安装的 Edge，未找到时使用 Chrome；不捆绑浏览器。浏览器仅在 open 时启动，关闭 GUI 不影响任务。UI Automation 的 PowerShell 工作进程空闲约 60–75 秒回收，旧控件引用随之失效；重新 snapshot 即可。

## 最小浏览器流程

```powershell
.\WorkLink.exe browser A status
.\WorkLink.exe browser A open https://example.com
# 从结果取 tab；实际网站用 selector 或 text 等待有意义的就绪条件
.\WorkLink.exe browser A wait <tab> --text 'Example Domain'
.\WorkLink.exe browser A snapshot <tab> --max-items 20 --max-chars 3000
.\WorkLink.exe browser A fill <tab> <ref> '要输入的内容'
.\WorkLink.exe browser A click <tab> <ref>
.\WorkLink.exe browser A text <tab> --selector main
```

短 ref 来自最近一次 snapshot；导航、刷新、控件移除或另一个快照会使它失效。收到 `ref_expired` 后重新 snapshot。点击前检查可见、禁用和遮挡；只读探测遇导航更换脚本上下文时允许有限重试，输入和点击不自动重放。当前只处理页面主框架，跨域 iframe、原生文件选择器、验证码等需要人工或其它明确支持的工具。

浏览器由 WorkLink 的专用持久 profile 管理，不接管普通浏览器，也不复制 cookie。第一次在目标电脑的 WorkLink 可见浏览器里手动登录，后续沿用该 profile。远程网页访问使用目标电脑当时的浏览器网络配置；本工具不提供通用 SOCKS/VPN，也不自动复制当前浏览器的代理扩展。

## 网页提问与长任务

```powershell
.\WorkLink.exe browser A open https://chatgpt.com/
# 在 A 的新浏览器人工登录、选择所需模型，再使用该页 tab
.\WorkLink.exe browser A ask <tab> '请回答这个问题' --id <UUID> --timeout 900 --detach
.\WorkLink.exe tool A get <UUID> --wait-ms 30000
```

ask 默认使用 ChatGPT 的输入、发送、assistant 消息和生成状态标记；也支持 `--input`、`--send`、`--answer` 自定义 CSS selector。它保护已有草稿，上一条生成中时拒绝新发送，并等待新 assistant 内容出现、生成标记消失、文本至少稳定约 5 秒。网页结构或 Pro 思考状态可能变化；实际账户和模型应单独验收，不能把测试页面通过当作所有 ChatGPT Pro 场景保证。

每个任务在执行前保存 UUID。相同 ID、相同参数查询原记录，不重复执行；参数不同报 `id_conflict`。必须保留原 ID、background 模式与用户会话。响应丢失会返回 `submission_unknown` 和 ID；先查询，必要时只以同一 ID 和参数重试。`--timeout` 为 1–3600 秒，MCP 默认 120 秒，长回答显式设大。

断开连接不取消远端任务。助手崩溃后未完成任务标为 `interrupted`，不自动重放。取消仅停止等待和后续步骤，不撤销已发生操作。`error.may_have_applied` 表示需要检查网页或应用当前状态。任务 ID 去重窗口为 24 小时；单助手最多保留 10000 个窗口内记录，达到容量拒绝新任务。过期记录清理后 ID 不再保证去重。

## 输出、分页和图片

默认操作预算 4000 字符、30 个控件，可设置 512–16000 字符和 1–100 项。预算约束正文或快照，不包含外层任务/JSON-RPC 包装，字符数也不等于模型 Token 数。大页面不会完整返回 DOM。text/ask 返回最多约 3200 字符的预览以及 `result_id`、`total_chars`、`next_offset`。

```powershell
.\WorkLink.exe tool A read <result_id> --offset 3200 --limit 2000
.\WorkLink.exe tool A read <result_id> --query '关键结论' --limit 1500
.\WorkLink.exe browser A screenshot <tab>
.\WorkLink.exe tool A download <result_id> .\page.jpg
```

read 的偏移与长度按 Unicode 字符，搜索最多返回 10 个匹配片段。图片按需生成 JPEG，最长限制为 1280×900，普通工具响应不含 Base64。download 内部分块读取并校验 SHA-256，成功后原子保存，拒绝覆盖现有文件。桌面截图捕获窗口所在屏幕区域，窗口应无遮挡；它不保证能获取被其它窗口覆盖的内容。

结果按发起设备隔离，每项最大 2 MiB，24 小时过期，总计最多 200 项/128 MiB，容量回收可能提前使旧结果失效。结果与浏览器 profile 存在受保护的数据目录；其内容可能含登录态或网页数据，不应提交 Git。`source_truncated` 表示源内容触及截取上限。

## 桌面控件

```powershell
.\WorkLink.exe desktop A windows
.\WorkLink.exe desktop A snapshot <window> --max-items 25
.\WorkLink.exe desktop A fill <ref> '内容'
.\WorkLink.exe desktop A invoke <ref>
.\WorkLink.exe desktop A screenshot <window>
.\WorkLink.exe desktop A launch C:\Windows\System32\notepad.exe -- C:\Temp\notes.txt
```

只在当前所有者会话操作；使用 UIA 的 Value、Invoke、Toggle、SelectionItem 模式。应用未实现 UIA、进程权限高于助手、桌面锁定或控件提供程序无响应时会明确失败。UIA 隔离工作进程超时后重建，不让挂起的提供程序持续占据服务线程。新快照和工作进程回收会使旧 ref 失效。

## Cursor / Codex / 其它 MCP 客户端

支持 MCP stdio，每行一个 JSON-RPC 消息。GUI 可复制 MCP 配置。配置示例：

```json
{
  "mcpServers": {
    "worklink": {
      "command": "D:\\Tools\\WorkLink.exe",
      "args": ["--data-dir", "C:\\ProgramData\\WorkLink", "mcp"]
    }
  }
}
```

Codex TOML 配置：

```toml
[mcp_servers.worklink]
command = 'D:\Tools\WorkLink.exe'
args = ['--data-dir', 'C:\ProgramData\WorkLink', 'mcp']
```

工具仅五个：`worklink_devices`、`worklink_browser`、`worklink_desktop`、`worklink_task`、`worklink_result`。browser/desktop 使用 `action.kind` 与参数，默认等待最多 1500ms，未完成则返回任务 ID；用 task 的 `get` 长轮询，避免频繁读 DOM。result 下载只返回本机路径和校验信息，不重复返回二进制或大段 structuredContent。需要把图片送给视觉模型时，由调用方按返回路径显式读取。

CLI 自动化命令始终输出紧凑 JSON。终态失败、取消、超时、中断返回非零退出码，查询这些任务时也如此；拿到进程输出后仍需检查 `state`。MCP 不自动修改权限或提升进程。

## 开发验证

```powershell
cargo test --locked -- --nocapture
py -3 scripts/test-automation.py --exe target/debug/WorkLink.exe --desktop
```

第二项使用临时身份、真实已安装浏览器、本机 HTTP 流式聊天页面和自建 WinForms 窗口；不访问真实聊天账户。测试结束只关闭本次 profile 和测试窗口。
