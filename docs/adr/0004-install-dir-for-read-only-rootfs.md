# 只读根文件系统上的 install_dir 与 svcd_daemon.sh

SR6110 的根文件系统里只有 `/etc` 持久：`/usr/sbin` 在 ROM 里写不了，`/var` 重启即丢；`/etc/inittab` 和 `/etc/sr/runtime/sshd.sh` 也在 ROM 里，只能随固件刷机改。默认的 `--prefix /usr/sbin`、`data_dir=/var/lib/svcd`、`logs_dir=/var/log/svcd` 与「改一行 inittab 就能开机自启」的假设在这儿都不成立。

## Decision

- `--prefix` 是安装根的唯一命令行输入：`svcd --install --prefix <dir>` 把二进制、`svc`/`plugin`/`mcp` 别名装到 `<dir>`，并把生效值以 `install_dir = <dir>` 写回安装后的 `svcd.conf`。相对 `--prefix` 先转成绝对路径再记录。
- `--prefix` 未给时，沿用已安装 `svcd.conf` 里的 `install_dir`（升级不用每次带 `--prefix`），再退回平台默认。`--conf` 里的 `install_dir` 在安装时解析为绝对路径后写回，避免 conf 复制到别的目录后相对基准漂移。
- `install_dir` 继续是运行时的安装根：`<install_dir>/data` 与 `<install_dir>/logs` 作为 data/logs 的默认值。运行时 `paths::data_dir`/`logs_dir` 的顺序是 `SVCD_DATA_DIR`/`SVCD_LOG_DIR` → `install_dir` → 平台默认，所以 daemon 和直接运行的 `svc`（`svc logs` 等）看到同一棵树。安装器（`--install`/`--uninstall`）按目标 `svcd.conf` 解析并忽略当前 shell 的这两个环境变量：它生成并打印的是开机脚本在干净环境里使用的路径。
- `--manual` 负责手动启动场景：跳过所有 init 服务注册（显式 `--target` 也跳过并打印提示），按宿主系统生成 `svcd_daemon.sh`（Unix）或 `svcd_daemon.bat`（Windows）。脚本导出 `PATH`、`SVCD_DATA_DIR`、`SVCD_LOG_DIR` 后启动 daemon；Unix 用 `exec`，让 BusyBox 的 `::respawn:` 监督 daemon 本身而不是包一层 shell。输出会列出这些变量，提示直接手动启动时要自行 export。
- 没有 `--manual` 时照常注册 init 服务；指定了 `install_dir`/`--prefix` 也照常注册。SR6110 必须用 `--manual` 才不碰 ROM 里的 inittab。
- `--no-service` 仍然只是不注册服务；它不生成启动脚本，也不打印环境变量提示。
- `--uninstall` 不接受 `--conf`/`--manual`，因此从已安装的 `svcd.conf` 推断 `install_dir`；`--prefix` 仍可覆盖。`--purge` 一并删除 `<install_dir>/data` 与 `<install_dir>/logs`，并删除存在的手动启动脚本。
- `config_dir` 语义不变：仍默认平台路径（SR6110 上就是 `/etc/svcd`），也仍可由 `SVCD_CONFIG_DIR` 或 `config_dir` 覆盖。启动脚本用 `--conf` 传配置位置，不导出 `SVCD_CONFIG_DIR`，避免盖掉配置里自己的 `config_dir`。

## Considered Options

- **把 `--prefix` 只当一次性的文件落点**（拒绝）：升级和卸载都需要重复传同样的值，conf、data/logs 与卸载清理容易漂移；现在回写 `install_dir` 让三者对齐。
- **每次安装都手写启动脚本**（拒绝）：`--manual` 生成并列出环境变量，少一次手工同步。
- **让 `install_dir` 同时决定 `config_dir`**（拒绝）：`config_dir` 已能单独配置；把两件事绑死会让「配置在 `/etc`、程序在别处」这种组合没法表达。
- **启动脚本改成 systemd/procd 单元**（拒绝）：SR6110 是 BusyBox init，能用的接点只有 ROM 里的 inittab；脚本是最小、可刷入的交付物。
- **由安装器直接改 ROM 里的 inittab/sshd.sh**（拒绝）：它们只读，写失败会把「文件已装好」变成整体安装失败。

## Consequences

- SR6110 的固件需要把 `svcd_daemon.sh` 和新的 inittab 行一起刷入；换 svcd 版本可重跑 `--install --prefix … --manual`（安装器用临时文件 rename 替换二进制，运行中的 daemon 不会遇到 ETXTBSY）。
- `install_dir` 写进 launcher 的 `SVCD_DATA_DIR`/`SVCD_LOG_DIR` 与 `PATH` 后就固定了；改动它必须重跑 `--install … --manual` 重新生成 launcher（视部署重刷固件）。直接运行 `svc`（不经过 launcher）按 `svcd.conf` 解析，在脚本重新生成前可能与 daemon 不一致。
- 从 `svcd.conf` 删掉 `install_dir` 后，下次不带 `--prefix` 的安装会退回平台默认根；已回写的值不会自动消失。
- `svc` 通过「程序目录的 svcd.conf」或平台配置目录发现 `install_dir`；如果 `config_dir` 被改到别处，直接运行 `svc` 需要先设置 `SVCD_CONFIG_DIR`，这与现有 `config_dir` 行为一致。
