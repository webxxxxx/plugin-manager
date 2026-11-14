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
# 1. 和 root 安装参数一致，但自动装到当前用户目录
svcd --install --no-service
# 也可以用：
# svcd install --manual
# svcd install --prefix ~/apps/svcd --config ./svcd.conf

# 2. 加到 PATH
export PATH="$HOME/.local/share/svcd/bin:$PATH"

# 3. 启动后台 daemon
svcd start

# 4. 查看状态
svcd status

# 5. 停止
svcd stop
```

`--install`、`--uninstall` 会按当前用户自动判断，不需要 `svcd user install`。  
`--prefix`、`--conf`/`--config`、`--manual`、`--no-service`、`--dry-run` 都保留；`--target` 在 user mode 下接受但忽略（不会注册系统服务）。

`--uninstall` 会先停止用户 daemon，再删除二进制；如果 `svc`/`plugin`/`mcp` 进程仍占用文件，会给出 warning 并继续，关闭对应进程后再次执行即可。

Windows 安装会把 `<data>\\bin` 追加到当前用户的 `HKCU\\Environment\\Path`，并广播环境变更；**新开一个终端**即可直接使用 `svcd` / `svc` / `plugin` / `mcp`。已有终端仍使用旧 PATH，需要重新打开或执行：

```powershell
$env:Path = "$env:LOCALAPPDATA\svcd\bin;$env:Path"
```

Linux/macOS 安装会把同一个目录写进 shell 配置（`.profile`，以及存在的 `.bashrc` / `.zshrc`）；重新打开 shell 或 source 对应文件后即可直接使用。

`svcd user start|stop|status|install` 仍保留为兼容别名，分别对应顶层 `svcd start|stop|status|--install`。

### Windows：从低完整性目录运行时安装失败

如果 `svcd.exe --install` 报：

```text
cannot create C:\Users\<user>\AppData\Local\svcd\bin: 拒绝访问 (os error 5)
```

先检查二进制是否带了低完整性标签：

```powershell
icacls .\svcd.exe | findstr Mandatory
```

出现 `Mandatory Label\Low Mandatory Level` 时，进程会以低完整性运行，不能写普通的用户目录。把二进制复制到普通用户目录后再安装：

```powershell
Copy-Item .\svcd.exe "$env:LOCALAPPDATA\Temp\svcd.exe" -Force
& "$env:LOCALAPPDATA\Temp\svcd.exe" --install --no-service
```

安装后的 `%LOCALAPPDATA%\svcd\bin\svc.exe` 可以正常使用。  
不要直接从沙箱/低完整性构建目录运行安装器。

调试时也可以前台运行：

```bash
svcd run --foreground
```

`start` 使用当前可执行文件，日志写到用户日志目录；默认 HTTP 端口与 root 一致，是 `8787`。多个用户同时运行时要各自在 `svcd.conf` 里改成不同端口，否则会冲突。

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

当前用户安装只安装二进制/别名，不注册开机自启。Linux 可用 systemd user unit：

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
