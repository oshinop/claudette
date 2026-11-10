<a id="startup-latency"></a>
# 启动延迟
[English](../latency.md) · 简体中文

会话启动时，需要进行 ssh 交互、建立挂载（项目、项目的会话文件夹、用户内容和插件，以及 Claude Code 文件夹），然后启动 claude 本身。在慢速连接上，claude 等待的每个请求都需要与本地机器往返一次，而且这些请求串行执行。因此，设计上尽可能让 claude 无须等待本地机器：

- **每个挂载的近端（near end）**（参见[设计](design.md#the-near-end-of-a-mount)）。claude 最先从挂载中读取的内容会随本次运行一起发送，由本地机器自身的服务器提供，因此建立挂载和启动 claude 都不需要往返。查找结果和目录列表缓存 10 s；对于不存在的名称，直接根据完整目录列表返回结果。会话记录（transcript）采用后写（write-behind）：claude 打开、写入和读取自身会话记录时，都无须等待本地机器。
- **运行所需的一切都放在最初的帧中。** 会话的设置、追加的提示词、MCP 配置、本地 tool 列表和挂载的预热（warm start）数据，都在启动之前发送；首次连接会在自身的问候消息之后立即发送这些内容，而不是等辅助程序（helper）的问候消息之后再发。
- **在远程机器上回应握手。** 包括本地 tool 的 MCP 握手（`initialize`、`tools/list`、`server/discover` 探测）、claudette 的状态 hook（claude 不再等待这些 hook），以及 Bash 调用的权限判定（仅依据会话的 Bash 规则）。
- **共享 ssh 连接。** 会话在同一条共享连接（`ControlMaster`，保留 10 分钟）上打开各自的通道，而不是新建连接；会话承载的所有内容都经由辅助程序通过该通道传输。
- **缓存辅助程序。** 首次会话会将 claudette 的二进制文件复制到远程机器（约 6.5 MB，每个版本只需一次）；后续会话会在 `~/.claudette/bin` 中找到它。

仍需往返的操作包括：打开本次运行的通道（ssh 先启动远程命令，再经过一次往返才发送其 stdin）、每次本地 tool call、编辑操作的权限判定（路径规则需要解析本地机器上的文件）、读取预热数据之外的内容，以及结束会话（需要等待本地机器确认会话记录的最后几次写入）。

<a id="measured"></a>
## 实测

在一台机器上测量：claudette 运行于 macOS，通过 127.0.0.1 上的 ssh 连接 playground 容器（`tests/docker/playground.yaml`）；Claude Code 连接模拟 API，并设置 `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`；使用干净的主目录，取五至七次运行的中位数。“原生 claude”指通过同一条共享 ssh 连接，在容器中运行相同的 Claude Code，但不使用 claudette。

| | 原生 claude | claudette |
|---|---|---|
| `claude -p hi`，直至退出 | 0.18 s | 0.53 s |
| `claude -p`，包含 10 次 Bash 调用，直至退出 | 0.29 s | 0.76 s |
| stream-json 会话：从启动到收到 `initialize` 响应 | 0.12 s | 0.22 s |
| stream-json 会话：不调用 tool 的一轮 | 0.01 至 0.05 s | 0.01 至 0.06 s |
| stream-json 会话：包含 10 次 Bash 调用的一轮 | 0.12 s | 0.22 s |
| 界面：首屏 | 0.19 s | 0.33 s |
| 界面：从按键到重绘 | 16 ms | 7 ms |

Claude Code 自身的后台流量比 claudette 带来的开销更大。在可访问互联网的机器上，启用这些流量时，原生 `claude -p hi` 约需 1.1 s，禁用后则为 0.18 s；其中大部分来自遥测（仅设置 `DISABLE_TELEMETRY=1` 就能降至 0.5 s）。claudette 会将 Claude Code 的变量传给远程 claude，因此，在本地机器上设置的 `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` 会应用于每个会话。

<a id="on-a-slow-link-500-ms-round-trip-10-mbits"></a>
### 慢速连接：往返 500 ms，10 Mbit/s

使用相同配置，但将发往远程机器的每个字节在两个方向上各延迟 250 ms，并将连接限速为 10 Mbit/s（通过 ssh `ProxyCommand` 中继（relay）实现），取三至五次运行的中位数。在共享连接上启动命令并看到它退出，需要两次往返，共 1.04 s。“优化前”指挂载底层使用普通中继、每个文件、握手和 hook 都通过连接获取或响应的 claudette。

| | 原生 claude | 优化前 | claudette |
|---|---|---|---|
| `claude -p hi`，直至退出 | 1.67 s | 24.2 s | 3.05 s |
| `claude -p hi`，带一个用户 skill，直至退出 | | | 3.13 s |
| `claude -p`，包含 3 次 Bash 调用，直至退出 | 1.71 s | 28.5 s | 4.74 s |
| stream-json 会话：从启动到收到 `initialize` 响应 | 1.58 s | 7.4 s | 2.15 s |
| stream-json 会话：不调用 tool 的首轮 | 0.58 s | 9.2 s | 0.69 s |
| stream-json 会话：不调用 tool 的后续一轮 | 0.54 s | 1.6 s | 0.55 s |
| stream-json 会话：包含 3 次 Bash 调用的一轮 | 0.59 s | 4.7 s | 2.19 s |
| stream-json 会话：关闭 | 0.54 s | 6.4 s | 1.30 s |
| 界面：首屏 | 1.19 s | 6.9 s | 1.83 s |
| 界面：从按键到重绘 | 534 ms | 513 ms | 527 ms |

新版本首次运行时还需要复制辅助程序，在此速度下约需 10 s。

`-p hi` 的 3.05 s 分解如下：

- ssh 打开本次运行的通道并启动辅助程序需要 0.8 s，与执行任何命令相同。
- 本次运行的数据帧到达辅助程序需要再等 0.5 s：ssh 只有在命令启动后才会发送该命令的 stdin。
- 远程机器上的准备工作需要 20 ms：挂载通过预热数据建立。
- claude 启动并完成这一轮需要 0.7 s，与不使用 claudette 时相同；回答约在 2.4 s 时到达本地机器。
- 等待本地机器确认会话记录的最后几次写入需要 0.55 s，之后还需传回退出码并发送告别消息。

每次本地 tool call 需要一次往返，也就是调用本身：Bash 调用的权限在远程机器上判定，其会话记录行采用后写。编辑操作还需要额外一次往返，用于在本地机器上判定权限。

<a id="not-done"></a>
## 尚未实施

| 方案 | 可节省 | 代价 |
|---|---|---|
| 本地机器在收到退出码后排空其服务器中的待处理数据，而不是由远程端等待会话记录的最后几次写入得到确认 | 每个会话结束时约 0.5 s | 守护进程（holder）必须先发送每个通道的全部数据，再发送退出码；本地机器则需等待自身服务器处理完这些数据。目前由确认机制保证这一顺序。 |
| 像本地 tool 一样，在远程机器上回应桥接 MCP server 的握手 | 本地机器配置中的每个服务器可节省 3 次往返 | 每个服务器都必须在本次运行开始前启动并列出其 tool。 |
| 在本地机器上运行长期驻留的 agent，在会话之间为每个项目保持连接 | 打开通道的一次往返，以及 ssh 传递 stdin 之前的一次往返 | 需要安装并管理一个守护进程。 |
