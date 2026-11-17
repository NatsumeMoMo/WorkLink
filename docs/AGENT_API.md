# WorkLink Agent 接口 v1

v0.2 新增浏览器、桌面、持久自动化任务与 MCP，详见 [AUTOMATION.md](AUTOMATION.md)。自动化输出默认预算为 4000 字符/30 项，长正文按结果 ID 分页，图片显式下载。

优先使用 CLI；不要操作 GUI，也不要读取或解密设备私钥。固定安装位置后，先执行 `WorkLink.exe --version` 和 `devices --json`，确认本机后台与目标在线。

## 调用约定

- 全局参数 `--data-dir <目录>` 选择设备身份；默认 `%ProgramData%\WorkLink`。
- 命令使用 UTF-8 JSON 写 stdout；原始 SSH `proxy` 的 stdout 只允许二进制数据。
- 进程退出码 0 表示成功；SSH 命令返回 SSH/远端进程退出码。原生任务失败、取消、超时、中断返回非零。
- 基础调用失败返回 `{"ok":false,"error":"具体原因"}`。原生任务返回任务对象，其 `state`、`exit_code` 和 `error` 表示执行结果。
- SSH 是默认后端，执行使用 `BatchMode=yes`，依赖已配置的主机信任和 SSH 密钥认证。
- 不根据认证失败自动选择 native；只有调用方明确指定 `--backend native` 才使用兜底授权。
- `--system` 仅适用于 native，并需要目标授予 system 和相应 exec/files 权限。
- `put/get` 默认禁止覆盖；需要时显式添加 `--overwrite`。断点续传自动尝试。

## 命令索引

| 命令 | 用途 |
|---|---|
| `devices --json` / `status --json` | 本机信息、设备身份、在线状态、入口和最近错误 |
| `exec <设备> --command <PowerShell脚本> --user <SSH用户> --json` | 默认 SSH 执行 |
| `exec <设备> --backend native --command <脚本> --json` | 原生持久任务，等待结束 |
| `exec ... --backend native --detach --id <UUID>` | 原生提交后立即返回，ID 可由调用方预先生成 |
| `exec ... --timeout <秒> --cwd <目标工作目录>` | 超时范围 1–86400 秒 |
| `job list <设备> --json` | 当前设备创建的最近任务摘要 |
| `job get <设备> <UUID> --json` | 状态和已保存输出；`--no-output` 只取状态 |
| `job cancel <设备> <UUID> --json` | 请求终止任务进程树 |
| `put <设备> <本机文件> <目标绝对路径> --json` | 上传并验证 SHA-256 |
| `get <设备> <源绝对路径> <本机文件> --json` | 下载并验证 SHA-256 |
| `terminal <设备> [--backend native]` | 人工交互终端，Agent 应优先使用 exec |
| `proxy --device <设备> --service ssh` | OpenSSH ProxyCommand，原始字节流 |
| `forward <设备> --bind 127.0.0.1:22001` | 为客户端保持本机 SSH 转发入口 |
| `ssh-config <设备> --user <用户名>` | 输出 SSH 配置，不自动修改用户文件 |
| `peer ssh <设备> --user <用户> --identity <本机私钥路径> --known-hosts <本机文件>` | 保存该设备的本机 SSH 客户端设置；`--clear` 清除 |
| `browser <设备> [--background] <status/open/tabs/snapshot/click/fill/press/text/wait/ask/screenshot/close>` | 使用目标网络和专用浏览器；`--id UUID`、`--detach`、`--timeout`、`--max-chars`、`--max-items` |
| `desktop <设备> <windows/launch/snapshot/invoke/fill/focus/screenshot>` | 活动且解锁的所有者桌面 UIA |
| `tool <设备> [--background] <get/cancel/read/download>` | 查询持久自动化任务、分页文本或校验下载图片 |
| `mcp` | stdio MCP，5 个紧凑工具；GUI 可复制配置 |

设备参数接受完整设备 ID 或不区分大小写的设备名。

## 原生任务结果

```json
{
  "spec": {
    "id": "调用方生成或程序返回的UUID",
    "program": "目标PowerShell路径",
    "args": ["编码后的调用参数"],
    "cwd": null,
    "timeout_secs": 3600,
    "system": false
  },
  "owner_peer": "发起设备ID",
  "state": "completed",
  "created": 0,
  "finished": 0,
  "exit_code": 0,
  "error": null,
  "stdout": "结果",
  "stderr": "",
  "output_truncated": false
}
```

状态：`queued`、`running`、`completed`、`failed`、`cancelled`、`timed_out`、`interrupted`。`completed` 仍需检查 `exit_code`。

调用超时或连接丢失时，先查询原有 UUID。不要生成新 UUID 盲目重复执行有副作用的命令。记录已被清理后的 UUID 不再提供去重保证。

## SSH 结果

```json
{"backend":"ssh","exit_code":0,"stdout":"...","stderr":"","output_truncated":false}
```

SSH 命令超时会结束本地 SSH 调用，但远端命令状态需要核实。要求断线后可恢复查询的任务，应明确使用原生持久任务。

## 文件结果

成功结果包含 `path`、`size`、`verified:true`，下载和 SSH 上传还包含 `sha256`。原生传输包含 `resumed_from`。发生中断时保留 `.worklink*.part`，完整性失败时删除或清零损坏临时文件以便重试。SSH 上传发现临时文件已完整时会先核验，通过后直接提交；损坏的等长临时文件会重新上传。

Windows 目标路径使用绝对路径，例如 `C:\Temp\app.zip`。原生通道限制为本机盘符路径，不支持 UNC、设备命名空间或 NTFS 备用数据流。文件父目录需要已存在。
