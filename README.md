# 净流 · PureFlow

<!-- @author jvz -->
<!-- PureFlow watermark: jvz | Original author of PureFlow; see LICENSE. -->

面向 **macOS 菜单栏与 Windows 托盘**的本地流量监控应用，基于 Tauri 2、Vue 3、TypeScript 和 Rust，提供实时读数、整机与进程流量统计及本地历史查询。

安装包将通过本仓库的 **GitHub Releases** 发布，请选择对应平台的安装包或压缩包，并使用随附的 SHA-256 校验文件核对下载内容。

## 目录

- [核心功能](#核心功能)
- [平台与使用](#平台与使用)
- [本地开发](#本地开发)
- [打包](#打包)
- [隐私与限制](#隐私与限制)
- [许可证](#许可证)

## 核心功能

- **系统栏实时读数**：macOS 菜单栏双行显示上传/下载速度；Windows 托盘图标悬停提示显示双行速度。
- **整机与进程统计**：查看实时速度、流量累计和进程排行，支持进程名称搜索及排序。
- **历史查询**：按今日、昨日、最近 7 天或自定义时段查询，可按 SSID 筛选。
- **CSV 导出**：通过原生保存对话框选择位置，支持 UTF-8 BOM 与公式注入防护。
- **暂停与恢复**：支持暂停、继续和异常后的显式重试；收起弹层不会停止后台采集。
- **本地存储及设置**：SQLite 分钟记录保留 30 天，日汇总保留最近 365 个日期；提供登录启动、存储位置、缓存清理等设置。

只提供 GUI，**不提供独立 CLI**。Windows 的 `pureflow-collector.exe` 是 GUI 管理的内部采集器，不是用户操作入口。

## 平台与使用

| 目标平台 | 范围 |
| --- | --- |
| macOS | macOS 13+，Apple Silicon（arm64），使用系统 WebKit |
| Windows | Windows 10 / 11 x64，依赖 WebView2 Runtime |

以上是配置目标，不等于完整验证；目前不声明 Linux、Intel Mac 或 Windows ARM64 支持。

- **打开界面**：从菜单栏或托盘（可能在折叠区域）点击打开统计弹层，没有常驻普通主窗口；右键菜单提供暂停、重试、关于和退出等操作。
- **Windows 权限**：GUI 使用普通权限，内部采集器按需请求 UAC，仅支持同账号提升，不应手动运行采集器。
- **SSID 权限**：读取 Wi-Fi 名称可能需要位置等隐私授权；拒绝后仍可进行全网络统计，不记录位置坐标。
- **手动更新**：Windows 显式打开固定发布页；macOS 可显式查询发布 API，再打开发布页或对应资产。两端均不自动下载安装。

## 本地开发

### 环境准备

- Git、Node.js 22.19+ 或兼容 LTS、npm、rustup；Rust 版本及组件以 [rust-toolchain.toml](rust-toolchain.toml) 为准。
- macOS：Xcode Command Line Tools，使用原生 arm64 终端。
- Windows：MSVC C++ Build Tools、Windows SDK、WebView2，以及可执行已审查本地脚本的 PowerShell；受组织策略阻止时应停止并联系管理员。
- 按目标平台准备 Rust target：macOS 使用 `aarch64-apple-darwin`，Windows 使用 `x86_64-pc-windows-msvc`；Rust target 不能替代系统构建工具。

### 获取源码

在本仓库的 GitHub 页面点击 **Code → HTTPS** 复制克隆地址，替换下方的 `<GitHub仓库地址>`：

```sh
git clone <GitHub仓库地址>
cd pure-flow/apps/desktop
npm ci
```

以下 npm 和 helper 构建命令均在 **`apps/desktop`** 执行。原生 GUI/helper 的实际运行须满足目标设备授权要求；静态构建成功不代表原生功能或桌面交互验证通过。

### 浏览器布局预览

```sh
npm run dev
```

访问 <http://127.0.0.1:1420>。此入口仅预览布局，**不采集流量、不启动 helper**，依赖原生能力的控件不可用。

### macOS 原生开发

```sh
npm run tauri -- dev
```

此命令会构建并启动原生应用，从菜单栏打开弹层。

### Windows 原生开发

在普通权限 PowerShell 中先构建内部采集器，再使用专用开发入口：

```powershell
node scripts/build-helper.mjs
npm run dev:windows
```

不要直接使用 `npm run tauri -- dev`。专用入口管理开发进程并准备开发采集器；GUI 和终端均保持普通权限，结束时使用 Ctrl+C。端口固定为 `127.0.0.1:1420`，占用时失败，不自动换端口。

### 前端检查

```sh
npm run build
```

执行前端类型检查与 Vite 构建，输出到 `apps/desktop/dist/`；**不验证原生采集或桌面交互**。

## 打包

以下命令均从**仓库根目录**执行，必须在目标平台原生构建。`<version>` 由项目配置确定；脚本依赖默认 Cargo 输出目录 `target/`，不要设置自定义 `CARGO_TARGET_DIR`。

### macOS Apple Silicon

```sh
bash scripts/package-desktop-macos.sh
```

输出至根 `dist/`：`PureFlow-<version>-macos-arm64.zip` 及对应的 `.zip.sha256` 校验文件（SHA-256）。

### Windows x64

在普通权限 PowerShell 中执行：

```powershell
.\scripts\package-desktop-windows.ps1
```

输出至根 `dist/`：NSIS 安装包 `PureFlow_<version>_x64-setup.exe` 及对应的 `.exe.sha256` 校验文件（SHA-256）。

**Windows 打包会清空根目录 `dist/`。** 安装器采用系统级安装，需要管理员权限，并可能联网下载 WebView2 Runtime；打包脚本本身不运行安装器。

## 隐私与限制

- **本地数据**：统计保存在本地，无账号和云同步；显式更新操作、访问发布页及 WebView2 下载会联网。
- **分享前脱敏**：数据库与 CSV 可能包含 SSID、进程信息和时间记录。设置页可查看实际存储路径；缓存清理不等于删除历史。
- **持久化边界**：历史批量落盘，异常终止可能丢失尚未保存的数据；实时累计不代表全部已持久化。
- **统计边界**：进程流量可能受代理/VPN 重复计数、短时进程和事件延迟影响，不能相加当作整机账单；缺测不等于零流量。
- **待实机验收**：最低系统、多屏、休眠与网络切换、长期精度及 Windows 隐藏后 WebView 释放等仍待验证，不作性能或安全保证。

## 许可证

原作者 **jvz**（Developed by jvz），本项目原始代码采用 [MIT 许可证](LICENSE)。

**Copyright (c) 2026 jvz (PureFlow)**

第三方依赖及模板的版权归各自权利人所有，遵循各自许可证与必要声明。Windows [NSIS 模板](apps/desktop/src-tauri/windows/installer.nsi) 基于 Tauri 官方模板，保留 Tauri Apps Contributors 版权及其 [MIT 许可证](apps/desktop/src-tauri/windows/LICENSE-MIT)。
