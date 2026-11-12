# 桌面发行基线：发行版覆盖、安装包格式、签名公证与体积（Windows / macOS / Linux）

调研日期：2026-11-12。问题：桌面版（webview 单一 feature）要把 **svcd 装成系统级守护进程**
（systemd system unit / launchd LaunchDaemon / Windows SCM）、把插件放进
`/var/lib/svcd/plugins/<id>/<version>`，并为每个平台选一种安装包格式、估一个体积与签名成本。
本笔记只补「发行版基线 / 安装包格式的适用性与限制 / 签名与公证流程与成本 / 安装包体积基线 /
当前 OS 支持窗口」这五块。

**不重复的内容**：WebView 选型与 WebView2 Evergreen/Fixed、macOS 需签名+公证+Hardened
Runtime、Linux 需 libwebkit2gtk-4.1-0、WebKitGTK 包体积量级，见
`docs/research/2026-11-12-desktop-webview-options.md`；四平台服务安装字段见
`docs/research/2026-11-12-process-supervision-and-service-managers.md`。

**取证方式说明**：全部来自一手来源（Ubuntu/Debian/Launchpad 包数据库、Fedora Packages、
Rocky（RHEL 重建）镜像目录、微软 Learn、Apple Developer 文档 JSON、CA/Browser Forum 规范、
rust-lang.org）。**做了一次本机实测**：用 Python 手工拼一个最小 `.deb` 并量出容器开销。
**没有做**的是：MSI/NSIS/pkg/dmg 的构建实测（本机无 WiX/NSIS/macOS，且沙箱内 TLS 被拦，
无法下载工具链），也没有 macOS/Windows 实机安装验证。凡属推断都标注「推断」，
查不到的进「未确认项」。

## 结论（先给答案）

1. **桌面 Linux 的基线应定在 Ubuntu 22.04 / Debian 12**：`libwebkit2gtk-4.1-0` 在这两个
   LTS 的官方仓库里都存在（Ubuntu 22.04 在 **universe**，2.50.4；Debian 12 在 main，
   2.50.6）。比它们老的发行版没有 4.1：**Debian 11 明确「Package not available in this
   suite」**，而 **RHEL 9 只有 `webkit2gtk3`（4.0 API）**——这是本轮最容易被忽略的一条，
   「声明依赖 4.1」等于**放弃 RHEL 9**。**确定性：高**（四个包页/镜像目录逐条核对）。
2. **真正的下限不是 glibc，是「发行版仓库里有没有 4.1 包」**。Rust 的
   `x86_64-unknown-linux-gnu` 二进制下限仍是 **glibc 2.17 / kernel 3.2**（Rust 1.64 起），
   远低于任何带 4.1 的发行版；一旦依赖 4.1，实际下限被拉到 Ubuntu 22.04 的 glibc 2.35 /
   Debian 12 的 2.36 这一档。**确定性：高（glibc 2.17 部分）/ 中（发行版 glibc 版本号未逐页核）**。
3. **musl 静态构建链接不了 WebKitGTK，桌面 Linux 必须用 gnu target**。WebKitGTK 只以共享库
   形式发布（Debian 包名就是 `libwebkit2gtk-4.1-0`，只有 `.so.0`），musl 静态二进制既没有
   静态 libwebkit 可链，也不能把 glibc 编译的 DSO 拉进 musl 进程。**确定性：高（结论）/
   中（机制细节为推断）**。
4. **Flatpak / Snap / AppImage 都不能承担「root 装系统服务 + 写 /var/lib/svcd」**：Flatpak
   默认「No access to any host files except the runtime, the app …」；AppImage 官方口径是
   「you do not need to install AppImages」——没有安装步骤就没有「以 root 注册服务」的位置；
   snap 是三者里唯一有 daemon 机制的（snapd 管服务），但数据目录被限在
   `$SNAP_DATA`（`/var/snap/<name>/<rev>`）。→ **系统级守护进程只能走 deb/rpm**。
   **确定性：高（Flatpak/AppImage 引文）/ 中（snap 的结论是结构推断）**。
5. **deb/rpm 的分工**：`Depends: libwebkit2gtk-4.1-0`（rpm 对应 `Requires: webkit2gtk4.1`）、
   postinst 建系统用户/组、unit 文件属于包（`/usr/lib/systemd/system`）。**自建 apt 仓库必须
   签 Release/InRelease**（apt 只认签名文件），**直接给单个 .deb 下载则没有强制签名**——
   两种分发方式的签名义务不同。**确定性：中高**。
6. **Windows 侧「MSIX 不能装服务」是过时结论**：`desktop6:Service` 明确写「Specifies a
   service that is installed and registered along with the app」，可跑 localSystem /
   localService / networkService，MSIX 能力表也标 `Windows services in MSIX ✅ (requires
   Windows 10 version 2004+)`。但 MSIX 要包身份、按用户安装的模型与「每机器系统守护进程 +
   共享数据目录」仍不贴合；**MSI/WiX（ServiceInstall）与 NSIS 更直接**。**确定性：高（MSIX 能装
   服务）/ 中（「更贴合」是工程判断）**。
7. **代码签名在 2026 年是持续成本，不是一次采购**：CA/B Code Signing BR 规定
   **2023-06-01 起私钥必须在硬件密码模块（HSM/令牌）内生成、存储、使用**；
   **2026-03-01 起签发的代码签名证书有效期不得超过 460 天**；**2026-09-15 起订户证书必须带
   保留策略 OID**。→ 每年换证 + 硬件令牌或云 HSM 订阅，CI 里要有签名服务。**确定性：高**。
8. **macOS 只有一个格式能装 LaunchDaemon：`.pkg`**。dmg 只是容器（内含 pkg 或 .app）。
   Apple 的公证服务接受 **UDIF 磁盘映像、flat installer package、ZIP、.app/非 app bundle**，
   并「processes nested containers as well, like packages inside a disk image」；`stapler`
   可对 **app / bundle / dmg / flat pkg** 打票，**ZIP 不能直接 staple**（要逐项 staple 后重打）。
   macOS 13+ 的 `SMAppService` 是「在自己的 app bundle 里注册 LaunchDaemon」的官方替代路径，
   替代往 `/Library/LaunchDaemons` 写 plist。**确定性：高**。
9. **macOS 最低部署目标 11.0**（`aarch64-apple-darwin` = ARM64 macOS 11.0+），2026-11 的
   当前版本是 **macOS 27 "Golden Gate"**；按 Apple 惯例支持窗口是 **27 / 26 / 25** 三个大版本
   （N/N-1/N-2）。**确定性：高（11.0、27 为当前）/ 中（N-2 窗口是惯例推断）**。
10. **体积上 30MB 预算的关键不在安装包格式，而在你打不打包引擎**：deb 容器开销实测
    **≈190 字节**（空包 487 字节），MSI/NSIS/pkg 的空壳开销是几十 KB 量级，dmg 稍大；
    真正吃预算的是 Linux 的 WebKitGTK（24MB deb / 141MB 装后，**不打包、声明依赖**）、
    Windows 的 WebView2（**不打包，装期下 bootstrapper ≈2MB**）、macOS 的 **universal2 双架构**。
    **确定性：高（deb 实测）/ 低-中（其余为空壳量级推断）**。

## 证据

### A. Linux 桌面基线

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| Ubuntu 22.04（jammy）提供 `libwebkit2gtk-4.1-0` 2.50.4-0ubuntu0.22.04.1，**section = universe**，security/updates 均已发布（2026-01-13） | 22.04 可用 4.1，但**在 universe**（不是 main）：安装前提是启用了 universe | https://packages.ubuntu.com/jammy/libwebkit2gtk-4.1-0 ; https://launchpad.net/ubuntu/jammy/amd64/libwebkit2gtk-4.1-0 | 2.50.4-0ubuntu0.22.04.1，发布 2026-01-13 |
| Ubuntu 24.04（noble）提供 `libwebkit2gtk-4.1-0` 2.52.6-0ubuntu0.24.04.1 | 24.04 可用 4.1 | https://packages.ubuntu.com/noble/libwebkit2gtk-4.1-0 | 2.52.6-0ubuntu0.24.04.1 |
| Ubuntu 26.04 = 「Resolute Raccoon」，2026-04 发布；该系列 webkit2gtk 为 2.52.6-0ubuntu0.26.04.1（main，security/updates） | 26.04 可用 4.1，且进 main | https://wiki.ubuntu.com/Releases ; https://launchpad.net/ubuntu/+source/webkit2gtk | 26.04（2026-04），2.52.6-0ubuntu0.26.04.1 |
| Debian 12（bookworm）提供 `libwebkit2gtk-4.1-0` 2.50.6-1~deb12u2（main） | Debian 12 可用 4.1，无需 backports | https://packages.debian.org/bookworm/libwebkit2gtk-4.1-0 | 2.50.6-1~deb12u2 |
| Debian 13（trixie）提供 `libwebkit2gtk-4.1-0` 2.54.0-1~deb13u3（security） | Debian 13 可用 4.1 | https://packages.debian.org/trixie/libwebkit2gtk-4.1-0 | 2.54.0-1~deb13u3 |
| Debian 11（bullseye）**没有**该包：页面返回「Error Package not available in this suite.」 | bullseye 只有 4.0（且 2026-08-31 已结束 LTS） | https://packages.debian.org/bullseye/libwebkit2gtk-4.1-0 | 2026-11 查询 |
| Fedora Packages 收录 `webkit2gtk4.1`（描述「WebKitGTK for GTK 3 and libsoup 3」），可见于 Fedora 43、Fedora 44 | 当前 Fedora 两条支持线上都有 4.1 | https://packages.fedoraproject.org/search?query=webkit2gtk4.1 | Fedora 43 / 44，2026-11 |
| **RHEL 9 系只有 `webkit2gtk3`**：Rocky 9 AppStream 的 `w/` 目录里是 webkit2gtk3-2.52.3/2.52.4/2.52.5/2.54.0/2.54.1/2.54.2，**没有 webkit2gtk4.1** | RHEL 9 不满足「依赖 4.1」；要么放弃 RHEL 9，要么为 4.0 单独构建（推断：RHEL 9 与 Rocky 9 包集一致） | https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os/Packages/w/ | 2026-11（webkit2gtk3-2.54.2） |
| Arch `webkit2gtk-4.1` 2.54.2-1：包 38.4 MB / 装后 141.2 MB / 71 依赖（已在上一份笔记记录） | 再次印证「永不打包引擎」 | https://archlinux.org/packages/extra/x86_64/webkit2gtk-4.1/ | 2.54.2-1 |
| Rust 把 Linux 最低要求定为 glibc ≥ 2.17 / kernel ≥ 3.2（Rust 1.64，2022-09-22 起） | Rust 产物本身不构成限制 | https://blog.rust-lang.org/2022/08/01/Increasing-glibc-kernel-requirements/ | 2022-08-01，Rust 1.64 |
| Ubuntu 22.04 标准支持到 2027-05，ESM 到 2032-05；24.04 标准支持 2029-05、ESM 2034-05；26.04 于 2026-04 发布 | 定 22.04 为下限，支持窗口还有 6 年+ | https://ubuntu.com/about/release-cycle | 2026-11 页面 |
| Debian 12：发布 2023-06-10，安全支持 2026-07-11 结束，**LTS 到 2028-06-30**；Debian 13：2025-08-09 发布，安全支持到 2028-08-09、LTS 2030-06-30；Debian 11 LTS 到 2026-08-31 | 12 已经只剩 LTS；13 是当前稳定版 | https://www.debian.org/releases/ | 2026-11 页面 |

**发行版基线速查（2026-11，是否具备 4.1）**

| 发行版 | 4.1 包 | 说明 |
| --- | --- | --- |
| Ubuntu 22.04 LTS | 有（universe，2.50.4） | 最低可用下限之一，注意 universe |
| Ubuntu 24.04 LTS | 有（2.52.6） | 推荐基线 |
| Ubuntu 26.04 LTS | 有（main，2.52.6） | 2026-04 起 |
| Debian 12 | 有（2.50.6） | 2026-07 起仅 LTS |
| Debian 13 | 有（2.54.0） | 当前 stable |
| Debian 11 | **无** | 已 EOL（2026-08-31） |
| Fedora 43 / 44 | 有（webkit2gtk4.1） | Fedora 40–42 已在 2026-11 前 EOL（推断：约 13 个月窗口） |
| RHEL 9 | **无**（只有 webkit2gtk3） | 关键限制 |
| RHEL 10 | 未确认 | 镜像目录抓取被截断，未证实 |
| Arch | 有（2.54.2） | rolling |

### B. Windows

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 「Windows 10 will reach end of support on October 14, 2025. The current version, 22H2, will be the final version of Windows 10」 | Win10 已 EOL；最终版本 22H2 | https://learn.microsoft.com/en-us/lifecycle/products/windows-10-home-and-pro | 2026-11 页面 |
| Windows 11 当前版本（2026-11）：26H2（GA 2026-09-29，Home/Pro 支持到 2028-10-10、Enterprise 2029-10-09）、26H1（2026-02-10）、25H2（2025-09-30，Home/Pro 2027-10-12）、24H2（Home/Pro 已 End of updates，Enterprise 2027-10-12）、23H2（全部 End of updates） | 若定「Windows 11 24H2+」，消费级只剩 25H2/26H1/26H2 可选；24H2 的 Home/Pro 已停更 | https://learn.microsoft.com/en-us/windows/release-health/windows11-release-information | 2026-11-10 数据 |
| MSIX 能力表：`Windows services in MSIX ✅ (requires Windows 10 version 2004+)`；同表另有 `Package with external location (sparse packages)`、`Hosted Apps` 均需 2004+ | MSIX 支持装服务，但门槛是 Win10 2004+ | https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/msix-windows10-windows11 | 2026-11 页面 |
| `desktop6:Service`：「Specifies a service that is installed and registered along with the app. These services can be configured to run under either the Local Service, Network Service or Local System account.」`StartupType` = auto/manual/disabled | MSIX 包能声明并注册 Windows 服务，可跑 LocalSystem | https://learn.microsoft.com/en-us/uwp/schemas/appxpackage/uapmanifestschema/element-desktop6-service | 2026-11 页面 |
| CA/B Code Signing BR：`Effective June 1, 2023 … the Subscriber's Private Key is generated, stored, and used in a suitable Hardware Crypto Module` | 私钥必须在硬件模块/云 HSM 内 | https://cabforum.org/working-groups/code-signing/requirements/ | 2023-06-01 生效 |
| 同规范：`For Code Signing Certificates issued on or after March 1st, 2026, the validity period MUST NOT exceed 460 days`；`Effective September 15, 2026, a Certificate issued to a Subscriber MUST contain exactly one of the reserved policy OID` | 2026 起证书 ≤460 天、必须带保留 OID | 同上 | 2026-03-01 / 2026-09-15 |
| SmartScreen 是**信誉制**：「It also provides reputation checks for apps … If there's no reputation, the item is marked as a higher risk and presents a warning to the user.」 | 新证书没有信誉 = 仍会弹警告；签名不等于免警告 | https://learn.microsoft.com/en-us/windows/security/operating-system-security/virus-and-threat-protection/microsoft-defender-smartscreen/ | 2026-11 页面 |
| WebView2 分发文档把「检测是否已装 → 缺失则运行 bootstrapper」作为官方兜底；Bootstrapper ≈2 MB、Fixed Version >250 MB（上一份笔记已记） | 装期下载量以 bootstrapper 为界，但**Runtime 本体体积文档未给** | https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution | ms.date 2024-06-27 |

### C. macOS

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 公证接受的可交付物：「macOS apps / Non-app bundles … / Disk images (UDIF format) / Flat installer packages」 | pkg 与 dmg 都是第一类公证对象 | https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution | 文档站 2026 |
| 「It processes nested containers as well, like packages inside a disk image.」；「If you distribute your software via a custom third-party installer, you need two rounds of notarization.」 | dmg 套 pkg 只需对 dmg 提一次（嵌套会被处理）；**自研安装器**才需要两轮 | https://developer.apple.com/documentation/security/customizing-the-notarization-workflow | 文档站 2026 |
| 「To attach a ticket to your app, bundle, disk image, or flat installer package, use the stapler tool」；「While you can notarize a ZIP archive, you can't staple to it directly」 | pkg 与 dmg **都要**打票；ZIP 只能逐项 staple | 同上 | 文档站 2026 |
| 公证前置条件：Developer ID 证书（app / installer）、Hardened Runtime、安全时间戳、不得带 `com.apple.security.get-task-allow`、链接 macOS 10.9+ SDK | 四件套缺一不可 | https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution | 文档站 2026 |
| 插件相关：「In macOS 10.15 and later, apps can load quarantined plug-ins … only if the plug-in is notarized」；「Plug-ins don't declare their own entitlements. Instead, they inherit the entitlements of the host process.」 | 宿主必须替插件声明所需 entitlement | 同上 | 文档站 2026 |
| Library Validation：阻止加载「not signed by Apple or signed with the same Team ID as the main executable」的库；放开需 `com.apple.security.cs.disable-library-validation`，且「Gatekeeper runs extra security checks on programs that have it disabled」 | **只影响 dlopen 第三方 dylib**；spawn 独立进程不触发 | https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.disable-library-validation | 文档站 2026 |
| `SMAppService`：「In macOS 13 and later, use SMAppService to register and control LoginItems, LaunchAgents, and LaunchDaemons」；「For SMAppServices initialized as LaunchDaemons, the … methods provide a replacement for installing property lists in /Library/LaunchDaemons」 | macOS 13+ 的官方 LaunchDaemon 注册路径；仍可走 pkg 装 plist | https://developer.apple.com/documentation/servicemanagement/smappservice | 文档站 2026 |
| `aarch64-apple-darwin` = 「ARM64 macOS (11.0+, Big Sur+)」 | arm64 最低部署目标 11.0 | https://doc.rust-lang.org/nightly/rustc/platform-support.html | 2026-11 nightly 页 |
| Apple Developer Program「$99 annual membership」 | 公证/签名的固定年费 | https://developer.apple.com/programs/ | 2026-11 页面 |
| apple.com 当前 macOS 页面标题为「macOS 27 Golden Gate」 | 2026-11 当前版本 = macOS 27 | https://www.apple.com/macos/ | 2026-11 |

### D. 体积基线

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| **本机实测**：手工构造的最小 `.deb`（debian-binary + 203 B control.tar.gz + 92 B data.tar.gz）总长 **487 字节**；ar 头部 3×60 B + 8 B magic ⇒ 容器开销 **≈190 字节** | deb 的容器开销可忽略，包大小 ≈ 载荷 | 本机 Python 构造（ar + tar.gz），2026-11-12 | 实测 |
| Debian trixie `libwebkit2gtk-4.1-0` 单包 25,210,160 字节（24.0 MB）（上一份笔记） | Linux 上「把引擎当依赖」是唯一可行解 | https://packages.debian.org/trixie/amd64/libwebkit2gtk-4.1-0/download | 2.54.0-1~deb13u3 |
| Rocky 9 的 `webkit2gtk3-2.52.3-1.el9_8.x86_64.rpm` = **30,010,235 字节**；`i686` = 29,234,465 字节 | rpm 侧引擎包同为 30 MB 量级 | https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os/Packages/w/ | 2026-11 目录列表 |
| WebView2：Bootstrapper ≈2 MB（在线）、Fixed Version >250 MB；Evergreen Runtime 安装后体积文档未给 | Windows 装期下载量 ≈ 2 MB 起，实际 Runtime 另计 | https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution | ms.date 2024-06-27 |

**空壳安装包开销（量级，除 deb 外均为推断）**

| 格式 | 空壳量级 | 依据 |
| --- | --- | --- |
| `.deb` | **≈190 字节容器**（实测 487 B 全包） | 本机实测 |
| `.rpm` | 约 5–15 KB（lead + header + cpio 尾） | 推断（未实测） |
| `pkg`（xar） | 约 5–20 KB | 推断 |
| `dmg`（UDIF） | 数百 KB 到数 MB（文件系统/分区元数据为主） | 推断 |
| MSI（WiX） | 约 20–60 KB（OLE 复合文档 + 必备表） | 推断（无 WiX，无法实测） |
| NSIS | 约 35–60 KB（stub + LZMA 头） | 推断 |

### E. 最低系统版本可用的支持窗口（2026-11）

| 平台 | 事实 | 对「最低版本」的含义 | 来源 |
| --- | --- | --- | --- |
| Windows 10 | 2025-10-14 结束支持，22H2 为最终版本；其后只有 ESU | 定 Win10 等于定一个已 EOL 的系统，需 ESU 兜底 | https://learn.microsoft.com/en-us/lifecycle/products/windows-10-home-and-pro |
| Windows 11 | 26H2 / 26H1 / 25H2 在支持中；24H2 的 Home/Pro 已 End of updates（Enterprise 到 2027-10-12）；23H2 全部停更 | 建议最低 **Windows 11 24H2（Enterprise）/ 25H2（消费级）** | https://learn.microsoft.com/en-us/windows/release-health/windows11-release-information |
| macOS | 当前 macOS 27（Golden Gate）；Apple 惯例是最近三个大版本拿安全更新 | 建议最低 **macOS 14 或 15**，工程下限可到 11.0（arm64 部署目标） | https://www.apple.com/macos/ （窗口为推断） |
| Ubuntu | 22.04 到 2027-05（ESM 2032）、24.04 到 2029-05（ESM 2034）、26.04 2026-04 起 | 最低 **22.04** | https://ubuntu.com/about/release-cycle |
| Debian | 12 仅 LTS（到 2028-06）、13 到 2028-08（LTS 2030-06） | 最低 **12** | https://www.debian.org/releases/ |
| Fedora / RHEL | Fedora 43/44 在支持中；RHEL 9 **无 4.1**、RHEL 10 未确认 | 不支持 RHEL 9 | 见 A 表 |

## 对选型的影响（直接给建议，标注确定性）

### 1. 最低系统版本（建议值）

| 平台 | 建议最低版本 | 理由 | 确定性 |
| --- | --- | --- | --- |
| Linux | Ubuntu 22.04 / Debian 12 / Fedora 43 / Arch | 4.1 在这些版本上「装得上」；更老的没有 | 高 |
| Windows | Windows 11 23H2+（实际可跑 24H2+） | Win10 已 EOL；WebView2 只是必要条件不是充分条件 | 中 |
| macOS | macOS 13（SMAppService 可用）/ 部署目标 11.0 | 11.0 是 arm64 下限，13 才有 SMAppService | 中高 |

**明确不要**：把 RHEL 9 写进支持矩阵（没有 4.1）；把 Debian 11 / Ubuntu 20.04 写进去（已 EOL 或无包）。

### 2. Linux：gnu target + 发行版依赖，没有第二条路

- 构建目标：`x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`。**musl 静态不可行**
  （无静态 WebKitGTK、glibc DSO 不能进 musl 进程）。**确定性：高（结论）**。
- 打包：deb 的 `Depends: libwebkit2gtk-4.1-0 (>= 2.40)`，rpm 的 `Requires: webkit2gtk4.1`。
- 安装脚本：postinst 用 `addgroup --system` / `adduser --system --no-create-home`（rpm 用
  `groupadd -r` / `useradd -r`）；unit 装到 `/usr/lib/systemd/system/svcd.service` 并由
  `systemctl daemon-reload` + `enable --now` 接管。**确定性：中高（常规做法，未逐条引政策文档）**。
- 签名：**自建 apt 仓库必须签**（apt 认签名的 Release/InRelease，见 Debian SecureApt）；
  直接发单个 `.deb` 给用户 `dpkg -i` 则没有强制签名检查。rpm 侧 `dnf` 默认 `gpgcheck=1`，
  自建仓库要签包。**确定性：中高**。

### 3. Windows：MSI（WiX）+ 独立 bootstrapper 最稳，MSIX 作为「以后再说」

- **每机器安装**：MSI 用 `ALLUSERS=1` + `ProgramFiles64Folder`；服务用 WiX 的
  `ServiceInstall`/`ServiceControl`（这是 WiX 文档里的既有元素）。**确定性：中**
  （元素存在，但我没抓到可引用的文档页，见未确认项）。
- **WebView2 前置**：官方做法就是「检测注册表 `pv` → 缺失则静默跑
  `MicrosoftEdgeWebView2RuntimeInstaller{X64/ARM64}.exe /silent /install`」；把它塞进 MSI 需要
  一个引导器（Burn bundle / NSIS / custom action）。**「用 Burn 包 MSI+ExePackage」是 WiX 常规
  做法，但本次没抓到权威页面 → 推断**。
- **MSI 建本地组**：MSI 没有原生「建组」动作（WiX `util:Group` 是扩展里的 custom action）。
  **命名管道 ACL 不是安装期对象**，只能由 svcd 启动时按 SDDL 设置。**确定性：中（推断）**。
- **MSIX**：能装服务（Win10 2004+），但要包身份、且安装模型偏向按用户；对「系统级守护进程 +
  共享插件目录」不如 MSI 直接。**若未来要上 Store/App Installer，再评估**。**确定性：中**。

### 4. macOS：只发 `.pkg`（可包在 dmg 里），签名→打包→公证→staple 一条链

流程（确定性：高，均出自 Apple 文档）：

1. `codesign` 签每个可执行文件与 .app（Developer ID Application + `--options runtime` + 时间戳）；
2. `pkgbuild`/`productbuild` 出 flat pkg（安装脚本以 root 跑，装 LaunchDaemon plist 到
   `/Library/LaunchDaemons`，装 .app 到 `/Applications`）；如要 dmg，再把 pkg 放进 dmg；
3. `xcrun notarytool submit <pkg|dmg> --wait`（Apple 明确 altool 自 2023-11-01 起不再接受上传）；
4. `xcrun stapler staple <pkg|dmg>` 把票钉上（ZIP 不行，要对里面的每一项 staple）。

- **entitlements**：spawn 自己的 multicall 子进程 → **不需要**额外 entitlement（Hardened Runtime
  不限制 fork/exec）。**dlopen 第三方 .spk/.dylib → 需要
  `com.apple.security.cs.disable-library-validation`**（否则只允许 Apple 或同 Team ID 签的库），
  而且"插件继承宿主 entitlements"，宿主必须替插件声明它需要的能力。JIT：WKWebView 的 JS 引擎在
  系统进程里，**不需要** `allow-jit`；**只有当插件自己带 JS 引擎（如 Node/V8）才需要**。
  **确定性：中高（entitlement 语义高、具体插件形态未实测）**。
- **.app 最小结构**：`Contents/Info.plist`、`Contents/MacOS/<multicall>`、
  `Contents/Resources/`、`Contents/Library/LaunchDaemons/<label>.plist`（走 SMAppService 时）。
  `Info.plist` 需要 `NSAllowsLocalNetworking`（见上一份笔记的 ATS 结论）。
  **确定性：中（目录约定为社区/文档惯例，SMAppService 要求 helper 在 bundle 内已由文档写明）**。
- **universal2**：等于两份切片，压缩后通常小于两倍但仍是最大单项。**30MB 预算下，先决定要不要
  universal2**——只要 arm64 就省一半。**确定性：中（体积倍数为推断）**。

### 5. 体积：30MB 预算怎么花（组合建议）

| 平台 | 交付组合 | 估算（推断） | 备注 |
| --- | --- | --- | --- |
| Linux | 单一 multicall（gnu，strip+LTO）+ deb/rpm | 10–20 MB | 引擎不打包；WebKitGTK 由用户装 |
| Windows | 同一 binary（MSVC）+ MSI + 2 MB bootstrapper | 8–15 MB | Runtime 装期下载 |
| macOS | universal2 binary + pkg（内含 .app + LaunchDaemon） | 18–30 MB | 只 arm64 可降到 10–18 MB |

**结论：30MB 对 Linux/Windows 宽松，对 macOS universal2 紧张**。若要守 30MB，
**macOS 先砍 universal2**（或按架构分两个 pkg），这比砍资源更有效。**确定性：中**。

### 6. 成本清单（一次性 + 每年）

| 项目 | 量级 | 依据 |
| --- | --- | --- |
| Apple Developer Program | **$99/年** | developer.apple.com/programs |
| 代码签名证书（OV/EV）+ 硬件令牌或云 HSM | 证书年费 + 令牌/HSM 订阅；**私钥必须在硬件模块内** | CA/B CSBR 2023-06-01 |
| 证书有效期 | **≤460 天**（2026-03-01 起签发）⇒ 每年至少换一次 | CA/B CSBR |
| 公证 | 免费（含在 $99 内），但每次提交有等待时间 | Apple 文档（未逐条核「免费」措辞） |

## 未确认项

1. **Windows 10 消费级 ESU 的准确截止日**。微软 ESU 页面本次抓取拿不到正文（learn 页面正文里
   没有日期表、microsoft.com 页面返回空壳）。业界口径是「消费级 1 年、到 2026-10-13；商业
   3 年、到 2028-10」，但**本次没有可引用的官方页面**。
2. **RHEL 10 是否有 `webkit2gtk4.1`**。Rocky 10 的 `AppStream/.../w/` 目录抓取被截断（列表只到
   WALinuxAgent），未证实；RHEL 9 的「无 4.1」也只核到 Rocky 9 镜像（下游重建，推断等同）。
3. **WebView2 Runtime 支持的最低 Windows 10 版本**。`distribution` 页只写「Windows 10 devices」，
   没给 build 号；上一份笔记里「Runtime 109 是最后支持 Win7/8.1 的版本」是间接证据。
4. **WebView2 Evergreen Runtime 安装后体积 / Standalone Installer 下载体积**。微软只给
   「>250 MB（Fixed）」「≈2 MB（Bootstrapper）」，没给 Evergreen 的体积。
5. **MSI / NSIS / pkg / dmg 的空壳体积没有实测**。本机无 WiX/NSIS/macOS，且沙箱内
   `Invoke-WebRequest` 的 TLS 被拦（无法取工具链）。表中量级都是按格式结构推断，
   建议在 CI 里用 1 个空工程实测一次并回填。
6. **WiX `ServiceInstall`/`ServiceControl` 与 Burn 的权威文档页**。Docs 站点路径在 2026-11 变了
   （`docs.firegiant.com/wix/schema/wix/serviceinstall` 返回 404），只从搜索索引看到
   「ServiceInstall Element」「How To: Install a Windows service」两个标题，未抓到正文。
7. **MSIX 的「按用户 vs 每机器」安装模型对新版 Windows 的细节**（provisioning、
   `desktop6:Service` 是否要求包已 provision 给所有用户）。只核到「能装服务 + 需 Win10 2004+」。
8. **macOS 支持窗口的官方措辞**。Apple 没有可直接引用的「最近三个大版本」政策页；N/N-1/N-2 是
   按历年安全更新覆盖范围归纳的惯例，未确认 Apple 官方文本。
9. **公证票的体积**。Apple 文档只说「ticket」，没给字节数；推断是 KB 量级（对 30MB 预算无影响），
   但未实测。
10. **Ubuntu 22.04 首次提供 4.1 的时间**。Launchpad 只翻到 2026-01 的发布记录，无法确定
    22.04 是「发布即有」还是「后续 SRU 加入 universe」；这会影响「老 22.04 镜像是否开箱可用」。
11. **deb/rpm 的 postinst 具体写法与 systemd 宏**（`dh_installsystemd`、`%systemd_post`）本次未
    引政策文档，属常规做法；写打包脚本前应回到 Debian Policy / Fedora Packaging Guidelines 核一遍。
12. **EV 证书是否仍能「立刻获得 SmartScreen 信誉」**。微软现行文档只讲信誉制，没写 EV 例外；
    「EV 免警告」是旧说法，**本次未证实也未证伪**。
