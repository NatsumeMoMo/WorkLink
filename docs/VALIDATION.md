# WorkLink 0.1.0 验证记录

日期：2026-11-13（Asia/Shanghai）。目标平台：Windows 11 x64。

结论：已生成可运行的单 EXE，发布版通过本机双端联调、真实 Windows OpenSSH/SFTP、原生 ConPTY 和故障恢复验证。Windows 服务代码已经编译，服务安装、无人登录及 SYSTEM 执行仍需在管理员环境验证；公网 frp/NAT4 链路需在实际入口配置后验证。本记录不代表长期稳定性已经得到证明。

## 发布文件和环境

- 文件：`dist\WorkLink.exe`，8,892,928 字节，约 8.48 MiB。
- SHA-256：`b139c7a0ca2de218efd3e46308c818b2cce8f5a4f421c06d64b642e0fc12dc9e`。
- 验证系统：Windows 11 Enterprise LTSC，版本 10.0.26100。
- Rust/MSVC 构建，静态链接 C 运行库；`dumpbin /dependents` 显示静态导入均为 Windows 系统 DLL，无独立 VC++ 运行库依赖。
- GUI 使用 Direct3D 12 后端，实际启动发布版、自动截图并人工检查中文与深色布局。截图为 [worklink-ui.png](worklink-ui.png)。

## 已通过的自动验证

`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings` 和全部测试通过：2 个单元测试、2 个集成测试。集成测试将多种真实操作串成完整场景，以下各行不是单独的测试函数数目。

使用 `WORKLINK_TEST_EXE` 指向最终发布版 EXE，再次运行两个集成测试，2 个通过，0 个失败，用时约 35.74 秒。实际 OpenSSH 验证均已执行，没有因权限条件跳过。

| 范围 | 实际验证内容 |
|---|---|
| 本地命令接口 | PowerShell 直接调用的 stdout、JSON 错误和非零退出码；Windows 参数引用和路径约束 |
| 身份保护 | DPAPI 加密/解密；错误配对指纹被拒绝 |
| 双向连接 | B 不监听入站；TLS 1.3 双向证书认证；同一连接上同时进行两个方向的二进制 SSH 转发 |
| 默认权限 | 原生命令和文件操作默认被拒绝；未授权 SYSTEM 请求被拒绝 |
| 原生执行 | 使用当前用户令牌执行命令；持久任务 UUID 去重；相同 UUID 对应不同命令时拒绝 |
| 交互终端 | 创建真实 Windows ConPTY，在加密通道中写入 PowerShell 命令并读取结果 |
| 原生传输 | Unicode 路径、2 MiB 文件、预置部分数据续传、实际续传偏移、完整 SHA-256、默认禁止覆盖 |
| 损坏数据恢复 | 损坏临时文件不会提交；重试能够恢复 |
| 调用端重启 | 杀死并重启 B 后台；A 上任务继续执行；重新查询结果；副作用只出现一次 |
| 接收端重启 | 杀死并重启 A 后台；B 自动重连；未结束任务标为 interrupted，不自动重跑 |
| 终止任务 | 超时和显式取消；通过 Job Object 约束进程树 |
| 真实 SSH | 独立启动系统自带 sshd，使用临时主机密钥；ssh-keyscan 经 WorkLink 得到预期密钥；通过 ProxyCommand 完成公钥认证和实际远程执行 |
| 真实 SFTP | 上传/下载中文文件名和空文件；默认拒绝覆盖；显式覆盖正确提交；损坏的部分/完整临时文件恢复；完整临时文件直接校验提交；下载遇到超长临时文件时重置恢复 |
| 权限撤销 | 收回执行权限后取消正在运行的原生任务；撤销设备信任后关闭连接并拒绝后续操作 |

测试使用独立数据目录、临时密钥、临时 SSH 配置和回环端口。没有安装 WorkLink 服务，没有修改已有 SSH 配置、用户 SSH 密钥、frp 或防火墙。

## 空闲资源实测

测量最终发布版，两端 daemon 已建立加密长连接，A 同时运行原生托盘，GUI 已关闭。等待初始化后采样 30.01 秒，覆盖连接心跳周期。原始记录见 [idle-measurement.json](idle-measurement.json)。

| 进程 | 私有内存 | 工作集（包含共享页） | 采样期 CPU 累计增量 |
|---|---:|---:|---:|
| A 后台 | 6.41 MiB | 41.29 MiB | 0.0000 秒 |
| B 后台 | 6.46 MiB | 41.31 MiB | 0.0000 秒 |
| A 托盘 | 2.15 MiB | 44.22 MiB | 0.0000 秒 |

工作集含共享系统 DLL 和文件映射页面，各进程工作集相加会重复计算共享页面。CPU 增量低于此次系统计时精度，不表示完全没有 CPU 开销。这是单次短时、无任务和传输时的测量，传输/执行任务和打开 GUI 时的占用会增加。

可在源码项目目录复现：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\measure-idle.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\ui-smoke.ps1 -Exe .\target\release\WorkLink.exe
$env:WORKLINK_TEST_EXE = (Resolve-Path .\target\release\WorkLink.exe).Path
cargo test --locked --test end_to_end -- --nocapture
Remove-Item Env:\WORKLINK_TEST_EXE
```

## 仍需在部署环境完成的验证

1. 在两台 PC 上以管理员身份安装服务，验证重启后、无人登录时的自动连接，以及明确授权的 SYSTEM 任务。当前开发会话没有管理员令牌，因此未安装服务或宣称该场景已实测。
2. 在 A 的 frpc 中添加 TCP 映射，公网服务器放行选定端口，B 配置真实入口；验证实际 NAT4 网络和 frp 重连。
3. 在实际 PC 上观察持续运行、休眠唤醒、系统重启及大文件传输。短时本机测试不能替代这些场景。

默认新增公网映射为 TCP 41443 → A 的 `127.0.0.1:41443`。公网端口可以更改，不必公开目标机 SSH 的 22 端口。加密保护内容，IP、端口和流量特征仍可见。
