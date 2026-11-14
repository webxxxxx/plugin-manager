# 模式 1：每用户 daemon（User Mode）

非 root 用户不安装系统服务，而是运行自己的 `svcd` 实例。插件、配置、信任根、日志和数据都属于该用户。

## 适用场景

- PC / 工作站：用户自己安装插件，不需要管理员；
- 开发机：不同用户互不影响；
- 不希望把插件代码放进系统级 root daemon。

服务器多租户仍建议“管理员安装 + RBAC”或容器隔离，不建议开放 root 系统 daemon 的安装权。

## 自动选择

- **非 root 进程**：默认进入 user mode；
- **root 进程**：默认 system mode；
- 可显式覆盖：

```bash
SVCD_USER=1 svcd ...   # 强制 user mode
SVCD_USER=0 svcd ...   # 强制 system mode
```

现有系统安装/服务路径不变。

## 目录

Linux（遵循 XDG）：

| 内容 | 默认路径 |
|---|---|
| 配置 | `$XDG_CONFIG_HOME/svcd` 或 `~/.config/svcd` |
| 数据/插件 | `$XDG_DATA_HOME/svcd` 或 `~/.local/share/svcd` |
| 日志 | `$XDG_STATE_HOME/svcd/logs` 或 `~/.local/state/svcd/logs` |

Windows：

- 配置：`%APPDATA%\svcd`
- 数据/日志：`%LOCALAPPDATA%\svcd`

`SVCD_CONFIG_DIR`、`SVCD_DATA_DIR`、`SVCD_LOG_DIR` 仍可覆盖。

## 安装与启动

在非 root 账户下：

```bash
# 1. 把 svcd/svc/plugin/mcp 安装到用户目录
svcd user install

# 2. 加到 PATH
export PATH="$HOME/.local/share/svcd/bin:$PATH"

# 3. 启动后台 daemon
svcd user start

# 4. 查看状态
svcd user status

# 5. 停止
svcd user stop
```

调试时也可以前台运行：

```bash
svcd user run --foreground
```

`user start` 使用当前可执行文件，日志写到用户日志目录；默认 HTTP 端口是 `0`（临时端口），避免每个用户抢 `8787`。

## 安装插件

```bash
svc trust add my-key.pub
plugin install demo-1.0.0.spk
svc start demo
svc list
```

以上命令在非 root 下自动使用用户配置，**不再要求 root/Administrator**。

`svc auth passwd`、`plugin edit-config`、`svc trust add` 在 user mode 下也允许当前用户操作自己的实例。

## 与 root 系统 daemon 的关系

- root 系统 daemon 仍然可以按原方式安装和运行；
- 非 root 用户启动 user daemon 后，会同时存在两个独立实例；
- 两者不共享插件、配置、信任根或任务；
- 建议不要在 user mode 机器上同时启用系统服务，除非确实需要两套实例。

## 自动启动（可选）

当前 `user install` 只安装二进制，不注册开机自启。Linux 可用 systemd user unit：

```ini
# ~/.config/systemd/user/svcd.service
[Unit]
Description=svcd user daemon

[Service]
Type=simple
Environment=SVCD_USER=1
ExecStart=%h/.local/share/svcd/bin/svcd run --foreground
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now svcd.service
```

Windows/macOS 可使用启动项或 LaunchAgent，当前未提供生成器。

## 资源

- daemon 空闲：约 **3.1–3.4 MiB RSS**、**10 个线程**；
- 每个用户一个 daemon：10 个用户空闲约 30–40 MiB daemon RSS；
- 插件进程另算；
- 二进制可共享页缓存，不按用户复制。

## 安全边界

- 插件以当前用户身份运行，只能访问该用户可访问的资源；
- 信任根按用户隔离；
- user mode **不是**强沙箱：同一用户下的插件仍可读写该用户文件；
- 不要把 root 系统 daemon 的安装权开放给普通用户来“实现 user mode”。
