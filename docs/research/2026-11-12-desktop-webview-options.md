# 桌面 WebView 方案选型：webview/webview vs wry + tao（Windows / macOS / Linux）

调研日期：2026-11-12。问题：桌面端用一个 WebView 窗口加载本地反向代理暴露的插件 UI
（`http://127.0.0.1:<port>/plugin-ui/<name>/`），且「嵌入式构建不编译 WebView」。
哪个方案在可用性、分发成本、依赖边界上最省事？

**取证方式说明**：本笔记全部来自一手来源（官方仓库源码/README、微软 Learn、Apple Developer
文档、crates.io API、Arch/Debian 包数据库）。**没有做本机构建或运行实测**——没有在
Windows 上编译 webview/webview，没有 macOS 环境，没有跑 `cargo tree`/`cargo build`。
凡属推断的地方都标注了「推断」，凡查不到的进「未确认项」。

## 结论（先给答案）

1. **三平台都不需要自带浏览器内核**，两个方案都是「用系统引擎的薄壳」：Windows 用
   WebView2（Chromium），macOS 用 WKWebView，Linux 用 WebKitGTK。真正的分发成本差异
   **几乎全部落在 Linux**，Windows/macOS 上两者都接近零。
2. **Windows：WebView2 Runtime 事实上已经是系统组件**。Windows 11 全部预装；微软称
   「vast majority of Windows 10 devices」已安装，并仍建议带上 ~2 MB 的 Evergreen
   Bootstrapper 兜底；Fixed Version 是 **>250 MB** 的自带整包，除非要锁死版本否则不要用。
   → Windows 侧分发成本接近 0，两个方案同价。
3. **macOS：WKWebView 是系统框架（macOS 10.10+），无运行时下载**；但**分发**必须
   Developer ID 签名 + 公证（macOS 10.15 起）+ Hardened Runtime，否则 Gatekeeper 拦。
   另外 **macOS 14 起 ATS 默认不再放行「IP 地址」明文连接**，加载 `http://127.0.0.1:...`
   需要在 Info.plist 里显式开 `NSAllowsLocalNetworking`（或对 IP 段加 `NSExceptionDomains`）。
   这是本方案在 macOS 上**唯一容易踩到的配置坑**，且两个方案完全一样（都是 WKWebView）。
4. **Linux 是唯一真正贵的平台**：`webkit2gtk-4.1` 在 Arch 上是 **38.4 MB 包 / 141.2 MB
   安装后 / 71 个依赖**，Debian trixie 的 `.deb` 为 24.0 MB。结论很清楚：**Linux 上绝不
   自带引擎，声明成发行版依赖**；这也正是「不自带浏览器内核」的实际含义。
5. **webview/webview 的 API 面极小**（一个头文件、19 个 C 函数），能覆盖「建窗口 + 加载
   URL + 双向 JS 绑定 + JS 注入 + 主线程 dispatch」，**但没有**：自定义协议 handler、
   无边框/托盘/菜单、Cookie/下载/权限控制。它的优势是 MIT、单头文件、Windows 端自带
   WebView2 loader 实现（**不需要分发 `WebView2Loader.dll`**）、Linux 端同时支持
   WebKitGTK 4.0 / 4.1 / 6.0 三代 API。
6. **wry 并不是「只能配 tao」**：它接受任何实现了 `HasWindowHandle` 的窗口（官方示例同时
   给了 `tao` 和 `winit`），Linux 上还能 `build_gtk()` 直接挂 GTK 容器以同时支持
   X11/Wayland。代价是 **Cargo 依赖树**（gtk-rs + webkit2gtk-rs + soup3 + objc2-* +
   webview2-com + windows crate…）和**更严的 WebKitGTK 下限**（feature 里写死 `v2_38`，
   即 WebKitGTK ≥ 2.38），发行版覆盖不如 C 库宽。
7. **C 库的「裁剪边界」是编译单元级开关**：每个 backend 整体包在
   `#if defined(WEBVIEW_PLATFORM_LINUX) && defined(WEBVIEW_GTK)` 这类条件里（macOS/Windows
   同理是 `WEBVIEW_COCOA` / `WEBVIEW_EDGE`）。嵌入式构建只要不 include 头、不加
   `webview::core` target，就是**零编译、零链接、零依赖**。
   **Rust 侧没有对等能力**：`os-webview` 是 crate 的**必需** feature（README 原文
   「This must be enabled for the crate to work」），`default-features = false` 只去掉
   x11/serde，去掉 WebView 引擎的唯一办法是**不把 wry 放进依赖图**。
8. **建议：桌面端选 webview/webview**（若近期就要托盘/无边框/多窗口/自定义协议，见下一节
   的切换条件）。它把「窗口 + 本地 URL + JS 桥」这件事的**分发成本压到最低**，且把
   「嵌入式不编译 WebView」变成一个干净的编译期边界；wry+tao 多出来的是 Rust 工具链和
   一棵较深的依赖树，换来的窗口/托盘能力可以用平台原生代码或后续再引入。

## 证据

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| webview/webview 平台绑定：Linux=GTK+WebKitGTK，macOS=Cocoa+WebKit，Windows=Win32+WebView2 | 三平台均复用系统引擎，无自带内核 | https://github.com/webview/webview （README「Platform Support」） | master @ 2026-10-22 |
| 许可证 MIT | 可用于闭源商业产品 | https://github.com/webview/webview/blob/master/LICENSE | master |
| 最近提交 2026-10-22；最近 release 0.12.0 于 2024-09-11 | 项目**在维护**但**发版很慢**（>2 年无 tag），README 仍 pin 0.12.0 | https://github.com/webview/webview/commits/master.atom ; https://github.com/webview/webview/releases.atom | 2026-10-22 / 0.12.0 |
| 公开 API 全集：create/destroy/run/terminate/dispatch/get_window/get_native_handle/set_title/set_size/navigate/set_html/init/eval/bind/unbind/return/version | 能加载 URL、双向 JS 绑定、窗口生命周期；**无自定义协议、无无边框、无托盘、无菜单** | https://github.com/webview/webview/blob/master/core/include/webview/api.h | master |
| 原生句柄 kinds：UI_WINDOW / UI_WIDGET / BROWSER_CONTROLLER（NSWindow / NSView / WKWebView / HWND / ICoreWebView2Controller…） | 需要无边框/托盘时可自取原生句柄自己写平台代码 | https://github.com/webview/webview/blob/master/core/include/webview/types.h | master |
| backend 选择由编译期宏控制：`WEBVIEW_GTK` / `WEBVIEW_COCOA` / `WEBVIEW_EDGE`，每个 backend 文件整体包在 `#if defined(WEBVIEW_PLATFORM_LINUX) && defined(WEBVIEW_GTK)` 内 | 「不编译 WebView」= 不 include 头/不加 target，编译器层面零残留 | https://github.com/webview/webview/blob/master/core/include/webview/backends.hh ; https://github.com/webview/webview/blob/master/core/include/webview/detail/backends/gtk_webkitgtk.hh | master |
| Windows 内置 WebView2 loader 实现（`WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL` 默认开），`WebView2Loader.dll` 可有可无；不支持 `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` 等环境变量、不支持 Edge Insider 通道 | 少一个必须分发的 DLL（对比微软通用做法） | https://github.com/webview/webview （README「MS WebView2 Loader」） | master |
| 编译产物：Windows 需 C++14 + 较新 Windows SDK + NuGet `Microsoft.Web.WebView2`；其余平台 C++11 | 构建期 Windows 需要一个 NuGet 依赖（头文件），运行期无 | 同上（Prerequisites / Library Dependencies） | master |
| macOS 示例以 `MACOSX_BUNDLE` 构建 | macOS 侧按 .app bundle 交付（WKWebView 需要正常的 app 生命周期） | https://github.com/webview/webview/blob/master/examples/CMakeLists.txt | master |
| WebView2 两种分发：Evergreen（自动更新、共享副本）vs Fixed Version（随包分发、体积大）；Evergreen 为推荐 | 默认走 Evergreen | https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/evergreen-vs-fixed-version | 文档更新 2025-10-15 |
| **Windows 11 全部预装** Evergreen Runtime；微软已向所有合格 Windows 10 设备推送；仍建议带 Bootstrapper 兜底 | Win10/11 上「几乎一定有」，但不能假设 100% | 同上（Windows 11 devices and Windows 10 devices）+ https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution | 2025-10-15 / ms.date 2024-06-27 |
| 「The vast majority of Windows 10 devices have the WebView2 Runtime installed already」，少量设备没有 | 精确百分比未公开 | https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution | ms.date 2024-06-27 |
| Evergreen Bootstrapper ≈ **2 MB**（在线）；另有 Standalone Installer（离线整包）；支持 per-machine / per-user | 兜底成本极低（2 MB 或一个链接） | 同上 | ms.date 2024-06-27 |
| **Fixed Version binaries 超过 250 MB**，会等量撑大安装包；v120 起 Win10 非打包应用还需对目录跑两条 `icacls` 授权 | Fixed Version 只在必须锁版本时用 | 同上 | ms.date 2024-06-27 |
| 微软文档明确描述「把 Fixed Version 打进你的 app 包」「把 Standalone Installer 放进安装器」 | **允许随应用分发**（微软官方推荐路径之一）；许可证条文本身未逐条核对 | 同上 | ms.date 2024-06-27 |
| WebView2 生产环境**不允许**用 Microsoft Edge Stable 作为后端，只能用 Runtime | 必须处理 Runtime 是否存在 | https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution （「Microsoft Edge Stable channel isn't supported for WebView2」） | ms.date 2024-06-27 |
| WebView2 Runtime 109 是最后支持 Win7/8/8.1 的版本；SDK 1.0.1519.0+ 不再支持 | 实际支持面 = Windows 10 及以上 | https://blogs.windows.com/msedgedev/2022/12/09/microsoft-edge-and-webview2-ending-support-for-windows-7-and-windows-8-8-1/ | 2022-12-09（2023-01-17 更新） |
| WKWebView 可用性：macOS **10.10+**、iOS 8.0+（`@MainActor`） | macOS 侧无最低版本压力 | https://developer.apple.com/documentation/webkit/wkwebview | 文档站 2026 |
| 公证：macOS 10.14.5 起新 Developer ID 证书签名须公证；**macOS 10.15 起所有 Developer ID 分发的软件必须公证**；需 Hardened Runtime、安全时间戳、有效 Developer ID 证书 | macOS 对外分发 = 签名 + 公证（一次性流程成本，非运行时成本） | https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution | 文档站 2026 |
| ATS：macOS 10.12–13 默认允许「未限定域名 / .local / IP 地址」；**macOS 14 起 ATS 默认不再允许连接 IP 地址**，需在 `NSExceptionDomains` 里逐个加 IP/CIDR | `http://127.0.0.1` 在 macOS 14+ 需显式放行 | https://developer.apple.com/documentation/bundleresources/information-property-list/nsapptransportsecurity/nsallowslocalnetworking | 文档站 2026 |
| `NSAllowsLocalNetworking`（macOS 10.12+）：布尔，默认 NO，放开「未限定域名 / .local / IP」的 ATS 限制，且会让新系统忽略 `NSAllowsArbitraryLoads` | 首选这个键（比 `NSAllowsArbitraryLoads` 窄） | 同上 | 文档站 2026 |
| Secure Contexts：`127.0.0.0/8` 与 `::1/128` 属于 **potentially trustworthy origin**（与端口无关） | `http://127.0.0.1:port` 本身就是安全上下文，`crypto.subtle`、Service Worker 等可用，**不需要**自定义协议来「升格」 | https://www.w3.org/TR/secure-contexts/ （§3.1 第 4 步） | CRD 2023-11-10 |
| Chrome Local Network Access：定义为「**从公网到本地网/回环**」的请求才需要权限；计划 Chrome 142 上线提示；`fetch` 目标若是私有 IP 字面量则豁免混合内容检查 | 页面本身在 127.0.0.1（回环）时，同源请求不落入「公网→本地」 | https://developer.chrome.com/blog/local-network-access | 2025-06-09（2025-09-29 更新） |
| webkit2gtk-4.1（WebKitGTK）2.54.2-1：包 **38.4 MB**、安装后 **141.2 MB**、**71 个依赖**（gtk3、libsoup3、gstreamer、mesa…） | Linux 绝不自带；打包成发行版依赖 | https://archlinux.org/packages/extra/x86_64/webkit2gtk-4.1/ | 2.54.2-1，构建 2026-10-23 |
| Debian trixie `libwebkit2gtk-4.1-0_2.54.0-1~deb13u3_amd64.deb` 精确大小 **25,210,160 字节（24.0 MB）** | 压缩后的单包量级 ~24 MB | https://packages.debian.org/trixie/amd64/libwebkit2gtk-4.1-0/download | 2.54.0-1~deb13u3 |
| webview/webview 的 WebKitGTK API 可选 6.0 / 4.1（推荐）/ 4.0，并自动挑可用版本；「不同 API 版本间可能有影响功能可用性的重大差异」 | 老发行版（只有 4.0/libsoup2）仍可构建 | https://github.com/webview/webview （README「Linux-specific Options」） | master |
| wry 0.57.2：license Apache-2.0 OR MIT，crate 源码包 222,390 B，rust-version 1.85，edition 2024，发布 **2026-11-10** | 非常活跃；需要较新 Rust 工具链 | https://crates.io/api/v1/crates/wry | 0.57.2 @ 2026-11-10 |
| wry features：default = `os-webview` + `x11`；`os-webview` 被注明「This must be enabled for the crate to work」；0.55 之前的 `protocol` feature 在 0.56+ 的 feature 列表中消失（推断：转为常规能力） | **无法通过 feature 关掉 WebView 引擎**；`default-features=false` 只减 x11/serde | https://github.com/tauri-apps/wry （README「Feature flags」）+ crates.io versions 元数据 | 0.57.2 / 0.55.1 |
| wry 接受任何 `HasWindowHandle` 的窗口，官方示例同时给 `tao` 与 `winit`；Linux 可 `build_gtk()` 以同时支持 X11/Wayland | **不是只能配 tao**；Wayland 需走 GTK 路径 | https://github.com/tauri-apps/wry （README） | dev @ 2026-11 |
| wry 0.57.2 直接依赖（按目标平台）：Linux 侧 gtk 0.18、webkit2gtk `=2.0.2`（feature `v2_38`）、webkit2gtk-sys、javascriptcore-rs、soup3、gdkx11、x11-dl、dirs、percent-encoding；Windows 侧 webview2-com 0.39、windows/windows-core/windows-version 0.62、dunce；Apple 侧 objc2 0.6、objc2-{app-kit,foundation,web-kit,core-foundation,ui-kit}、block2、url | 依赖树主要为各平台绑定 crate；**Linux 要求 WebKitGTK ≥ 2.38**（比 webview/webview 的 4.0 回退更严） | https://crates.io/api/v1/crates/wry/0.57.2/dependencies | 0.57.2 |
| tao 0.37.2：license Apache-2.0，crate 源码包 314,105 B，default features = `rwh_06` + `x11` + `dbus`；**已无 `tray` feature**（旧的 0.16.x 才有 `tray`），README 只列 `serde` | 托盘能力在 Tauri 生态里已不在 tao 内（推断：移到 `tray-icon` 一类独立 crate），需自行确认 | https://crates.io/api/v1/crates/tao ; https://github.com/tauri-apps/tao （README「Cargo Features」） | 0.37.2 @ 2026-10-23 |
| tao 是 winit 的 fork，Linux 端口换成 GTK | 窗口属性 API 与 winit 同源；Linux 上 GTK3 是其硬依赖 | https://github.com/tauri-apps/tao （README「Acknowledgement」） | dev @ 2026-11 |

## 对技术选型的影响（直接给建议，标注确定性）

### 1. 选 webview/webview（确定性：中高）

理由按权重：

- **分发成本**：运行期什么都不用带。Windows 靠预装 WebView2（兜底 2 MB bootstrapper 或一个
  下载链接）、macOS 靠系统 WKWebView、Linux 声明 `libwebkit2gtk-4.1-0` 依赖。**确定性：高**
  （微软与包数据库数据都指向同一结论）。
- **依赖边界干净**：不 include 就不编译，嵌入式构建零残留。C 侧这是「结构性」保证，不是
  CI 里记得关某个 flag。**确定性：高**（源码里 backend 整体裹在 `#if` 中）。
- **构建期唯一硬需求**：Windows 上需要一个 NuGet 包（`Microsoft.Web.WebView2`）拿
  `WebView2.h`；运行期不需要 `WebView2Loader.dll`。**确定性：高**（README 明写）。
- **许可证**：MIT，无附加义务。**确定性：高**。

代价（都是可预期的，不是坑）：

- 没有无边框/托盘/菜单/自定义协议。做无边框：Windows 走 `WM_NCCALCSIZE`/DWM，
  macOS 走 `NSWindow` 的 `styleMask`，Linux 走 `gtk_window_set_decorated`——**三套原生代码**，
  但都能通过 `webview_get_native_handle` 拿到对应窗口对象。**确定性：中**
  （句柄 kind 已由 `types.h` 保证；具体工作量未实测）。
- 没有自定义协议 handler。**你不需要它**：见第 3 点。
- 发版慢（0.12.0 停在 2024-09）。若依赖 master 的新修复，要 pin commit 而不是 tag。
  **确定性：高**。

### 2. 什么时候改选 wry + tao（确定性：中）

同时出现两条以上时再换，否则不值得：

- 近期必须要**无边框 + 托盘 + 多窗口 + 自绘标题栏**，且不想写三套平台 UI 代码；
- 团队已经在 Rust 里（wry 接进来是 `cargo add`，不是新工具链）；
- 需要**自定义协议**（`with_custom_protocol`）或更细的 WebView 控制（devtools、代理配置
  `mac-proxy`、`linux-body`）；
- 可以接受 Linux 上 WebKitGTK ≥ 2.38 的下限，以及 gtk-rs/webkit2gtk-rs 的依赖树。

需要澄清的常见误解：

- **「wry 只能配 tao」不成立**（README 同时支持 tao/winit，Linux 还有 GTK 容器路径）。
  但 Wayland 只有 GTK 路径覆盖，纯 winit 路径官方标注 X11-only。**确定性：高**。
- **「wry 能按 feature 裁掉 WebView」不成立**：`os-webview` 是必需项。想裁剪只能不依赖 wry。
  **确定性：高**（README 原文）。
- **「tao 提供托盘」已过时**：0.37 的 feature 列表里没有 tray。**确定性：中**
  （feature 元数据明确；Tauri 现用哪个 crate 做托盘未核实）。

### 3. 关于 `http://127.0.0.1` 的 CORS / 混合内容 / 证书（确定性：中高）

- **同源，没有 CORS**：页面本身就是 `http://127.0.0.1:<port>/plugin-ui/<name>/`，它调
  同端口的 API 属同源请求。只有「插件 UI 与实际 API 不同端口/主机」时才需要 CORS 头。
  **确定性：高**（同源定义层面）。
- **没有混合内容**：顶层文档是 http，子资源也是 http，不存在 https 页面加载 http 的情况。
  **确定性：高**。
- **没有自签证书问题**：只要坚持 `http://127.0.0.1`（不要为了「安全上下文」上 https），
  就不需要证书、不需要 `webview` 层做证书例外。**确定性：高**。
- **本来就是安全上下文**：Secure Contexts 规范把 `127.0.0.0/8` 列为 potentially
  trustworthy，**与端口无关**。所以 `crypto.subtle`、Service Worker、`navigator.clipboard`
  等都能用，**「必须上 app:// 才能用现代 API」是错的**。**确定性：高**（规范条文）。
- **Chrome/WebView2 的 LNA**：现行定义只管「公网 → 本地网/回环」，页面自己在回环时不受影响。
  但 Chrome 明说**未来会扩展到「本地→本地」的跨源请求**；一旦如此，若把 UI 改成
  `app://` 再 `fetch('http://127.0.0.1')`，就可能落进权限提示甚至被拦。
  **确定性：中**（现状高、未来风险为推断，需实测）。这条本身就是**不要引入 app://** 的理由。
- **macOS 的坑不在 CORS，在 ATS**：macOS 14+ 默认不再放行 IP 地址的明文连接，
  `http://127.0.0.1` 需要 Info.plist 的 `NSAllowsLocalNetworking = YES`。
  **确定性：中高**（键的文档写明 IP 地址规则；「WKWebView 顶层导航也受 ATS 管」这一点
  文档是 ATS 全局口径 + 存在 `NSAllowsArbitraryLoadsInWebContent` 这类 web 专用键，
  但我没有实测 macOS 14 上的实际行为）。

**自定义协议（`app://`）在本设计里没有必要**，反而会引入三个新问题：(a) WKWebView 只能
用 `WKURLSchemeHandler` 拦自定义 scheme（不能拦 http/https），而 webview/webview 没有
这个 API，需要下沉写平台代码或换 wry；(b) 页面从「回环 http」变成「自定义/不透明源」，
`fetch('http://127.0.0.1')` 从同源变成跨源，要重做 CORS 且可能撞上 LNA；(c) 多一层路径
重写与 MIME 处理。**确定性：中高**。

### 4. 「嵌入式不编译 WebView」的裁剪边界到底在哪

| 方案 | 边界形态 | 具体做法 | 确定性 |
| --- | --- | --- | --- |
| webview/webview（C/C++） | **编译单元级** | backend 翻译单元整体由 `WEBVIEW_PLATFORM_*` + `WEBVIEW_GTK`/`WEBVIEW_COCOA`/`WEBVIEW_EDGE` 包裹；嵌入式目标不 include `webview/webview.h`、不链接 `webview::core`/`core_static`，符号与平台库依赖**完全不进二进制**。注意这是「整库开关」，不是「关掉某平台」——每个目标平台只会编进自己那一个 backend。 | 高（源码可见） |
| wry + tao（Rust） | **没有可用开关** | `os-webview` 是必需 feature；`default-features = false` 只去掉 x11/serde。要「不编译 WebView」只能让嵌入式 crate **不依赖 wry/tao**（feature 里把桌面壳整体做成可选依赖）。 | 高（README 原文） |

这条差异对本项目是决定性的：你们的约束是「嵌入式构建不编译 WebView」，C 库天然满足，
Rust 侧则必须靠**上层 crate 的可选依赖**来模拟，容易在 CI 里被 feature unification
悄悄拉回来。**确定性：中高**。

### 5. 落地建议（按确定性排序）

1. 桌面壳只做三件事：开窗口、`navigate('http://127.0.0.1:<port>/plugin-ui/<name>/')`、
   用 `webview_bind` 暴露少量 native 能力（启停插件、开日志目录、退出）。**确定性：高**。
2. 窗口尺寸/标题走库 API；**无边框与托盘第一版先不做**，等真需要再写平台代码或换壳。
   **确定性：中**。
3. 端口要由服务管理器分配后传给 WebView，不要硬编码；URL 里带 token 或依赖同源即可，
   别引入 https。**确定性：中高**。
4. macOS 打包清单：.app bundle + Hardened Runtime + Developer ID 签名 + 公证 +
   `NSAllowsLocalNetworking`。Windows 打包清单：检测 WebView2（注册表
   `...\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` 的 `pv`）→ 缺失则跑
   bootstrapper。Linux：`Depends: libwebkit2gtk-4.1-0`。**确定性：高**（均为微软/苹果文档
   给出的官方做法）。
5. 若后续一定要自定义协议，**先做一次 macOS 14+ 与 WebView2(Chromium 142+) 的实测**
   再决定，不要先改架构。**确定性：中**。

## 未确认项

1. **WebView2 Evergreen 的具体预装比例**。微软只给「Windows 11 全部预装」「vast majority
   of Windows 10 devices」的定性说法，没有百分比或分地区数据。
2. **Evergreen Runtime 安装后的磁盘占用**。微软文档只给了 Fixed Version「>250 MB」与
   Bootstrapper「≈2 MB」，没有 Evergreen 安装体积。
3. **Evergreen Standalone Installer 的下载体积**。文档只说「large, standalone installer」。
4. **wry 0.57.2 的完整传递依赖数量与 Release 二进制增量**。crates.io 只给直接依赖；
   没有跑 `cargo tree` / 构建，无法给出「加了多少 MB」。
5. **webview/webview 官方 macOS 最低部署目标**。README 只写 C++11/C++14 与链接
   `-framework WebKit`，未声明 `CMAKE_OSX_DEPLOYMENT_TARGET`；WKWebView 自身是 10.10+。
6. **macOS 14+ 的 ATS IP 限制对 WKWebView 顶层导航的实际拦截行为**。键的文档口径是全局
   ATS；我没有 macOS 14/15/26 环境实测「不加 `NSAllowsLocalNetworking` 时
   `load(URLRequest(http://127.0.0.1))` 是否报 -1022」。
7. **`app://` 方案下 `fetch('http://127.0.0.1')` 是否触发 Chrome LNA 提示**。基于
   「未来会扩展到本地→本地」的表述推断为「可能」，未实测。
8. **webview/webview 在无 .app bundle 的裸可执行文件下 WKWebView 的完整能力**
   （菜单、激活策略、媒体权限等）。官方示例用 `MACOSX_BUNDLE`，但未说明裸二进制是否可行。
9. **`WebView2Loader.dll` 分发与 WebView2 SDK 的许可证条文细节**。微软文档明确把
   「随应用分发 Runtime」当作官方路径，但 NuGet 包内 license 条款未逐条阅读。
10. **tao 0.37 的托盘替代方案**（Tauri 现用哪个 crate、`tray-icon` 是否可直接用）未核实；
    仅确认 tao 自身已经没有 `tray` feature。
11. **三平台无边框/托盘在 C 方案下的实际工作量**。只是按平台 API 推断，没有写验证代码。
12. **WebView2 支持的最低 Windows 10 版本号**。微软「Supported operating systems」页
    本次抓取超时；仅确认 Win7/8.1 在 Runtime 109 终止支持，故支持面为 Windows 10 及以上。
