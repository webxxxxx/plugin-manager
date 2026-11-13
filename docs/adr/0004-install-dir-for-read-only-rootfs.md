# 只读根文件系统上的 install_dir 与 svcd_daemon.sh

SR6110 的根文件系统里只有 `/etc` 持久：`/usr/sbin` 在 ROM 里写不了，`/var` 重启即丢；`/etc/inittab` 和 `/etc/sr/runtime/sshd.sh` 也在 ROM 里，只能随固件刷机改。默认的 `--prefix /usr/sbin`、`data_dir=/var/lib/svcd`、`logs_dir=/var/log/svcd` 与「改一行 inittab 就能开机自启」的假设在这儿都不成立。

## Decision

- `svcd.conf` 增加 `install_dir`。相对值相对 `svcd.conf` 所在目录解析，与 `config_dir`/`plugins_dir`/`services_dir` 一致。
- 指定后，`svcd --install` 以它为安装根：二进制与 `svc`/`plugin`/`mcp` 别名装在这里，同时生成 `svcd_daemon.sh`。命令行 `--prefix` 仍然优先于它；不带 `--conf` 时沿用已安装的 `<config>/svcd.conf` 里的 `install_dir`，升级不会退回 `/usr/sbin`。
- `<install_dir>/data` 与 `<install_dir>/logs` 作为 data/logs 的默认值。运行时 `paths::data_dir`/`logs_dir` 的顺序是 `SVCD_DATA_DIR`/`SVCD_LOG_DIR` → `install_dir` → 平台默认，所以 daemon 和直接运行的 `svc`（`svc logs` 等）看到同一棵树。安装器（`--install`/`--uninstall`）按目标 `svcd.conf` 解析并故意忽略当前 shell 的这两个环境变量：它生成并打印的是开机脚本在干净环境里使用的路径。
- `svcd_daemon.sh` 导出 `PATH`（安装根目录 + 原 PATH）、`SVCD_DATA_DIR`、`SVCD_LOG_DIR`，然后 `exec <install_dir>/svcd run --foreground --conf <config>/svcd.conf`。`exec` 让 BusyBox 的 `::respawn:` 监督 daemon 本身而不是包一层 shell。
- 指定 `install_dir` 时 `--install` 不再自动注册 init 服务，只打印要随固件刷入的 `::respawn:<install_dir>/svcd_daemon.sh`；`/etc/inittab` 在 ROM 里，安装器改不了也不该报错。显式 `--target` 也一并跳过并打印提示，由固件 owner 决定启动方式。
- `--uninstall` 不接受 `--conf`，因此从已安装的 `svcd.conf` 推断 `install_dir`；`--purge` 一并删除 `<install_dir>/data` 与 `<install_dir>/logs`。
- `config_dir` 语义不变：仍默认平台路径（SR6110 上就是 `/etc/svcd`），也仍可由 `SVCD_CONFIG_DIR` 或 `config_dir` 覆盖。启动脚本用 `--conf` 传配置位置，不导出 `SVCD_CONFIG_DIR`，避免盖掉配置里自己的 `config_dir`。

## Considered Options

- **每次安装都手写 `--prefix` 并手工维护启动脚本**（拒绝）：升级时容易漏掉脚本里的路径，`svc logs` 与 daemon 的日志目录也会漂移。
- **让 `install_dir` 同时决定 `config_dir`**（拒绝）：`config_dir` 已能单独配置；把两件事绑死会让「配置在 `/etc`、程序在别处」这种组合没法表达。
- **启动脚本改成 systemd/procd 单元**（拒绝）：SR6110 是 BusyBox init，能用的接点只有 ROM 里的 inittab；脚本是最小、可刷入的交付物。
- **由安装器直接改 ROM 里的 inittab/sshd.sh**（拒绝）：它们只读，写失败会把「文件已装好」变成整体安装失败。

## Consequences

- SR6110 的固件需要把 `svcd_daemon.sh` 和新的 inittab 行一起刷入；之后换 svcd 版本只覆盖 `<install_dir>/svcd`（或重跑 `--install --conf`）即可。安装器用临时文件 rename 替换二进制，运行中的 daemon 不会遇到 ETXTBSY。
- `install_dir` 写进 launcher 的 `SVCD_DATA_DIR`/`SVCD_LOG_DIR` 与 `PATH` 后就固定了；改动它必须重跑 `--install --conf` 重新生成 launcher（视部署重刷固件）。直接运行 `svc`（不经过 launcher）按 `svcd.conf` 解析，在脚本重新生成前可能与 daemon 不一致。
- `svc` 通过「程序目录的 svcd.conf」或平台配置目录发现 `install_dir`；如果 `config_dir` 被改到别处，直接运行 `svc` 需要先设置 `SVCD_CONFIG_DIR`，这与现有 `config_dir` 行为一致。
