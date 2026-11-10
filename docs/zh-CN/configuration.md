<a id="installation-and-configuration"></a>
# 安装与配置

[English](../configuration.md) · 简体中文

<a id="requirements"></a>
## 环境要求

远程机器需要：

- Claude Code（已用 2.1.319 测试），已登录，以及 POSIX shell；
- sshd，无须启用任何转发，因为所有数据都通过会话自身的通道传输；
- 支持 `-U` 的 `nc`，macOS 已自带；
- 用于挂载项目的 `sshfs`：Linux 上的 `sshfs` 软件包（FUSE 3），或 macOS 上来自 [macFUSE](https://macfuse.github.io) 或 [FUSE-T](https://www.fuse-t.org) 的 sshfs；
- 沙箱，因为 claude 始终在沙箱中运行：Linux 上使用 bubblewrap（`bwrap`），macOS 上使用内置的 `sandbox-exec`。

除此之外，无须在远程机器上安装任何东西。claudette 首次使用时会将自身复制到 `~/.claudette/bin`（参见[设计](design.md#the-helper)）。

本地机器需要能够无提示地连接远程机器的 ssh。密钥、用户和端口写在 `~/.ssh/config` 中，有口令的密钥应交给 agent 管理。在终端中运行一次 `ssh mini` 以接受主机密钥，然后用 `ssh -o BatchMode=yes mini claude --version` 检查。

只有从终端启动的一次运行中的首次连接，ssh 才可能显示交互提示。不带终端的运行（宿主（host）、脚本、守护进程）以及每次重连都会加上 `BatchMode=yes`，因此会立即失败，而不是等待无人能看到的提示。这也意味着，密码登录只适用于首次终端连接，其他情况均不适用。

`claudette +doctor` 会检查上述所有要求。

<a id="install"></a>
## 安装

```sh
curl -fsSL https://raw.githubusercontent.com/oshinop/claudette/main/scripts/install.sh | CLAUDETTE_HOST=mini sh
```

脚本会从最新的 GitHub 发行版下载所有平台的构建产物，用该发行版的 `SHA256SUMS` 校验，将它们放入 `~/.local/share/claudette`，并将 `~/.local/bin/claudette` 链接到本机的构建产物。claudette 会从自身所在目录复制远程机器所需的构建产物，因此 本地 Mac 连接 Linux 机器时，会使用该目录中的 `claudette-linux-arm64` 或 `-x64`。`CLAUDETTE_VERSION` 用于选择发行版（`v0.1.0`），`CLAUDETTE_BIN_DIR` 和 `CLAUDETTE_LIB_DIR` 用于更改目录。

通过变量提供的任何[设置](#configure)，例如 `CLAUDETTE_HOST` 或 `CLAUDETTE_DETACH_TIMEOUT`，都会写入新建的 `~/.config/claudette/config.toml`；已有的配置文件保持不变。变量应放在管道之后的 `sh` 上：如果放在 `curl` 上，脚本就收不到它们。

<a id="update"></a>
### 更新

```sh
claudette +update            # 最新发行版
claudette +update 0.1.0      # 指定发行版，也可以是旧版本
```

`+update` 会下载该发行版所有平台的构建产物，用其 `SHA256SUMS` 校验，确认本机构建产物可以运行，然后才通过重命名替换正在运行的 claudette 旁边的构建产物。失败时，已安装的构建产物保持原样。它适用于脚本安装的版本（构建产物命名为 `claudette-<platform>`）；与安装脚本一样，也可用 `CLAUDETTE_DOWNLOAD_URL` 指定镜像地址。重新运行安装脚本的效果相同。

已在运行的会话会继续使用启动时的构建产物，直到结束。远程机器会在下一次会话时获得新的构建产物（参见[设计](design.md#the-helper)）；远程机器上无须做任何操作。

在终端中运行 claudette 自身的命令（`+help`、`+version`、`+doctor`）时，如果有更新的发行版，会在末尾显示一行提示。每个命令都会在运行期间查询 GitHub，最多等待 2 秒，不在磁盘上保存任何内容。claudette 的其他运行方式，包括 `claude` 会话和脚本，都不会检查发行版。

如果要自行构建，需要通过 rustup 安装的 Rust、zig 和 cargo-zigbuild（参见 [README](../../README_zh-CN.md#development)）：

```sh
scripts/build.sh     # dist/claudette-{darwin-arm64,darwin-x64,linux-arm64,linux-x64}
```

Linux 二进制文件是静态链接的（musl），由 zig 链接。macOS 二进制文件使用 Apple 工具链，因此需要在 Mac 上构建。与安装脚本一样，将所有构建产物放在同一个目录中。

要将它用作 `claude`，请在 `PATH` 中排在其他所有 `claude` 之前的目录中，以该名称创建链接。claudette 会沿链接找到其他构建产物，绝不会运行本地的 `claude`。本地机器不需要安装 Claude Code。如果保留了原生安装，请将链接放在更靠前的目录中：原生安装的更新程序会重写 `~/.local/bin/claude`。

```sh
ln -s ~/.local/bin/claudette ~/bin/claude
```

<a id="configure"></a>
## 配置

设置保存在 `~/.config/claudette/config.toml` 中。只有 `host` 是必填项：

```toml
host = "mini"                                  # the remote machine's ssh name
# ssh = ["ssh", "-o", "ProxyJump=bastion"]     # instead of the default ssh, which shares one connection
# detach_timeout = "30m"
# forward_env = ["AWS_REGION", "AWS_PROFILE"]
# forward_claude_env = false
# log = "/tmp/claudette.log"
# prefix = "::"
```

然后运行 `claudette +doctor`。每项设置也可以通过 `CLAUDETTE_` 变量指定，键名大写，例如 `CLAUDETTE_HOST` 或 `CLAUDETTE_DETACH_TIMEOUT`；变量优先于配置文件，适用于单个 shell 或某个宿主的提供商设置。变量接受相同的值，以字符串形式表示：`ssh` 的各项用空格分隔，`forward_env` 的各项用逗号分隔，`forward_claude_env` 则用 `1` 或 `0`。claudette 会拒绝不认识的键，避免拼写错误被忽略。

| 设置 | 含义 |
|---|---|
| `host` | 远程机器，格式与 ssh 接受的相同：`~/.ssh/config` 中的别名、`user@host`，或 `ssh://user@host:port` URL。URL 不携带密钥或其他选项；这些应放在 `~/.ssh/config` 或 `ssh` 设置中。 |
| `ssh` | ssh 命令及选项，可以是列表或字符串。默认情况下，会话共享一个启用了保活的连接（`ControlMaster`，保留 10 分钟，套接字位于私有目录 `/tmp/claudette-<uid>`），而断线后重连的会话会建立自己的连接。这些选项排在 claudette 的选项之前，且 ssh 采用遇到的第一个值，因此这里的 `-o BatchMode=no` 会覆盖 claudette 的设置；`~/.ssh/config` 中的 `BatchMode` 行则不会。 |
| `detach_timeout` | 远程机器等待本地机器重连的时长，超过后会停止远程 claude：可以是秒数，或 `"90s"`、`"30m"`、`"12h"`（默认 `"12h"`）。 |
| `remote_claude` | 远程机器上的 claude 命令（默认 `claude`；其 PATH 包含 `~/.local/bin`）。 |
| `remote_nc` | 远程机器上的 Unix 套接字中继（relay），以 `<cmd> -U <path>` 形式运行（默认 `nc`）。 |
| `forward_claude_env` | 是否将本机设置的 Claude Code 自身的变量传给远程 claude（默认 `true`）。包括 `ANTHROPIC_*`、`CLAUDE_*`、`DISABLE_*`、`ENABLE_*`、`MCP_*` 和 `VERTEX_REGION_*` 变量，Claude Code 的其他变量（如 `MAX_THINKING_TOKENS`、`API_TIMEOUT_MS` 和 `DO_NOT_TRACK`），以及它读取的凭据：`ANTHROPIC_API_KEY`、`ANTHROPIC_AUTH_TOKEN`、`CLAUDE_CODE_OAUTH_TOKEN`（连同刷新令牌和作用域）、Foundry、AWS 和 Bedrock 的密钥，以及 `MCP_CLIENT_SECRET`。提供商和端点设置（`ANTHROPIC_BASE_URL`、`CLAUDE_CODE_USE_*`，以及 Bedrock、Vertex 和 Foundry 设置）按原样传递，因此远程 claude 会像本地 claude 一样，使用你在本机设置的提供商、端点和凭据；远程机器必须能够访问该端点。不会转发的有：指向本机文件或目录的变量（`CLAUDE_CONFIG_DIR`、`CLAUDE_CODE_TMPDIR`、mTLS 密钥和证书等）、本机 claude 为其子进程设置的变量（`CLAUDE_CODE_SESSION_ID`、消息传递令牌等），以及这些变量族中的其他密钥和令牌，例如某个服务器的 `MCP_*_TOKEN`。设置文件中的 `env` 也按相同方式传递，但其中的凭据除外；凭据会与本机其余登录信息（`apiKeyHelper` 等）一起保留在本机。 |
| `forward_env` | 额外传给远程 claude 的变量，不论变量是什么，也不论是否开启 `forward_claude_env`：例如 Bedrock 使用的 `["AWS_REGION", "AWS_PROFILE"]`，或 `["GITHUB_TOKEN"]`。转发的值绝不会出现在远程命令行上，避免被远程机器的其他用户读取（参见[设计](design.md#the-helper)）。 |
| `log` | 用于追加记录每个中继帧，以及远程 claude 通过挂载发出的每个请求的文件。它包含提示词、文件内容和转发变量的值。 |
| `prefix` | claudette 自身命令的标记（默认 `+`：`claudette +doctor`、`+update`、`+version`、`+help`）。只要第一个参数以它开头，就会交给 claudette 处理，因此应选择不会出现在提示词开头的内容。不允许使用以 `-` 开头的前缀。 |

<a id="plugins"></a>
### 插件

插件属于本机：`claude plugin install`、`claude plugin marketplace add` 等命令会像本地 claude 一样，在本机的 `~/.claude/plugins` 和设置上运行 Claude Code 自身的命令，会话则加载已安装并启用的插件。插件的 hook 和 MCP server 在本地机器上运行，使用你为它们指定的选项。
