# WinNetStress (`win_net_stress`)

纯内存原生 Windows 客户端（Rust + Win32 API），用于测试本机在公网环境下的出口网络极限能力。

## 核心特性

1. **纯内存运行（零磁盘读写）**
   - 禁止 `std::fs`、不生成配置文件、不写日志文件、不写临时文件、不修改注册表。
   - 预生成 1 MiB 不可压缩随机内存载荷（xorshift64*）供多流复用；日志采用内存环形缓冲（最多保留 400 条）。
2. **原生 Win32 API 界面（零闪烁 + 高 DPI 自适应）**
   - 直接基于官方 `microsoft/windows-rs` 最新版（`windows = 0.100.0`，tag `74`）调用 Win32 窗口、编译期 Common Controls v6 清单、`WS_CLIPCHILDREN` + `BeginDeferWindowPos` 无闪烁布局、`LabelCache` 差异文本刷新、`PerMonitorV2` 动态字体缩放和 `iphlpapi` / `GetProcessHandleCount` 系统指标，无重型前端运行时。
3. **三大测试模块**
   - **最大并发 TCP 连接数探测 (`ConnTester`)**：参考路由器 WAN 口探测原理，先执行并行预探测剔除不可达端口或无效 IPv6，再通过多公网 IP/端口四元组离散、信号量与 RTT 感知平滑建连、远端空闲断开自动补建，以及快速阶梯递增与“持续加压/维持连接”模式，实时统计存活会话、峰值并发、建连速率（conn/s）、握手延迟（ms）与分类错误。
   - **持续上行带宽压测 (`BandwidthUploader`)**：支持 HTTP/HTTPS POST 流式上传（含 413 自适应分片）及 `tcp://ip:port` 原始 TCP 持续发送。
   - **持续下行带宽压测 (`BandwidthDownloader`)**：多流并发请求公网大文件/测速流，收到的数据块直接在内存中丢弃并统计实时/峰值 Mbps 与累计流量。

## 目录结构

- `src/main.rs`：程序入口（Release 下使用 `windows_subsystem = "windows"` 无黑框启动）。
- `src/state.rs`：纯内存状态机、`SharedStats` 原子计数、环形日志缓冲、国内公网默认节点列表。
- `src/sys_stats.rs`：Win32 系统指标与本地时钟采集（`GetTcpStatistics` / `GetProcessHandleCount` / `GetLocalTime`）。
- `src/conn_tester.rs`：并发 TCP 连接极限探测引擎。
- `src/uploader.rs`：多流上行带宽压测引擎。
- `src/downloader.rs`：多流下行带宽压测引擎。
- `src/engine.rs`：Tokio 后台多线程运行时与 250ms 速率采样协调器。
- `src/ui.rs`：原生 Win32 GUI 与消息循环。

## 编译运行（Windows, Cargo ≥ 1.96）

```powershell
# 开发检查与单元测试
cargo test

# 编译 Release 可执行文件
cargo build --release
```

产物位于 `target\release\win_net_stress.exe`。

## GitHub 自动打包与发布

仓库已配置两个 GitHub Actions 工作流：

1. **`.github/workflows/windows-ci.yml`（持续集成）**
   - 在推送至 `main` / `arena/*` 分支或提交 Pull Request 时自动在 `windows-latest` 上运行单元测试、编译 Release 二进制并上传构建产物（Artifact）。
2. **`.github/workflows/release.yml`（自动打包发布 Release）**
   - **Tag 触发**：推送以 `v` 开头的版本标签（例如 `git tag v0.1.0 && git push origin v0.1.0`）时，自动编译 `x86_64-pc-windows-msvc` Release 版本，打包 `.exe`、`.zip`（含 `README.md`）及 `SHA256SUMS.txt`，并创建 GitHub Release 自动附带 Release Notes 与下载附件。
   - **手动触发**：也可在 GitHub 仓库的 **Actions -> Build & Release Windows Binary -> Run workflow** 中手动指定标签名（留空则默认读取 `Cargo.toml` 版本号如 `v0.1.0`）与是否为预发布版本。

## 注意事项与合规声明

- **仅限合法用途**：本工具仅用于测试本机网络出口能力或已获明确授权的目标，请勿对未授权第三方服务器发起高频连接或持续大流量压测。
- **Windows 动态端口限制**：Windows 默认动态端口范围较小，若高并发连接测试中提示资源不足，可在管理员终端临时调整系统动态端口范围（本程序本身不修改注册表或系统配置）：
  ```cmd
  netsh int ipv4 set dynamicport tcp start=10000 num=55000
  ```
