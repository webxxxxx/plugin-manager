# 管理员密码与可配置的配置目录

`svc lan` 把「LAN 开关」和「管理员 token」绑在一起：首次 `enable` 生成随机 token、写 `lan-token`、只打印一次，UI 用 token 登录。token 长、不能记忆，用户经常把 `svcd.token`（本地控制通道）和 `lan-token` 混用；`svc lan token rotate` 又让「改密码」和「轮换 API 凭据」耦合。配置目录则固定在平台路径，Windows 默认落在 `%ProgramData%\svcd\config`，与用户可读写的 home 不一致。

## Decision

- CLI 命令 `svc lan` 改名为 `svc auth`；子命令为 `status` / `enable [--bind]` / `disable` / `passwd <password>`。删除 `token rotate`。
- `svc auth passwd <password>` 只接收明文一次，由 daemon 生成随机盐的 PBKDF2-HMAC-SHA256 哈希，写入 `svcd.conf` 的 `auth_passwd`，并立即清空已有会话。明文不写文件、不写日志。
- `svc auth enable` 要求 `auth_passwd` 已设置；未设置直接失败并提示先执行 `svc auth passwd`。安装器在 `lan_default = true` 但没有密码时只警告，不自动开启 LAN。
- 网页登录表单改为密码；`POST /login` 接收 `{password}`，设置 `svcd_session`（Path=`/`）与 `svcd_api`（Path=`/api/v1/`）两个 HttpOnly cookie。浏览器之后只用 cookie，不把密码放进 sessionStorage，也不在 API 请求里发 Bearer。
- `/api/v1/*` 与 `/mcp` 的 Bearer 也改为密码；每次 Bearer 请求校验哈希。删除 `lan-token` 文件与相关读取路径。
- `svcd.conf` 增加 `config_dir`：配置目录覆盖项，`svcd.conf`、`trust.d`、`lan.json`、`svcd.token` 都在这里。默认 Linux `/etc/svcd`、macOS `/Library/Application Support/svcd/config`、Windows `%USERPROFILE%\.svcd`；`SVCD_CONFIG_DIR` 仍用于测试且优先级最高。
- `config_dir` 在配置加载后生效；daemon 在 `settings::init_from_path` 之后重新解析一次，避免启动前使用旧默认目录。

## Considered Options

- **保留 token，只增加 passwd（拒绝）**：两套凭据继续造成混用；用户已经明确要求删除 token rotate。
- **明文密码存 `svcd.conf`（拒绝）**：配置会被备份/导出，明文口令泄露面太大。
- **每次 Bearer 都跑高迭代 PBKDF2（拒绝）**：MCP/API 每请求都验哈希；10k 迭代在本地吞吐和测试里可接受，后续可加验证缓存/会话 token。
- **把 `config_dir` 仅当作日志/数据目录（拒绝）**：用户要求它指定 `svcd.conf` 所在目录，且 token/trust 必须跟着走。

## Consequences

- 旧 `lan-token` 登录立即失效；升级后必须执行一次 `svc auth passwd`。旧 token 文件可删除，不再读取。
- 密码通过明文 LAN Bearer 使用，v1 仍无 TLS；只在可信 LAN 开启。
- 会话 cookie 的 `svcd_session` Path=`/`，登出请求能真正到达 `/logout` 并撤销服务端会话；API cookie 仍限制在 `/api/v1/`。
- Windows 服务若以 LocalSystem 运行，`%USERPROFILE%` 是系统账户的 home；需要真正的用户 home 时用 `--conf`/`config_dir` 明确指定。
- 配置目录变化后，`svc`/`daemon` 的 token、trust、lan.json 都跟着走；已有部署需要把文件迁移到新目录或显式写回旧路径。
