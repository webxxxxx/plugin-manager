# 静态站点：一个配置目录，挂在 `/static/`

## Status

Accepted（2026-11-23）。新增一个只读 HTTP 面；不改变 S4 的连接所有权、认证与内存预算。

## Context

- svcd 已经有两条「从磁盘送文件」的路径：插件 UI（`/plugin-ui/{id}/{token}/`，带插件 token、HTML 注入 `<base>`）与 blob/file 下载（内容寻址、走 storage）。两者都解决不了「把一个普通前端构建目录原样挂出来」：前者要求 `.spk` 与 token，后者要求内容先进入 blob store。
- 控制台独占 `/` 与 `/index.html`，不能把项目目录直接挂在根路径，否则会遮蔽控制面或需要按 Host/端口再分一套监听。
- 静态站点是**公开内容**（浏览器直接加载 HTML/JS/CSS/图标），但 svcd 的 LAN 面默认要求口令；安全边界必须由现有的准入判断（ADR-0021）表达，不能新开一套 token 或绕开 401。

## Decision

- **一个全局配置键 `static_root`**：Path，默认未设（关闭）；相对值按 `svcd.conf` 所在目录解析，和 `config_dir` / `data_dir` / `logs_dir` 一致。它不进派生目录集，也不改变进程的 config/data/logs 根。
- **挂载在 `/static/`**：`/static` 308 到 `/static/`；`/static/` 与 `/static/<dir>/` 取该目录的 `index.html`；`/static/<dir>`（无尾斜杠且确实是目录）308 到带斜杠形式，保证相对链接按目录解析。缺失文件严格 404，不做 SPA fallback，不列目录。
- **只接受 `GET` / `HEAD`**：其他方法 405 + `Allow: GET, HEAD`，在读 body 之前拒绝。HEAD 发真实 `Content-Length`，不发文件字节。
- **跟随 daemon 的准入**：复用 `Admission`（loopback 无口令 = 无需登录；LAN 绑定 = 会话 / Bearer）。未认证的 HTML 导航与插件 UI 一样 303 到 `/?next=…`，API/资源请求仍是 401。
- **路径规则沿用插件 UI 的解析器**：逐段 percent-decode，拒绝 `.` / `..` / 空段 / 反斜杠；canonicalize 后必须仍在根内；额外拒绝所有点文件与点目录。符号链接逃出根目录同样 404。
- **字节由共享的 `AssetStream` 送**：与插件 UI 共用条件 GET（ETag/304）、MIME 与 `FileSend` 零拷贝路径；静态站点不注入 `<base>`，因为页面按 `/static/` 前缀编写相对 URL。一次性文件不占 `max_streams`。

## Considered options

- **再开一个静态监听端口（`static_listen`）**：能真正把目录挂在 `/`，但要新增 listener、线程/轮询槽、auth 与生命周期，且与 daemon 的 bind 回退规则重复。当前需求用路径前缀即可满足。
- **挂在 `/` 并替换控制台**：会遮蔽管理面，也需要决定未配置时是否回退控制台；不做。
- **复用插件 UI 的 token + `<base>` 注入**：普通项目没有 `.spk`，也不该为每个预览页面签 token；注入 `<base>` 还会改变页面作者写的相对路径语义。
- **公开路由（跳过准入）**：配置一个目录就等于把它暴露到 LAN 上，默认不安全；需要公开时应该由反向代理或以后单独的显式开关承担，而不是静态站的默认行为。

## Consequences

- 新配置键出现在 README、`svcd.conf.example`、`config_doc` 与设置界面（只读），改完需要重启 daemon。
- 静态站点的文件读写都发生在每次请求：根目录不存在时 `/static/` 回答 404，启动日志给一条警告；不在启动期缓存目录清单。
- 没有 Range、目录列表、SPA fallback 与自定义 404 页面；需要这些语义时应由前置反向代理或专门的文件服务承担。
- 控制台资产（`src/assets/static/`）仍编译期内嵌并 gzip，由 `face/console` 在 `/`、`/index.html`、`/favicon.svg` 送出；不解压到 `data_dir`，也不与 `static_root` 共用目录：控制台必须公开，而可写目录里的同源脚本会把本地写权限升级成管理员会话。`static_root` 只承载用户自己的静态站点。
