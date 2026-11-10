<a id="testing"></a>
# 测试

[English](../testing.md) · 简体中文

```sh
scripts/test.sh
```

所有测试都在容器（`tests/docker/compose.yaml`）中运行，使用无法访问互联网的内部网络。端到端测试套件是 `tests/` 下独立的 Bun 包，本地机器镜像将其安装到 `/app`：

- `mini` 运行 sshd、sshfs、bubblewrap 和真正的 Claude Code CLI，后者与 `mockapi` 通信。
- `mockapi` 是可通过脚本编排的 Anthropic Messages API（`tests/mock-api/server.ts`）。测试注册一段对话，由 tool call 和随后的文本组成，文本可以引用上一次 tool call 的结果；测试会检查 claude 发出的每个请求，包括 tool 列表和系统提示词。
- `laptop` 包含从本 crate 构建的 claudette，以及测试所需的 Bun、Agent SDK、ssh 和 git。Agent SDK 自带的 `claude` 被排除在外，只要存在任何 `claude` 二进制文件，镜像构建就会失败，因此每次 tool call 都必须在这里运行。这里也没有 ripgrep：Grep 是内置的。
- `t3` 和 `paseo` 是装有真正的 T3 Code 服务器（0.1.7）或 Paseo 守护进程（0.15.3）的本地机器环境，通过各自的命令行客户端驱动。

| 测试套件 | 覆盖内容 |
|---|---|
| `cargo test` | 参数改写和调用方的 tool 限制；通过真正的 `sh` 执行的 ssh 命令引号处理和清理；环境变量允许列表；权限规则；中继（relay）的改写；套接字服务器；每个本地 tool；SFTP 服务器的访问范围限制和写入规则，以及会话文件夹的路径改写；挂载在其服务器响应时的预热（warm start）；位于真正的会话服务器前的近端（near end）：在变更使已知信息失效之前，利用这些信息响应查找；通过后写（write-behind）方式写入的会话记录（transcript），无论是新建还是恢复的，最终在本地机器上都与本地机器写入的内容完全一致；超过单次读取大小且在读取期间增长的会话记录仍会被完整获取，而本地机器拒绝打开的会话记录不会被保留；设置视图及其往返过程；exec 形式的 hook 不经 shell 运行，CwdChanged 和 FileChanged 在本机触发，`CLAUDE_ENV_FILE` 传给本地 Bash；插件：选择哪份安装、为其 MCP server 命名并填入选项、选项的来源、清单视图、插件存储的路径转换；MCP 配置和批准；文件夹名称和路径映射；会话无需注册表即可发现彼此，按名称和 id 寻址，空闲通知，删除已结束运行的文件夹；按数字比较发行版版本，为每个文件查找 `SHA256SUMS` 条目，以及在 `+update` 替换构建产物后，会话使用的构建产物仍能提供计算其哈希时的原始字节 |
| `tests/e2e/session` | 使用 T3 风格选项的 Agent SDK `query()`，通过 claudette 和 ssh 连接真正的 claude：tool 路由、内置允许列表、权限模式和前缀规则、拒绝、计划模式、http、stdio 和进程内 MCP server、通过 `setMcpServers` 设置的远程 MCP server、经会话文件夹写入本地机器的会话记录、恢复、留在本地机器上的会话分支、中断、`--add-dir`、项目 skill、保存的“始终允许”、在本地机器上运行且 `transcript_path` 可读的项目 hook、用户的 skill、agent、设置和 hook 被传递而登录信息和凭据留在本机、本地机器上已安装的插件默认启用且其 skill（选项已填入）、hook 和 MCP server 在本地机器上运行、保存在本地机器上的用户级“始终允许”、与不带约束的允许规则相对的 `Read(…)`/`Edit(…)` 拒绝和询问规则、本地机器命令中的设置 `env`、`additionalDirectories`、排除远程用户的配置、透传、doctor |
| `tests/e2e/direct` | `claudette -p`：tool 在本地机器上运行、通过 hook 实现 acceptEdits、未经批准不做编辑、T3 Code 读取仓库以生成文本、`--tools ""`、`--mcp-config` 服务器及其作为斜杠命令提供的提示词、`.mcp.json` 批准、远程服务器的密钥不出现在命令行中、排除远程机器自身的服务器、`claude mcp add/list/remove` 写入 `~/.claude.json` 和 `.mcp.json`（不修改无法解析的 `~/.claude.json`）、`claude plugin marketplace add` 和 `install` 使用本地机器路径写入本地机器的插件存储和设置、下一次会话加载插件、沙箱、恢复由 SDK 宿主（host）启动的会话、`--continue`、将 mini 早先的会话移至本地机器、清理 |
| `tests/e2e/agents` | 本地机器上会话之间的 ListAgents 和 SendMessage：本地版本替换 Claude Code 自带版本、列表显示项目和状态、消息在空闲的宿主会话中启动一轮并标明来自其他会话、`notify_when_idle`、不列出其他运行时文件夹中的会话和直接在 mini 上运行的会话；拒绝后台会话命令和 `--bg`，强制禁用 agent 团队和 mini 自身的 agent 视图 |
| `tests/e2e/interactive` | 伪终端中的 Claude Code 交互界面：信任提示、在本地机器上进行编辑和执行命令、退出、清理，以及 Claude Code 自带的 `/resume` 列表同时显示该会话和切换前由本地 claude 写入的会话；终端会话期间保存到用户设置中的 `/theme` 和权限规则，以内置名称写入本地机器的 `~/.claude/settings.json`，与本地机器已有的键并存，不修改远程机器自身的设置；`←` 和 `/bg` 不会在 mini 上启动后台服务 |
| `tests/e2e/helper` | 辅助程序（helper）只复制到 mini 一次并重复使用，一次运行结束后不留痕迹；首次运行在携带 3 MB 恢复会话记录的同时复制辅助程序；tool call 中途连接被切断的宿主会话仍能完成调用及回复；一个 `-p` 运行被停止（SIGSTOP）且连接被切断后恢复；本地机器始终未重新上线的会话在断开超时后于 mini 上结束并清理；消息送达跳过权限提示的会话 |
| `tests/e2e/fixes` | 提供 Cron tool；宿主会话级的“始终允许”涵盖某 tool 的其他路径，同时询问规则仍然有效 |
| `tests/t3` | T3 Code 运行一个线程，其 Bash、Write 和 Read 均在本地机器上运行，并在服务器重启后恢复该线程 |
| `tests/paseo` | Paseo 运行一个 agent，其命令和文件读取均在本地机器上运行，并在守护进程重启后恢复该 agent |

尚未覆盖：

- 使用 macOS 远程机器的完整会话。其 `sandbox-exec` 配置已单独检查过（钥匙串登录、Claude Code 的文件、封闭的主目录），但尚未在 macOS 上运行 sshfs（macFUSE 或 FUSE-T）。
- Claude Code 未记录在文档中的 `CLAUDE_SECURESTORAGE_CONFIG_DIR`，使每次运行专用的 Claude Code 文件夹能够共享登录。已在 Linux 上使用 2.1.319 检查；在 macOS 上，其作用仅根据 Claude Code 的代码判断，尚未测试。

<a id="playground"></a>
## 试验环境

`scripts/playground.sh` 提供一台远程机器，让你手动试用 claudette：即测试镜像中的 `remote` 容器，位于 `127.0.0.1:2222`，可以访问互联网。`up` 会为本机和容器构建 claudette、启动容器，并使用 `target/playground` 中的独立配置设置 ssh，因此既不读取也不修改 `~/.ssh`。构建 Linux 二进制文件需要 zig 和 cargo-zigbuild（参见 `scripts/build.sh`）。

```sh
scripts/playground.sh up
scripts/playground.sh login                    # 只需一次：Claude Code 登录，保存在卷中
eval "$(scripts/playground.sh env)"            # 在当前 shell 中将 claudette 指向该环境
eval "$(scripts/playground.sh env mock)"       # 或者：同样的环境，但使用 mock API，无需登录
cd ~/some/project && claudette -p "hi"
```

`env` 输出 `CLAUDETTE_HOST`、`CLAUDETTE_SSH`，以及它构建的 `claudette` 的别名；`env mock` 还会添加让 Claude Code 使用 mock API 的变量。使用 mock 时，`scenario <id> '<steps>'` 可为包含 `[[scenario:<id>]]` 的提示词编排回复：每一步是一个 tool call（`{"tool": "Bash", "input": {…}}`）或一段文本，文本可以引用 `{{last_tool_result}}`。`ssh` 打开容器内的 shell，`down` 停止容器（`down -v` 还会删除登录信息和 ssh 密钥）。
