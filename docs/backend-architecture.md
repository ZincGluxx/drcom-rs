# 后端架构：core / service / 前端

本文说明这次拆分后各 crate 的职责、依赖方向、数据流与扩展方式。前端接入的操作细节见
[frontend-guide.md](frontend-guide.md)；非 Rust 进程使用的线协议见
[ipc-protocol.md](ipc-protocol.md)。契约本身以
[crates/drcom-service/src/api.rs](../crates/drcom-service/src/api.rs) 为准，本文与前端指南
是它的说明，不另立一套。

## 拆分目标

- **认证与系统能力只实现一次。** 协议、会话、网卡选择、凭据存储这些逻辑不再和 Slint
  绑定，GUI、CLI、daemon 与测试共用同一份实现。
- **应用层只实现一次。** “现在是在连接、重连、等网卡，还是失败了、失败原因是什么”这类
  判断集中到 `drcom-service`，换一个前端不需要重写状态机。
- **前端只做前端的事。** 窗口、托盘、剪贴板、文件对话框、事件循环归各前端自己；
  后端不知道窗口是否存在。
- **契约固定，前端可并行。** `api.rs` 定义的命令、快照与通知同时用于进程内调用和
  JSON-lines IPC，前端可以各写各的，互不阻塞。

## 分层

依赖方向自下而上：箭头表示“被谁使用/依赖”。

```text
进程内 Rust 前端                              非 Rust 前端 / 其他进程
┌──────────────────────────────┐            ┌──────────────────────────────┐
│ crates/drcom-gui             │            │ 任意语言（Python / C# / …）   │
│ crates/drcom-cli             │            └───────────────┬──────────────┘
└──────┬───────────────────────┘                            │ JSON lines
       │ Command / Snapshot / Notification                  │（stdio 或回环 TCP）
       ▼                                                    ▼
┌──────────────────────────────┐            ┌──────────────────────────────┐
│ crates/drcom-service         │◀───────────│ crates/drcom-daemon          │
│ 应用状态机（无 UI 依赖）       │            │ + crates/drcom-ipc 协议实现   │
└──────┬───────────────────────┘            └──────────────────────────────┘
       │ 调用
       ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ crates/drcom-core                                                        │
│ 协议 / 认证 / 会话 / 保活 / 网卡与路由 / 配置 / 凭据 / 探针 / 静态 IPv4     │
└──────────────────────────────────────────────────────────────────────────┘
```

`drcom-cli` 是唯一不使用 `drcom-service` 的前端，直接依赖 `drcom-core`：脚本需要的是
`SessionEvent` 这种逐阶段的原始事件流（挑战、保活序号与尾码），而不是聚合后的展示快照。

## crate 职责

| crate | 职责 | 依赖 | 产物 |
| --- | --- | --- | --- |
| `drcom-core` | 协议与认证：`protocol`、`auth`、`reference_login`、`md5`、`transport`、`keepalive`、`session`；系统能力：`adapter`、`route`、`config`、`preferences`（DPAPI 偏好 + 开机自启）、`event_log`（有上限的内存诊断摘要）、`connectivity`（校内 Ping / 公网 HTTPS 探针）、`static_ipv4`（校验 + 提权 `netsh`）、`page_status`（只读网页状态） | `serde`、`serde_json`、`base64`、`socket2`、`windows-sys` | 无（库） |
| `drcom-service` | 前端无关的应用层：`Command` / `Snapshot` / `Notification` 契约；会话启停、重连退避与准入、待执行的自动登录、探针调度、失败卡片、诊断文本、静态 IPv4 编辑器状态、偏好保存与清除 | `drcom-core`、`serde` | 无（库） |
| `drcom-ipc` | JSON-lines 协议：行帧、请求/响应信封、通知广播、阻塞客户端与服务循环 | `drcom-service`、`serde`、`serde_json` | 无（库） |
| `drcom-daemon` | 无界面后端宿主：创建 `Service`、每秒 `tick()`、通过 stdio 或回环地址的 TCP 提供 `drcom-ipc` | `drcom-service`、`drcom-ipc`、`serde_json` | `target/release/drcom-daemon.exe` |
| `drcom-gui` | Slint 桌面前端：窗口与托盘、单实例保护、剪贴板/导出/打开系统网络设置；把快照映射到控件、把回调映射成命令 | `drcom-service`、`drcom-core`（只用于只读数据目录/事件摘要）、`slint`、`image`、`clipboard-win` | `target/release/drcom-campus.exe` |
| `drcom-cli` | 命令行前端：参数与 INI 解析，直接驱动 `drcom-core` 的 `LoginSession`，打印 `SessionEvent` | `drcom-core` | `target/release/drcom-cli.exe` |

## 依赖规则

1. 依赖只能向上：`drcom-core` ← `drcom-service` ← `{drcom-gui, drcom-ipc, drcom-daemon}`；
   `drcom-cli` 直接依赖 `drcom-core`。`drcom-core` 不依赖任何上层 crate。
2. `drcom-core` 不得依赖 UI/窗口/托盘/剪贴板 crate，也不得依赖 `drcom-service`。新的桌面
   集成能力属于前端，不要塞进 core。
3. `drcom-service` 不得依赖 Slint 或任何前端 crate；除 core 与 serde 外不引入其他依赖。
4. 前端只能通过契约操作后端：读 `Snapshot`，发 `Command`，处理 `Notification`。不要绕过
   服务直接驱动 `LoginSession`（CLI 是上面说明的例外），也不要把服务状态当可写。
5. 新增依赖前先看方向：出现 `core → service`、`service → gui`、`前端 ↔ 前端` 这类依赖，
   设计就不对。`cargo tree -p drcom-service` 可以核对。
6. workspace 成员是 `crates/*`；在 `crates/` 下新建 crate 会被自动纳入 workspace，版本与
   edition 从 `[workspace.package]` 继承。

## 数据流

```text
用户输入（连接 / 下线 / 检测 / 保存设置 / …）
        │ 映射
        ▼
Service::execute(Command) ──► 校验与准入（是否已有会话、重连退避、失败卡片）
        │                              │
        │                              ├─► 会话线程：LoginSession::run + SessionEvent
        │                              ├─► 探针线程：connectivity / page_status
        │                              └─► 网络线程：static_ipv4 + 提权 netsh
        │
        ├─ 立即返回 CommandOutcome（Snapshot，诊断文本除外）
        └─ 每次状态变化 / tick ──► Notification::State { Snapshot } ──► 前端整体重绘
```

启动阶段：

- `Service::new(ServiceOptions)` 读取 `drcom.ini`、DPAPI 偏好与开机自启状态。
- `ServiceOptions.startup_probe` 决定是否在启动时做一次只读网页状态探测；
  `ServiceOptions.auto_login` 决定是否恢复保存的自动登录。测试与诊断工具用
  `ServiceOptions::for_test()`（两者都关、不依赖磁盘上的 `drcom.ini`），daemon 对应
  `--no-startup-probe` / `--no-auto-login`。
- 前端订阅后先渲染一次 `Service::snapshot()`，之后收到 `Notification::State` 就整份覆盖。
  登录表单另用 `Service::form_seed()` 初始化（见下文“凭据边界”）。

命令阶段：

1. 前端把用户输入映射成 `Command`，调用 `Service::execute`。
2. 服务做校验与准入，必要时把耗时操作放到自己的线程；`execute` 不会等认证完成。
3. 结果通过两条路径回到前端：`Command::DiagnosticsText` 直接返回
   `CommandOutcome::Text`，其余命令返回 `CommandOutcome::Snapshot`，同时向所有订阅者
   推送 `Notification::State`。
4. 前端用最新快照整体重绘。快照是自包含的，不需要合并增量，也不需要记住旧值。

定时阶段：前端约每秒调用一次 `Service::tick()`。在线时长刷新、登录成功后自动收进托盘的
计时、待执行的自动登录都由 tick 推进；服务内部没有隐藏的定时器。

退出阶段：前端发 `Command::Shutdown`，后端在需要时先发下线报文，然后推送
`Notification::ExitReady`。前端收到后才结束事件循环或退出进程。

## 线程与顺序

- `Service` 可克隆，所有方法都可以从任意线程调用；共享状态由服务内部加锁，调用方不需要
  自己串行化。
- `execute` 只在必要范围内阻塞（网卡枚举、偏好写盘这类短操作）。认证、探针、`netsh` 都在
  后台线程执行，不要用短超时假设“登录已经完成”，要以状态通知为准。
- 订阅回调（`Sink`）在**导致这次变化的那条线程**上同步调用，可能来自会话线程、探针线程或
  tick 调用线程。回调必须立即返回；要碰 UI 先切到自己的 UI 线程（GUI 用
  `slint::invoke_from_event_loop`，daemon 转成 IPC 广播，其他前端用自己的消息队列）。
- 允许注册多个 `Sink`（例如 daemon 向每个客户端广播）；`Sink` 的类型是
  `Arc<dyn Fn(Notification) + Send + Sync>`。
- 快照始终完整，跨线程的到达顺序不做保证。前端按到达顺序覆盖即可，最后一份为准；任何
  时刻都可以调用 `snapshot()` 重新对齐，不需要重放历史。
- 会话线程的创建、停止与下线路由服务管理，前端不持有线程句柄；`Logout` 只是发一条命令。
- 不要在前端直接 `std::process::exit` 退出：那会跳过下线报文。走 `Shutdown` → `ExitReady`
  的正常路径。

## 凭据边界

- **密码永不进入 `Snapshot`。** 快照是 IPC 客户端可以读到的数据，里面只有已保存账号的
  显示值（`SettingsView.username`），没有任何密码。
- `Service::form_seed()` 返回的 `FormSeed` 是密码唯一一次交给前端的地方，用于初始化登录
  表单。`FormSeed` 不实现 `Serialize`，只在进程内使用，也不要写日志。
- `Command::Login`、`Command::SaveSettings`、`Command::ClearSavedPassword` 会携带密码（或
  处理密码）。走 IPC 时凭据经过管道或回环 socket，因此 `drcom-daemon` 只允许 stdio 或
  回环 TCP 监听，且没有 TLS/鉴权，不能暴露到校园网/公网。详见
  [ipc-protocol.md](ipc-protocol.md)。
- 记住的凭据由 Windows DPAPI 加密保存在 `%LOCALAPPDATA%\DrComCampusRust\preferences.dat`；
  `drcom.ini` 是明文配置，把密码写进去就是明文落盘，推荐留空、由界面输入。CLI 提供
  `DRCOM_PASSWORD` 环境变量，避免 `--pass` 进入命令行历史。
- 诊断摘要（`event_log`）与 `Command::DiagnosticsText` 生成的报告都不含账号密码；前端复制
  或导出时不要自行拼接表单里的密码。
- 前端自己也不得持久化密码：表单内容只在内存中，随 `Login` / `SaveSettings` 交给后端。

## 工作区目录

```text
drcom-rs/
├── Cargo.toml                 # workspace 根清单；版本在 [workspace.package]
├── Cargo.lock
├── setup.iss                  # Inno Setup 安装脚本（版本号需单独同步）
├── drcom.ini.example
├── README.md
├── docs/
│   ├── backend-architecture.md
│   ├── frontend-guide.md
│   ├── ipc-protocol.md
│   └── …（截图、发布说明、占用报告）
└── crates/
    ├── drcom-core/
    │   ├── src/               # protocol / auth / session / adapter / config / preferences …
    │   ├── reference/         # 协议参考向量与 Python 校验脚本
    │   └── examples/          # adapter_probe
    ├── drcom-service/
    │   └── src/               # api.rs（契约）、service.rs（状态机）
    ├── drcom-ipc/
    ├── drcom-daemon/
    ├── drcom-gui/
    │   ├── src/               # main.rs、ui_backend.rs、platform.rs、single_instance.rs
    │   ├── ui/app.slint
    │   ├── assets/            # 图标（exe 内嵌 + 托盘/窗口）
    │   └── examples/          # open_probe / tray_floor / ui_preview / window_floor
    └── drcom-cli/
        └── src/main.rs
```

构建产物在 `target/`（已 git 忽略），不随仓库提交。

## 构建、测试与产物

环境：Rust + MSVC 工具链（edition 2024，要求 1.85+；本仓库在 1.98.1 MSVC 上验证）、
Windows SDK（`rc.exe`，GUI 的 `build.rs` 会自动定位）、Inno Setup 6（仅打包需要）。

```bash
# 构建全部产物
cargo build --release --workspace --offline

# 只构建某一个包（并行开发时用这个，避免动到别人的 crate）
cargo build --release --offline -p drcom-gui
cargo build --offline -p drcom-cli

# 测试：整个 workspace，或单个包
cargo test --workspace --offline
cargo test --offline -p drcom-service

# 静态检查
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check

# 安装包（产出 DrComRust_v<版本>_Setup.exe，包含三个 exe）
"C:/Program Files (x86)/Inno Setup 6/ISCC.exe" setup.iss
```

`--offline` 依赖 `Cargo.lock` 固定的版本已经缓存在本机；在一个全新的联网环境里首次构建
可以暂时去掉它，之后 CI/日常构建保持离线。

产物：

| 路径 | 内容 |
| --- | --- |
| `target/release/drcom-campus.exe` | Slint 图形界面 |
| `target/release/drcom-cli.exe` | 命令行前端 |
| `target/release/drcom-daemon.exe` | 无界面后端宿主 |

版本号只在一个地方定义：根 `Cargo.toml` 的 `[workspace.package] version`，各 crate 用
`version.workspace = true` 继承。`setup.iss` 的 `MyAppVersion` / `VersionInfoVersion`
必须独立同步；GUI exe 的文件版本资源来自 `CARGO_PKG_VERSION`
（`crates/drcom-gui/build.rs`），不需要手写。

## 旧模块 → 新位置

| 旧位置（单 crate） | 新位置 | 说明 |
| --- | --- | --- |
| `src/protocol.rs`、`auth.rs`、`md5.rs`、`login_response.rs`、`reference_login.rs`、`reference_vectors.rs`、`retry.rs`、`transport.rs` | `crates/drcom-core/src/` | 协议层与参考实现 |
| `src/session.rs`、`keepalive.rs` | `crates/drcom-core/src/` | 会话状态机与保活 |
| `src/adapter.rs`、`route.rs` | `crates/drcom-core/src/` | 网卡选择与认证路由 |
| `src/config.rs` | `crates/drcom-core/src/config.rs` | `drcom.ini` 解析与查找 |
| `src/preferences.rs` | `crates/drcom-core/src/preferences.rs` | DPAPI 偏好与开机自启 |
| `src/event_log.rs` | `crates/drcom-core/src/event_log.rs` | 有上限的内存诊断摘要，不落盘 |
| `src/page_status.rs` | `crates/drcom-core/src/page_status.rs` | 只读网页状态探测 |
| `src/diagnostics.rs` | `crates/drcom-core/src/connectivity.rs` + `crates/drcom-gui/src/platform.rs` | 校内 Ping / 公网 HTTPS 探针进 core；复制/导出/打开系统网络设置属于桌面集成，进 gui/platform；诊断报告文本的组装改由 service 的 `DiagnosticsText` 负责 |
| `src/static_ipv4.rs` | `crates/drcom-core/src/static_ipv4.rs` | 静态 IPv4 校验与 `netsh` 应用 |
| `src/ui_backend.rs` 的状态机部分 | `crates/drcom-service/src/service.rs` | 会话/重连/自动登录/失败卡片/偏好等应用逻辑（去掉 Slint 粘合） |
| `src/ui_backend.rs` 的窗口与托盘粘合 | `crates/drcom-gui/src/ui_backend.rs` | 只做快照 → 控件、回调 → 命令的映射 |
| `src/ui_backend.rs` 对复制/导出/打开网络设置的调用 | `crates/drcom-gui/src/platform.rs` | 桌面集成属于前端，服务只负责生成报告文本 |
| `src/single_instance.rs` | `crates/drcom-gui/src/single_instance.rs` | 单实例保护 |
| `src/main.rs` | `crates/drcom-gui/src/main.rs` | 窗口、托盘、事件循环、按需建窗 |
| `src/bin/drcom-cli.rs` | `crates/drcom-cli/src/main.rs` | 命令行前端，直连 core |
| `src/lib.rs` | 拆分 | `drcom-core` 与 `drcom-service` 各自的 `lib.rs`；旧 crate 名 `drcom_campus` 不再存在 |
| `ui/app.slint` | `crates/drcom-gui/ui/app.slint` | Slint 界面描述 |
| `assets/` | `crates/drcom-gui/assets/` | 图标资源 |
| `examples/adapter_probe.rs` | `crates/drcom-core/examples/adapter_probe.rs` | 网卡探测示例 |
| `examples/{open_probe,tray_floor,ui_preview,window_floor}.rs` | `crates/drcom-gui/examples/` | 窗口/托盘/界面预览示例 |
| `reference/` | `crates/drcom-core/reference/` | 协议参考向量与 Python 校验脚本 |
| （新增） | `crates/drcom-ipc/`、`crates/drcom-daemon/` | 非 Rust 前端的接入路径 |

## 新增前端

### (a) Rust 进程内：使用 `drcom-service`

在 `Cargo.toml` 里加路径依赖（外部项目按实际路径调整）：

```toml
[dependencies]
drcom-service = { path = "../drcom-rs/crates/drcom-service" }
```

最小循环：订阅 → 渲染 → 执行命令 → 每秒 `tick`，退出等 `ExitReady`。

```rust
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};

use drcom_service::{Command, Notification, Service, ServiceOptions, Snapshot};

fn main() {
    let service = Service::new(ServiceOptions::default());

    // 1. 订阅：sink 在后台线程上同步调用，这里只把通知转进自己的队列。
    let (sender, receiver) = mpsc::channel();
    service.subscribe(Arc::new(move |note| {
        let _ = sender.send(note);
    }));

    // 2. 只读一次表单种子；先整份渲染一次，别等第一个通知。
    let seed = service.form_seed();
    render(&service.snapshot());

    // 3. 用户点击“连接”：命令是异步的，结果从通知回来。
    service.execute(Command::Login {
        username: seed.username,
        password: seed.password.unwrap_or_default(),
        auto_reconnect: seed.auto_reconnect,
    });

    // 4. 前端自己持有 1 秒节拍：tick 才会推进在线时长与登录后自动隐藏。
    //    用户点“退出”时发 Command::Shutdown，收到 ExitReady 后 break。
    loop {
        match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Notification::State { snapshot }) => render(&snapshot),
            Ok(Notification::HideToTray { note }) => hide(&note),
            Ok(Notification::ExitReady) => break,
            Err(RecvTimeoutError::Timeout) => service.tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn render(_: &Snapshot) {}
fn hide(_: &str) {}
```

完整字段说明、命令表与检查清单见 [frontend-guide.md](frontend-guide.md)；现成的参考实现是
`crates/drcom-gui/src/ui_backend.rs`。

### (b) 任意语言：`drcom-daemon` + JSON-lines

`drcom-daemon` 在内部创建并持有 `Service`，自己按秒 `tick()`，把 `drcom-ipc` 的 JSON-lines
（stdio 或回环 TCP）转给其他进程。其他语言只需要：读初始快照 → 发命令 → 处理
`State` / `HideToTray` / `ExitReady` 通知。快照里的 `api_version` 用于判断协议兼容性。

请求/响应信封、行帧、通知格式、字段示例与安全约束都在
[ipc-protocol.md](ipc-protocol.md)，这里不重复。

## 版本与契约演进

- **版本号**：`[workspace.package] version` 是唯一来源，各 crate 继承；`setup.iss` 的
  `MyAppVersion` / `VersionInfoVersion` 独立维护，发版前必须两处一致。
- **`API_VERSION`**（`crates/drcom-service/src/api.rs`，当前为 `1`）只描述可序列化的前端
  契约。新增字段、命令或通知变体通常向后兼容；删除/改名/改变语义属于破坏性变更，必须
  递增版本，并让前端在启动时检查 `Snapshot.api_version`。`drcom-ipc` 另有自己的
  `PROTOCOL_VERSION`（信封与帧格式），两者独立演进，细节见
  [ipc-protocol.md](ipc-protocol.md)。
- **契约变更时要同步改**：
  - `api.rs` 的文档注释（字段含义、通知语义）；
  - 本文的“crate 职责”“数据流”“凭据边界”；
  - [frontend-guide.md](frontend-guide.md) 的字段映射表、命令表和检查清单；
  - [ipc-protocol.md](ipc-protocol.md) 的信封/schema（由 IPC 维护者负责）；
  - 需要时更新 README 的构建命令、产物列表与“复用后端”段落。
- 只改实现、不改契约时，前端不需要跟随改动；在发布说明里如实描述行为变化即可。

## 刻意不做

- 不提供 HTTP / WebSocket 服务端或 REST API；跨进程接入只有 daemon + JSON-lines。
- 不提供 C ABI / FFI 绑定 crate；其他语言走 IPC。
- 后端不拥有窗口、托盘、剪贴板、文件对话框、主题和快捷键，这些永远属于前端。
- 不把密码写进快照、IPC 消息、诊断摘要或日志。
