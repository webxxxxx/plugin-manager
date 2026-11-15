# 未签名安装：只由 svcd.conf 控制

`.spk` 的默认准入是 fail closed：`signature.json` 必须是第二个条目、keyId 必须在 `trust.d` 里、manifest 签名必须通过。现场需要安装本地构建或内网尚未签名的包（设备样张、临时工具），而信任根只能由管理员手工添加，无法覆盖「根本没有签名」的包。此前的规格明确写着「不提供 `--insecure`」，与这个需求直接冲突。

## Decision

- **主机级开关是唯一入口**：`svcd.conf` 新增 `allow_insecure_install`，默认 `false`。只有能改本地 conf 的管理员才能把它设为 `true`；仅持有 Web 密码 / HTTP 权限的人不能自行开启。
- **没有逐次安装的 insecure 参数**：CLI 没有 `--insecure`，HTTP 没有 `?insecure=true`，Web 安装面板也没有勾选项。开关打开后，普通的上传 / `plugin install` / `POST /api/v1/plugins` 就按下面的规则工作；开关关闭时，未签名包按普通验签失败拒绝。
- **先严格验签，失败才降级**：开关打开时，安装路径先走完整的签名校验；只有验签这一步失败，才用 `SignatureCheck::AllowUnsigned` 重试并接受。`files[]` 的 size/sha256/mode、平台、schema、路径、hooks/interpreter 规则在所有情况下都不放宽。
- **接受未知 key 的签名**：开关打开后，有 `signature.json` 但 key 不在 `trust.d`、或签名无效的包也会被接受；这与「未签名」同属显式降级。
- **不伪造签名**：未签名安装的版本目录里不会写出 `signature.json`。因为导出 `.spk` / `.spkb` 需要重建签名包，这类插件导出会明确失败并提示重装签名包。
- **可见的降级**：只有真正走了 fallback 的安装才在 svcd 日志追加 `signature was NOT verified (allow_insecure_install)`；正常通过验签的安装不受影响。

## Considered Options

- **Web 安装面板的勾选 / 每次安装的 `--insecure`**：需要把「是否跳过验签」暴露给每个上传者，页面误点或脚本随手传参就会降级；而且 Web 密码等价 root，远程面无法区分。改为只由本地 conf 决定。
- **只加 `svcd.conf` 开关，但保留每次安装的 flag 作为二次确认**：多一层确认，但 CLI / Web / API 都要重复实现，且用户明确要求「配置中打开就可以装」；不采纳。
- **只允许空签名（missing signature），仍拒绝无效签名**：更窄，但用户无法表达「我知道这个来源，忽略未知 key」；实现上还要区分「缺签名」和「签名坏」两种失败，收益有限。
- **改 `trust.d` / 内置 store 公钥**：只解决「key 未知」，解决不了「包未签名」，还需要管理员权限与额外文件。
- **编译器/入口默认放行 + 警告**：会让所有安装入口失去签名保证，违背 fail-closed 的默认，不采纳。
- **单独的无签名包格式（例如 `.spku`）**：多一套格式、检查器和文档，而 tar 布局本身不需要变化；不采纳。

## Consequences

- 开启后，任何能发起安装的调用方（本机 CLI、Web 密码持有者、HTTP 客户端）都可以装未签名包；这是本地管理员明确接受的主机级取舍。关闭时行为与改动前完全一致。
- `allow_insecure_install` 在 svcd 启动时加载一次，修改后必须重启 svcd 才生效；生成的默认 conf 与示例都显式写出 `allow_insecure_install = false`。
- insecure 模式下，安装包完整性只剩 manifest 自带的 `files[]` 哈希；manifest 可被替换，因此这条路径不能作为发布渠道，只适合受控的本地/内网来源。
- 未签名安装的插件不能导出为签名包，升级也必须继续依赖开关或先改签；`export` 的错误信息会说明这一点。
