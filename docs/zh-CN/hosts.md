<a id="hosts"></a>
# 宿主
[English](../hosts.md) · 简体中文

claudette 接受与 `claude` 相同的参数，因此任何启动 `claude` 的程序都可以改为启动它。宿主（host）可以通过以下两种方式找到它：

- 使用[安装](configuration.md#install)时创建的 `claude` 符号链接，前提是宿主通过 `PATH` 查找 `claude`，且链接所在目录位于宿主的 `PATH` 中。从 Dock 或 Finder 启动的桌面应用可能无法获得 shell 的 `PATH`。如果它的 Claude 提供程序找不到 `claude`，或报告的版本与远程机器上的版本不同，请改用二进制文件路径设置。
- 在宿主自己的二进制文件路径设置中填入 claudette 的绝对路径。无论宿主的 `PATH` 是什么，这种方式都能生效。

宿主必须运行在本地机器上，能使用你的 ssh agent，并且 claudette 的[设置](configuration.md#configure)中配置了 `host`，或宿主的环境中设置了 `CLAUDETTE_HOST`。宿主的手机应用连接的是本地机器上的进程，因此使用期间本地机器必须保持唤醒。

<a id="t3-code"></a>
## T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) 通过内置的 Claude 提供程序驱动 Claude。接入 claudette 后，T3 仍照常工作：审批、差异、检查点、工作树和线程历史都对应本地机器上的文件，而 Claude 则在 mini 上运行。

该提供程序默认运行 `claude`（**Binary path** 为 `claude`），因此有符号链接就够了。如需显式设置，请在 **Settings → Providers → Claude** 下的 **Binary path** 中填入 claudette 的绝对路径；如果服务器环境中没有 `CLAUDETTE_HOST`，还需将其加入提供程序的环境变量。在 `~/.t3/userdata/settings.json` 中，对应配置为：

```json
{ "providerInstances": { "claudeAgent": { "driver": "claudeAgent", "enabled": true, "config": { "binaryPath": "/Users/me/.local/bin/claudette" } } } }
```

先使用桌面应用，或在终端运行 `t3 serve`，再转为后台服务。

T3 还会运行 `claude -p` 来编写线程标题、提交消息和拉取请求描述。这些运行也会经过 claudette；当 T3 允许模型读取仓库时，模型读取的是本地检出目录，并受 T3 设置的 tool 列表和 `git` 规则约束。

<a id="paseo"></a>
## Paseo

[Paseo](https://paseo.sh) 也通过 Claude 提供程序驱动 Claude。有符号链接时，其运行 `claude` 的内置提供程序会调用 claudette。也可以单独配置一个提供程序，将其 `command` 设为 claudette，同时为它指定名称和远程主机。将以下内容合并到 `~/.paseo/config.json`，然后运行 `paseo reload`：

```json
{
  "agents": {
    "providers": {
      "claude-mini": {
        "extends": "claude",
        "label": "Claude on Mac mini",
        "command": ["/Users/me/.local/bin/claudette"],
        "env": { "CLAUDETTE_HOST": "mini" }
      }
    }
  }
}
```

随后，“Claude on Mac mini” 会出现在提供程序列表中，并提供 Claude 常用的模式（plan、default、acceptEdits、auto、bypassPermissions）。

<a id="magpie"></a>
## magpie

[magpie](https://usemagpie.ai) 通过驱动本地 `claude` 二进制文件，将 Claude 订阅提供给机器上的其他 agent 使用，例如 Codex、OpenCode 和 Gemini CLI。每一轮都会运行 Claude Code，唯一可用的 tool 来自 magpie 的 MCP server，agent 的 tool call 会返回给 agent 自身。magpie 使用在 `PATH` 中找到的 `claude`，因此有符号链接时，它驱动的就是 claudette，远程机器上登录的订阅便可供本地机器上的所有 agent 使用。`magpie accounts add claude` 会运行 `claude auth`，claudette 会将该命令交给远程机器执行。

预期行为如下：

- magpie 的 MCP server 通过本次运行的 `--mcp-config` 传入，claudette 会在本地机器上运行它，因此它能像未使用 claudette 时一样，访问位于 `127.0.0.1` 的 magpie 网关。
- 在默认的交互模式下，magpie 从本地机器上的 `~/.claude/projects` 读取会话记录（transcript），claudette 也将会话写入这里。
- 回复逐条完整到达，而不是流式输出。magpie 通过 `BUN_OPTIONS` 将脚本加载到 Claude Code 中来实现流式输出，但该变量和脚本都留在本地机器上，因此 magpie 会退回到整条消息输出，和面对任何未加载该脚本的 Claude Code 时一样。
- 只会使用远程机器上的登录状态。各自设置了 `CLAUDE_CONFIG_DIR` 的已保存账号不会沿用，因为本地机器上的路径在远程机器上没有意义，因此 magpie 的 Claude 账号故障切换机制实际上只有一个账号可用。

<a id="acp-clients"></a>
## ACP 客户端

Zed、JetBrains AI Assistant、[Toad](https://github.com/batrachianai/toad)、[acpc](https://github.com/AkaraChen/acpc)、acpx 及其他 [Agent Client Protocol](https://agentclientprotocol.com) 客户端，通过 [claude-agent-acp](https://github.com/agentclientprotocol/claude-agent-acp) 适配器运行 Claude；该适配器通过 Agent SDK 驱动 Claude Code。设置 `CLAUDE_CODE_EXECUTABLE` 后，适配器会运行该变量指定的二进制文件，因此将它指向 claudette，就能让 Claude 在远程机器上运行，而 tool 仍操作本地机器上的文件。在 Zed 中，向 `~/.config/zed/settings.json` 添加一个自定义 agent：

```json
{
  "agent_servers": {
    "Claude on mini": {
      "type": "custom",
      "command": "npx",
      "args": ["-y", "@agentclientprotocol/claude-agent-acp"],
      "env": { "CLAUDE_CODE_EXECUTABLE": "/Users/me/.local/bin/claudette", "CLAUDETTE_HOST": "mini" }
    }
  }
}
```

在 acpc 中，向 `~/.config/acpc/config.toml` 添加一个 agent：

```toml
[[agents]]
slug = "claude-mini"
name = "Claude on mini"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]
env = { CLAUDE_CODE_EXECUTABLE = "/Users/me/.local/bin/claudette", CLAUDETTE_HOST = "mini" }
```

也可以将 `command` 改为适配器发行版提供的独立可执行文件，这样就不需要 Node.js。其他客户端也在各自的 agent 设置中使用相同的命令和环境变量。

预期行为如下：

- 它以完整的内置 tool 集启动 Claude Code，claudette 会将这些 tool 替换为本地机器上的 tool，因此修改会写入编辑器的检出目录，编辑器的差异视图也会像 T3 Code 一样显示本地机器上的路径。
- 编辑器的 MCP server 会传入本次运行的 MCP 配置，并在本地机器上运行，与 SDK 宿主中的行为相同。
- 适配器从本地机器上的 `~/.claude/projects` 列出会话，claudette 也将会话写入这里，因此编辑器的线程历史与 `claude --resume` 显示的是同一组会话。

<a id="other-agent-sdk-hosts"></a>
## 其他 Agent SDK 宿主

任何基于 Claude Agent SDK 构建的程序，都可以通过设置可执行文件来使用 claudette：

```ts
query({ prompt, options: { pathToClaudeCodeExecutable: "/Users/me/.local/bin/claudette", cwd } });
```

<a id="other-control-planes"></a>
## 其他控制平面

以下控制平面以相同方式使用 claudette：

| 控制平面 | 使用 claudette 的方式 |
|---|---|
| [Vibe Kanban](https://github.com/BloopAI/vibe-kanban) | 它的 Claude Code 执行器使用相同的 stream-json 协议，由它自行实现，而非通过 Agent SDK。将执行器的 `base_command_override` 设为 claudette。 |
| [Claude Code UI](https://github.com/siteboon/claudecodeui) | 将 `CLAUDE_CLI_PATH` 设为 claudette。 |
