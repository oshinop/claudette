<a id="usage"></a>
# 使用说明

[English](../usage.md) · 简体中文

安装 claudette 并将其链接为 `claude`（[安装说明](configuration.md)）后，用法与 Claude Code 相同。本页介绍 claudette 特有的用法和注意事项。

<a id="running-claude"></a>
## 运行 claude

```sh
claude                                  # Claude Code 的界面
claude -p "summarize the last commit"   # 单次运行，适用于脚本
claude --continue                       # 项目的上一次会话
claude --resume <session-id>            # 项目的任意会话
```

会话保存在本地机器的 `~/.claude/projects` 中，与本地 claude 的存储方式相同。因此，`--continue` 和 `--resume` 能找到项目的所有会话：从终端启动的会话、T3 Code 或 Paseo 启动的会话，以及切换前由本地 claude 写入的会话。

Claude Code 自带的命令，例如 `claude auth` 或 `claude doctor`，会原样在远程机器上运行。claudette 自带的命令以 `+` 开头：`claude +doctor` 检查 ssh 和远程机器，`claude +update` 安装最新发行版，`claude +help` 列出命令和设置。在终端中，这些命令还会提示是否有新发行版。如果 `+` 妨碍了你的使用，可以在[设置](configuration.md#configure)中选择其他 `prefix`。

要从 T3 Code、Paseo、magpie 或 Agent SDK 程序运行 claudette，请参阅[宿主](hosts.md)。

<a id="mcp-servers"></a>
## MCP server

使用 `claude mcp` 在本地机器上配置 MCP server，写入的文件与本地 claude 相同：用户作用域和本地作用域使用 `~/.claude.json`，项目作用域使用项目中的 `.mcp.json`。这些服务器在本地机器上运行，因此本地机器 `localhost` 上的服务器也能使用。

```sh
claude mcp add --scope user fs -- npx @modelcontextprotocol/server-filesystem .
claude mcp add --scope project lint -- node tools/lint-mcp.js   # 写入 .mcp.json
claude mcp list                                                 # 会话可用的所有服务器
claude mcp remove fs
```

添加 `--remote` 可改为在远程机器上运行服务器，例如需要靠近远程机器的网络或文件的服务器：

```sh
claude mcp add --remote --transport http docs https://example.com/mcp -H 'Authorization: Bearer …'
```

`claude mcp list` 和 `claude mcp get` 会像 Claude Code 一样检查每个服务器，远程服务器从远程机器上检查。`claude mcp add-json`、`claude mcp add-from-claude-desktop`（本地机器上的 Claude Desktop）和 `claude mcp reset-project-choices` 也可使用。服务器设置中的 `${VAR}` 会从本地机器的环境中填入，远程服务器也不例外。服务器的 `headersHelper` 在本地机器上运行。

项目 `.mcp.json` 中的服务器会在你批准后启动。claudette 不会显示批准提示：`claude mcp add --scope project` 会批准它添加的服务器，其他服务器需要在 `.claude/settings.local.json` 的 `"enabledMcpjsonServers"` 中批准。

需要 OAuth 登录的服务器会显示 `! Needs authentication`。使用 `claude mcp login <名称>` 登录：浏览器在本地机器上打开（或者使用 `--no-browser`，由你粘贴重定向 URL），令牌保存在本地机器的 `~/.config/claudette/mcp-oauth` 中。`claude mcp add` 的 `--client-id`、`--client-secret` 和 `--callback-port` 与 Claude Code 中的用法相同，`claude mcp logout` 用于退出登录。

<a id="plugins"></a>
## 插件

`claude plugin install`、`claude plugin marketplace add` 和其他插件命令操作的是本地机器上的插件，与本地 claude 相同。会话会加载已安装且已启用的插件，插件的 hook 和 MCP server 在本地机器上运行。

<a id="links-and-the-clipboard"></a>
## 链接和剪贴板

远程机器上的程序通过 `$BROWSER` 打开链接时（例如 Claude Code 通过 `/mcp` 登录用 `--remote` 添加的服务器），链接会在本地机器上打开，所在的独立浏览器窗口经由远程机器联网：该窗口中的 `localhost` 是远程机器的，域名在远程机器上解析，网站看到的是远程机器的地址。claudette 使用 105 或更高版本的 Google Chrome、Chromium、Brave、Microsoft Edge，或 107 或更高版本的 Firefox，并在 `~/.config/claudette/browser` 中为每台远程机器准备单独的配置文件，因此该窗口不带你日常浏览器的 Cookie 或扩展。它报告的时区是 UTC，语言是 `en-US`。只会打开 `http` 和 `https` 链接。只要还有在该远程机器上打开过链接的会话在运行，窗口就可以访问远程机器。

窗口只有经由远程机器来请求链接之后，才会拿到该链接。如果浏览器版本太旧、没有经由远程机器联网，或者仍以旧版 claudette 的设置运行，就什么也不打开，打开链接的程序会收到说明原因的错误。

远程机器上的程序看不到 ssh 会话，那里的 `pbcopy` 和 `pbpaste` 使用本地机器的剪贴板（在 Linux 上通过 wl-clipboard、xclip 或 xsel）。

<a id="sessions-talking-to-each-other"></a>
## 会话间通信

同一台本地机器上的会话，无论在终端还是宿主（host）中运行，都能通过 ListAgents 找到彼此，并通过 SendMessage 相互发送消息。其他本地机器上的会话即便使用同一台远程机器，也看不到这些会话。

<a id="things-to-watch-out-for"></a>
## 注意事项

<a id="prefer-a-host-to-claude-codes-interface"></a>
### 优先使用宿主，而非 Claude Code 自带界面

Claude Code 的界面在远程机器上绘制，并通过 ssh 显示。每次按键和重绘都要经过网络，每次启动比本地运行多花约一秒（[延迟](latency.md)），而且有些功能不可用：tool 显示为 `mcp__cc__*`，`/rewind` 无法撤销它们的编辑，后台会话也被禁用。T3 Code 和 Paseo 在本地机器上绘制界面，没有这些问题。Claude Code 自带界面适合用来快速检查。

首次打开项目时，Claude Code 会在远程机器上询问是否信任那里的项目文件夹。该文件夹就是从远程机器看到的你的项目。

<a id="keep-claudette-running"></a>
### 保持 claudette 运行

会话属于启动它的 claudette 进程。如果本地机器休眠或断网，会话会等待，并在恢复后继续。如果 claudette 进程本身结束，例如因为你关闭了终端，会话就无法再接手，远程 claude 会在 `detach_timeout` 后停止（默认 12 小时）。后台会话（`--bg`、`/bg`、`claude agents`）不可用；长时间运行的终端会话请改用本地机器上的 tmux 或 screen。

<a id="your-environment-goes-with-you"></a>
### 环境变量会随你一同传递

在本地机器上设置的 Claude Code 自身的变量会传给远程 claude，包括凭据和提供商设置（[配置](configuration.md#configure)）。如果 shell 导出了 `ANTHROPIC_API_KEY` 或 `CLAUDE_CODE_OAUTH_TOKEN`，远程会话就会使用该凭据，而非远程机器的登录，并采用该凭据对应的计费方式。设置 `forward_claude_env = false`，即可不受 shell 环境影响，始终使用远程机器的登录。这些值通过私密方式传输，绝不会出现在远程机器的进程列表中。

<a id="the-log-holds-everything"></a>
### 日志包含所有内容

`log` 设置会将 claudette 与 claude 之间的每条消息写入本地机器上的文件，包括你的提示词、文件内容，以及转发变量的值。调试时再启用，并在调试后删除该文件。

<a id="new-files-show-up-late-in-claude-codes-own-reads"></a>
### Claude Code 自行读取时，新文件会延迟出现

在本地机器上创建、重命名或删除文件后，最多需要 10 秒才能反映在 Claude Code 自行读取的内容中，例如设置、skill 和 `@` 提及。tool 始终能看到当前文件。

与本地 Claude Code 的完整差异列表见[限制](limitations.md)。
