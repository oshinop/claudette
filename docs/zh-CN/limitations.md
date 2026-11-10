<a id="limitations"></a>
# 限制

[English](../limitations.md) · 简体中文

与直接在本地机器上运行 Claude Code 相比，哪些行为有所不同，哪些功能完全不可用。

<a id="where-it-runs"></a>
## 运行平台

- 本地机器可以使用 macOS 或 Linux。不支持 Windows。
- 远程机器可以使用 Linux 或 macOS。Linux 远程机器是受支持的配置；Mac 远程机器应该可用，但尚未通过完整会话验证。
- 远程机器需要 Claude Code 2.1.319 或更高版本。`claudette +doctor` 会对旧版本发出警告。

<a id="features-that-are-off"></a>
## 已禁用的功能

- LSP、Monitor、worktree tool 和 agent 团队不可用。自行创建 worktree 的宿主（host），例如 T3 Code 和 Paseo，不受影响。
- 不允许后台会话：`claude agents`、`--bg`、`/bg`，以及在空提示词输入框中按 `←`。Claude Code 的后台服务会在远程机器的沙箱外运行这些会话。要在关闭窗口后继续运行终端会话，请在本地机器上使用 tmux 或 screen。
- Claude 无法按名称重新接手已完成的 subagent，而是会启动一个新的 subagent。

<a id="in-claude-codes-own-interface"></a>
## Claude Code 自带界面中的差异

T3 Code 和 Paseo 等宿主没有这些问题；它们只适用于在终端中运行 `claude` 的情况。

- 界面在远程机器上绘制，因此每次按键和重绘都要经过网络（参见[使用说明](usage.md#prefer-a-host-to-claude-codes-interface)）。
- 在本地机器上运行的 tool 显示为 `mcp__cc__Read`、`mcp__cc__Bash` 等名称。
- `/rewind` 无法撤销本地机器上 tool 所做的编辑。请使用 git 撤销。
- 如果启动界面时附带提示词（`claude "fix it"`），第一轮只能通过 tool 搜索获取本地机器上的 tool，这些 tool 会被视为延迟加载 tool。改为在界面中输入提示词即可避免这个问题；在服务器上设置 `alwaysLoad` 或设置 `MCP_CONNECTION_NONBLOCKING=0` 都无效。
- `/mcp` 会列出一个名为 `claudette-inbox` 的服务器。它不提供 tool，只负责传递其他会话发来的消息。

<a id="your-configuration"></a>
## 你的配置

- 设置、快捷键绑定、权限规则、hook、skill、agent、命令、输出样式、`CLAUDE.md` 文件、MCP server 和已启用的插件都来自本地机器。`~/.claude` 的其余内容以及 `~/.claude.json` 中的偏好设置不会使用。
- 首次使用引导和“信任此文件夹”提示属于远程机器。你只需在那里回答一次，所有使用该机器的本地机器都会共享这个回答。
- 远程机器自身的插件和 MCP server 永远不会加载。
- 如果某个会话更改了设置或快捷键绑定，更改会在下一个会话中生效，而不是当前正在运行的会话。

<a id="plugins"></a>
## 插件

- 插件的 MCP 捆绑包（`.mcpb`、`.dxt` 或 URL）及其监视器在远程机器上运行。它的 LSP 服务器与 LSP tool 一样保持禁用。
- 通过 `claude plugin` 设置的敏感插件选项不会被记住。
- 如果远程机器的托管设置禁止 `--plugin-dir`，则不会加载任何插件。

<a id="mcp-servers"></a>
## MCP server

- 在本地机器上运行的服务器无法向客户端提出任何请求：采样、信息征询和根目录请求都会被拒绝，也无法完成 OAuth 登录。
- 在 T3 Code 等 SDK 宿主中，这些服务器的提示词不会作为斜杠命令提供。终端会话和 `-p` 会话则会提供。
- 使用 `--remote` 添加的服务器在远程机器的沙箱内运行。它们只能看到项目和 Claude Code 自身的状态，无法看到远程主目录中的其他内容。

<a id="sessions-talking-to-each-other"></a>
## 会话间通信

- ListAgents 和 SendMessage 只能访问同一台本地机器上的 claudette 会话。
- 直接在远程机器上启动的会话仍然可以向你的会话发送消息。如果目标会话跳过权限提示，这类消息会等待批准；宿主或 `-p` 会话无法显示批准提示，因此消息会在五分钟后过期。

<a id="connection-and-speed"></a>
## 连接与速度

- 每次本地 tool call 都需要与远程机器往返一次，每次会话启动也比本地会话多一到两次往返（[延迟](latency.md)）。两台机器之间的直接连接（例如 Tailscale）可以缩短这一时间。
- 当本地机器休眠或离线时，会话会在远程机器上等待，并在本地机器恢复后继续。
- 会话属于启动它的 claudette 进程。如果该进程结束（崩溃、终端关闭或重启），其他进程无法接管该会话，远程 claude 会在 `detach_timeout` 后停止，默认值为 12 小时。
- 在本地机器上创建、重命名或删除文件后，最多需要 10 秒才能反映在 Claude Code 自行读取的内容中，包括设置、skill 和 `@` 提及。tool 始终能看到当前文件。
- 如果远程机器在会话中途无法再访问项目，claudette 会发出提示，但 claude 会在没有项目的情况下继续运行，而本地机器上的会话记录（transcript）会停在这一时刻。

<a id="smaller-differences"></a>
## 其他细微差异

- 使用 Mac 远程机器时，如果项目中已有一个会话，第二个会话会在常规文件夹旁边的文件夹中工作。它的会话记录仍会写入本地机器上的同一位置。
- 后台 Bash 命令会将输出写入文件，供模型读取。像 T3 Code 这样实时显示 Claude Code 自带后台任务输出的宿主，只能看到最终结果。
- 会话结束时，会删除远程机器上 Claude Code 的临时文件。
