# Windows 系统模式使用机器级配置目录

## Status

Accepted。取代 [ADR-0003](0003-password-auth-and-config-dir.md) 中「Windows 默认 `%USERPROFILE%\.svcd`」的取值；#144 的显式 DACL 已按 `%ProgramData%\svcd` 判定机器级安装，本 ADR 把配置默认值对齐。

## Context

SCM 服务以 LocalSystem 运行，`%USERPROFILE%` 是 `C:\Windows\System32\config\systemprofile`，而且这个 profile 默认连管理员都打不开。控制管道的名字由配置目录哈希得到（`channel::platform::windows::pipe_name`），所以服务和 CLI 必须解析出**同一个**配置目录才能连上。

旧默认值 `%USERPROFILE%\.svcd` 让两边各算各的：服务得到 systemprofile，管理员得到自己的 profile，管道名不同，`svc list` 只会报 `svcd control pipe is not available`。同一个错位还会让 `trust.d` 和 `auth_passwd` 分散在两个 profile 里。与此同时，`is_machine_wide`、目录加固和控制端点 ACL 都以 `%ProgramData%\svcd` 为机器级安装的判据。

## Decision

- Windows 的 `platform_config_dir()` 返回 `%ProgramData%\svcd\config`（`ProgramData` 可被环境变量覆盖，缺省 `C:\ProgramData`）。
- 非 root / 非 Administrator 仍是 user mode，配置在 `%APPDATA%\svcd`；`SVCD_CONFIG_DIR` 与 `svcd.conf` 的 `config_dir` 仍可覆盖。
- 系统模式的 `svcd.conf`、`trust.d`、`auth_passwd` 由服务和管理员 CLI 共用；默认路径落在 `is_machine_wide` 的机器级 tier，控制端点使用 shared / admin 两条 DACL。
- 机器级目录（`config`、`services.d`、`plugins`）对 `svcd` 组只读：组可以检查配置和日志，但不能替换以 SYSTEM 运行的脚本或插件；读写只属于控制管道的 read endpoint。
- 非提升的 CLI 不自动切换到机器级配置。连接失败且机器级配置存在时，`svc list/status` 提示用提升的终端，或加入 `svcd` 组走只读 tier。
- Linux `/etc/svcd` 与 macOS `/Library/Application Support/svcd/config` 不变。

## Consequences

- 已按旧默认安装的 Windows 系统服务需要重装或显式 `--conf` / `config_dir` 迁移；systemprofile 里的旧配置不会自动复制。
- 只读命令仍然需要提升或 `svcd` 组成员：非提升的管理员令牌里 Administrators 是 deny-only，读端点 ACL 不会因为“是管理员”而放行。
- 旧的按用户配置不再被系统模式读取；user mode 与之前一样，不受影响。
- 回归由 `tests/install.rs::the_service_account_and_an_elevated_admin_share_one_config_directory`、`svc.rs` 的不可达提示测试，以及 `dirs` / `acl` 的单测锁住。
