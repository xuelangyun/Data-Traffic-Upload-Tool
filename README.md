# WinNetStress (`win_net_stress`)

纯内存原生 Windows 客户端（Rust + Win32 API），用于测试本机在公网环境下的出口网络极限能力。

## 核心特性

1. **运行时纯内存（不读写业务数据文件）**
   - 测试配置、统计、日志和上传/下载数据缓冲均保存在进程内存；运行时不创建日志、临时文件或配置文件，不修改注册表。
   - 编译阶段 `build.rs` 会在 Cargo 构建目录生成清单和图标资源，这属于构建产物，不属于程序运行时 I/O。
   - 预生成 1 MiB 不可压缩随机内存载荷（xorshift64*）供多流复用；日志采用内存环形缓冲（最多保留 400 条）。
2. **原生 Win32 API 界面（零闪烁 + 高 DPI 自适应）**
   - 使用官方 `microsoft/windows-rs`（`windows = 0.100.0`，tag `74`）调用 Win32 API；支持无闪烁布局、差异刷新和 Per-Monitor DPI。仅采集 TCP/进程指标，不读取网卡收发字节。
3. **三大测试模块**
   - **公网 TCP 并发连接能力探测 (`ConnTester`)**：先执行并行预探测剔除不可达端口或无效 IPv6，再通过多公网 IP/端口四元组离散、信号量与 RTT 感知平滑建连、远端空闲断开自动补建，以及快速阶梯递增与“持续加压/维持连接”模式，实时统计存活会话、峰值并发、建连速率（conn/s）、握手延迟（ms）与分类错误。结果代表本机对所选公网目标的连接能力，不等同路由器 NAT 表容量。
   - **持续上行带宽压测 (`BandwidthUploader`)**：支持 HTTP/HTTPS POST 流式上传与 413 自适应请求体缩小；实时速率按最近约 1 秒请求体交给 HTTP 客户端的数据量计算，累计有效量只计完整请求体后收到 2xx 的请求，不将其称为逐字节服务端 ACK。
   - **持续下行带宽压测 (`BandwidthDownloader`)**：多流并发请求测速流，按最近约 1 秒实际收到的数据量计算实时速率；数据块只在内存中处理，并统计峰值和累计流量。

## Windows 程序图标

- 正式 EXE 使用 `build.rs` 生成的多分辨率网络测速图标（16/32/48/128 px），包含测速表盘、上行指针和网络地球元素。
- Windows MSVC 构建通过 Windows SDK `rc.exe` 将图标作为 PE 资源嵌入；若图标资源无法编译，构建会直接失败，不再静默退回默认图标。

## 软件截图

> 主界面真实运行截图待补充。请使用 Windows 实际运行程序后截图，并保存为 `docs/images/winnetstress-main.png`；收到截图素材后即可在此处嵌入，避免用示意图代替真实界面。

## WAN 出口公网 IP 自动识别

程序并行访问多个公网 IP 查询服务，显示服务端观察到的公网出口 IP。多出口分布表则按每条 TCP 连接的实际本地源 IP 分组：相同源 IP 的目标纵向合并到同一组，行数随当前活动连接动态增减。

本地源 IP 是电脑创建 TCP socket 时使用的地址，不等同于路由器物理 WAN 口或公网 NAT 地址。若路由器在电脑之外完成多 WAN 分流，电脑侧所有连接可能都使用同一个本地源 IP；此时仅凭本机数据无法准确区分物理 WAN。

## 测量口径说明

- 上行“近1秒”：HTTP Body 流交给 HTTP 客户端的数据速率；累计有效量仅计完整请求返回 2xx 的字节，不代表服务端逐字节 ACK。
- 下行“近1秒”：按实际读取到的响应数据计算；累计量为实际收到的字节。
- 多出口分组：按当前 TCP socket 本地源 IP 分组；它不能单独证明路由器物理 WAN 映射。
- 不采集网卡总字节/速率，避免将系统其他程序、虚拟网卡或 VPN 流量混入本工具测试结果。
- TCP 重传率：使用 Windows 系统级 IPv4 TCP 计数器的采样增量，不代表本进程专属重传率。

## 目录结构

- `src/main.rs`：程序入口（Release 下使用 `windows_subsystem = "windows"` 无黑框启动）。
- `src/state.rs`：纯内存状态机、`SharedStats` 原子计数、环形日志缓冲、国内公网默认节点列表。
- `src/sys_stats.rs`：TCP 重传率、系统 TCP 连接数、进程句柄和本地时钟采集；不读取网卡流量。
- `src/conn_tester.rs`：并发 TCP 连接极限探测引擎。
- `src/uploader.rs`：多流上行带宽压测引擎。
- `src/downloader.rs`：多流下行带宽压测引擎。
- `src/engine.rs`：Tokio 后台多线程运行时与 250ms 速率采样协调器。
- `src/ui.rs`：原生 Win32 GUI 与消息循环。

## 编译运行（Windows, Rust/Cargo ≥ 1.96）

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
