# 前端接入指南：写一个 drcom-service 前端

本文面向“再写一个前端”的场景：Rust 进程内直接链接 `drcom-service`，或者通过
`drcom-daemon` 的 JSON-lines 从其他语言接入。契约以
[crates/drcom-service/src/api.rs](../crates/drcom-service/src/api.rs) 为准；分层与依赖
规则见 [backend-architecture.md](backend-architecture.md)；其他语言的线协议见
[ipc-protocol.md](ipc-protocol.md)。

现成的参考实现是 `crates/drcom-gui/src/ui_backend.rs`（Slint 前端，全部是“快照 → 控件 /
回调 → 命令”的映射，没有任何会话逻辑）。

## 一个前端要做的六件事

1. 构造 `Service`；
2. `subscribe` 订阅通知；
3. 用 `form_seed()` 初始化登录表单、用 `snapshot()` 渲染首次状态；
4. 把用户输入映射成 `Command` 调 `execute`；
5. 每秒调一次 `tick()`；
6. 退出时发 `Command::Shutdown`，收到 `Notification::ExitReady` 后再结束。

下面逐项展开。

## 生命周期

### 1. 构造

```rust
use drcom_service::{Service, ServiceOptions};

let service = Service::new(ServiceOptions::default());
```

`ServiceOptions` 控制启动副作用：

| 字段 | 含义 |
| --- | --- |
| `startup_probe` | 启动时是否对 Dr.COM 网页做一次只读探测 |
| `auto_login` | 是否按保存的偏好恢复自动登录 |
| `settings` | 覆盖自动发现的 `drcom.ini`；`None` 表示按正常顺序查找 |

生产前端用 `ServiceOptions::default()`；测试或诊断工具用
`ServiceOptions::for_test()`（不探测、不自动登录、不依赖磁盘配置）。daemon 对应
`--no-startup-probe` / `--no-auto-login`。

### 2. 订阅

```rust
use std::sync::Arc;
use drcom_service::Notification;

let (sender, receiver) = std::sync::mpsc::channel();
service.subscribe(Arc::new(move |note: Notification| {
    // 在后台线程上被同步调用，不要在这里做重活或直接碰 UI。
    let _ = sender.send(note);
}));
```

- `Sink` 的类型是 `Arc<dyn Fn(Notification) + Send + Sync>`，可以订阅多个（daemon 用这个
  向多个 IPC 客户端广播）。
- 回调在**导致变化的那条线程**上调用：会话线程、探针线程，或你调用 `tick()` 的线程。
  要更新界面时先切到自己的 UI 线程（GUI 用 `slint::invoke_from_event_loop`）。
- 回调里不要阻塞、不要等锁、不要递归调用 `execute`；需要异步处理就转进自己的队列。

### 3. 初始状态与表单种子

```rust
let seed = service.form_seed();   // 含保存的密码，只用于初始化表单
let snapshot = service.snapshot(); // 完整展示状态，可随时重复调用
```

- `form_seed()` 只在**首次显示表单**或**窗口销毁后重建**时使用一次。之后
  `Notification::State` 里的 `settings` 是持久化值，不是“用户正在输入的内容”，不要拿它
  覆盖输入框。
- `seed.password` 是 `Option<String>`：`Some(密码)` 表示有保存的密码；`Some("")` 表示用户
  曾主动清除；`None` 表示没有保存过。按这个语义决定是否预填。
- `snapshot()` 是自包含的，窗口重建、从托盘重新打开、怀疑状态落后时都可以再取一次。

### 4. 执行命令

```rust
let outcome = service.execute(Command::Login {
    username,
    password,
    auto_reconnect: true,
});
```

- 命令是异步的：`execute` 返回时认证通常还没完成，结果通过后续通知回来。不要用返回的
  快照判断“已连接”。
- 返回 `CommandOutcome::Snapshot` 时可以直接渲染（与随后的 `Notification::State` 等价）；
  只有 `Command::DiagnosticsText` 返回 `CommandOutcome::Text`。
- 校验失败、保存失败等反馈不是错误返回值，而是下一份快照里的
  `settings.config_status` / `network_editor.message` / `failure` 等字段。
- `execute` 只在网卡枚举、写偏好这类短操作上阻塞；不要在 UI 线程上循环调用它。

### 5. 每秒 tick

```rust
service.tick();
```

- tick 由前端驱动，服务内部没有隐藏定时器。约 1 秒一次即可。
- 它负责：在线时长刷新、登录成功后自动收进托盘的计时、待执行的自动登录推进，并可能在
  tick 内推送 `Notification::State`。
- GUI 用 `slint::Timer`；daemon 自己起线程；测试里手动调用。
- 不 tick 的后果：在线时长停住、登录后自动隐藏不触发、待自动登录一直不推进。

### 6. 退出

```rust
service.execute(Command::Shutdown);
// 然后只等 Notification::ExitReady；收到后结束事件循环 / 退出进程。
```

- 不要直接 `std::process::exit`：后端可能还需要发下线报文。
- 不要自己 kill 会话线程；线程由服务管理。
- `ExitReady` 之后不会再有关联通知，可以安全退出。

## Snapshot 字段 → 界面控件

`Snapshot` 每次都是完整状态：拿到就整份刷新，不要只更新变化的字段。

### 顶层

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `api_version` | 序列化契约版本（当前 `1`） | 启动时校验；不匹配时提示前端/后端版本不一致 |
| `app_version` | 后端构建版本 | “关于”页或诊断信息 |
| `connection` | 连接状态 | 见下表 |
| `settings` | 已保存的账号/启动设置 | 见下表 |
| `adapter` | 当前网卡与地址 | 见下表 |
| `failure` | 失败卡片 | 见下表 |
| `probe` | 网络检测结果 | 见下表 |
| `network_editor` | 静态 IPv4 编辑器 | 见下表 |
| `campus_page` | 只读的 Dr.COM 网页状态 | 状态行文本，例如“网页状态未知…”；直接显示 |
| `tooltip` | 托盘/状态栏短句 | 托盘 tooltip 或状态栏；例如“已连接，正在保持在线” |

### `ConnectionView`

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `phase` | 机器可读阶段：`Disconnected` / `Connecting` / `Connected` / `Reconnecting` / `Disconnecting` / `WaitingForNetwork` | 配色、图标、动画、本地化；不要拿中文文案做判断 |
| `state_text` | 状态主文案：`未连接` / `连接中` / `已连接` / `重连中` / `断开中` / `等待网络` | 状态区大号文字，直接显示 |
| `detail` | 一行补充说明 | 状态下方小字 |
| `online` | 是否已认证且保活中 | 在线指示点；决定是否显示失败卡片 |
| `session_active` | 会话存在**或正在建立** | 禁用登录表单、把“连接”切换为“下线” |
| `online_seconds` | 在线秒数 | 需要自己算年龄/统计时用；一般不用 |
| `online_duration` | 已格式化时长（`0 秒` / `3 分 12 秒` / `1 时 02 分`） | 直接显示，不要自己格式化 |

### `SettingsView`

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `username` | 已记住的账号（空表示没保存过） | 账号输入框的初始值；只在种子/首次渲染时使用 |
| `auto_login` | 自动登录 | 复选框 |
| `auto_reconnect` | 自动重连 | 复选框 |
| `minimize_to_tray` | 关闭到托盘（也控制登录后自动隐藏） | 复选框；切换时立即发 `SetMinimizeToTray` |
| `start_with_windows` | 开机自启；`Some` 才表示能读到 | 复选框；`None` 时禁用或显示“未知” |
| `config_status` | 保存/读取反馈（`Some` 时） | 设置区状态行或 toast |

注意：`username`、`auto_login`、`auto_reconnect`、`minimize_to_tray` 同时是用户的输入。
**状态推送不要覆盖表单**，只在初始化时用 `form_seed()` 填一次（GUI 的
`apply_snapshot` 就有意跳过这几个字段）。

### `AdapterView`

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `available` | 是否找到可用物理网卡 | 空状态提示；没有网卡时可提示插网线 |
| `name` | 网卡友好名，或后端给出的“未检测到物理网卡” | 网络概览第一行 |
| `ipv4` | 本机 IPv4 | 概览行 |
| `mac` | 大写冒号分隔 MAC，或 `--` | 概览行 |
| `gateway` | 逗号分隔网关，或 `--` | 概览行 |
| `dns` | 逗号分隔 DNS，或 `--` | 概览行 |
| `ipv6` | 可路由 IPv6 摘要（隐藏链路本地地址） | 概览行；文本已由后端组织好 |

`--`、`未检测到物理网卡` 这类占位文案都是后端给出的，直接显示即可。

### `FailureView`

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `message` | 失败原因（已知时附处理建议） | 失败卡片正文 |
| `time` | 本地时间 `YYYY-MM-DD HH:MM:SS` | 详情里的“发生时间” |
| `stage` | 失败阶段，例如 `认证`、`网络通信` | 详情里的“阶段” |
| `code` | 稳定错误码，例如 `DRCOM-21`、`NETWORK-TimedOut` | 详情里的“错误码” |
| `duration` | 本次尝试耗时 | 详情里的“本次耗时” |
| `retry` | 下一步动作（`不会自动重试` / `将在 N 秒后重试…`） | 详情里的“重试” |

失败卡片由后端保存在进程生命周期内，关闭到托盘再打开仍会显示，直到下一次尝试覆盖。
没有失败时后端的 `message` 是固定占位文案（当前为 `本次运行尚未发生连接问题`），其余字段
为 `--`；旧 GUI 的显示条件是 `!connection.online && message != 占位文案`。前端不要自己
生成失败文案。

### `ProbeView`

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `busy` | 检测是否进行中 | 禁用“检测网络”按钮，文案可换成“检测中…” |
| `campus` | 校内 Ping 结果（`未检测` 表示还没测过） | 结果行 |
| `ipv4` | 公网 IPv4 HTTPS 结果 | 结果行 |
| `ipv6` | 公网 IPv6 HTTPS 结果 | 结果行 |

### `NetworkEditorView`

| 字段 | 含义 | 建议控件/用法 |
| --- | --- | --- |
| `visible` | 是否显示编辑器 | 覆盖层/独立对话框的显示开关；关闭时发 `CloseNetworkEditor` |
| `adapter_name` | 正在编辑的网卡 | 标题或说明文字 |
| `ip` / `mask` / `gateway` / `dns` | 后端填充的当前值 | 四个输入框；打开后再由用户编辑，按 `ApplyNetwork` 回传 |
| `message` | 说明或校验/错误结果 | 编辑器内提示行 |
| `busy` | 提权 `netsh` 是否在执行 | 禁用“保存/返回”按钮，文案可换成“应用中…” |

编辑器是后端驱动状态：`visible` 由快照决定，前端不要自己维护“打开/关闭”布尔值。

## 命令清单

| 命令 | 什么时候发 | 说明 |
| --- | --- | --- |
| `Login { username, password, auto_reconnect }` | 用户点“连接” | 异步认证；密码来自表单或 `form_seed` |
| `Logout` | 用户点“下线” | 停会话并下线；已有自动登录也会被取消 |
| `RefreshAdapter` | 用户点“刷新”/打开网络概览 | 重新枚举网卡并刷新 `AdapterView` |
| `Probe { kind }` | 用户点“检测网络” | `Campus` = 校内 Ping，`Internet` = 公网 IPv4+IPv6，`All` = 全部（旧 GUI 的 0/1/2 映射到这里） |
| `OpenNetworkEditor` | 用户点“修改网络设置” | 后端用当前选中网卡填充编辑器并置 `visible = true` |
| `CloseNetworkEditor` | 编辑器点“返回”/关闭 | 只关界面，不应用任何修改 |
| `ApplyNetwork { ip, mask, gateway, dns }` | 编辑器点“保存网络地址” | 后端校验并经提权 `netsh` 应用；结果在 `network_editor.message` 和 `busy` |
| `SaveSettings { username, password, auto_login, auto_reconnect, autostart, minimize_to_tray }` | 用户点“保存设置” | 持久化账号/启动偏好与开机自启；反馈在 `settings.config_status` |
| `ClearSavedPassword { username, auto_reconnect, minimize_to_tray }` | 用户点“清除已保存密码” | 清掉记住的密码、保留账号；成功后前端要清空密码框并取消自动登录（密码不在快照里） |
| `SetMinimizeToTray { enabled }` | “关闭到托盘”复选框变化时 | 立即生效（未保存也生效）；后端据此决定登录成功后是否自动收进托盘，前端关闭按钮也按同一复选框值决定隐藏还是退出 |
| `DiagnosticsText` | 用户点“复制/保存诊断结果” | 返回 `CommandOutcome::Text`；剪贴板/文件由前端处理 |
| `Shutdown` | 用户点“退出”/关闭窗口且不保留托盘 | 需要时先下发下线报文，完成后推 `ExitReady` |

所有命令都是“发出去就不等”的：错误与校验结果通过快照反馈，而不是返回值或异常。

## 通知语义

| 通知 | 何时到 | 前端做什么 |
| --- | --- | --- |
| `State { snapshot }` | 每次状态变更命令之后，以及后端 tick 刷新状态时（在线期间每秒一次） | 整份刷新界面；不要与旧值合并 |
| `HideToTray { note }` | 登录成功后自动隐藏，或用户要求关闭到托盘 | 隐藏/释放窗口，把 `note` 用作托盘提示；窗口生命周期归前端 |
| `ExitReady` | `Shutdown` 处理完毕（下线报文已发） | 结束事件循环或退出进程 |

- 通知在触发变化的线程上同步送达；多个线程可能并发调用 sink。前端必须自己切线程。
- 快照完整、跨线程顺序不保证；按到达顺序覆盖，最后一份为准。任何时候都可以用
  `snapshot()` 重新对齐。
- 通知没有“重放”：前端启动晚于某次变化时，直接取当前快照即可。
- 后端不会碰窗口：`HideToTray` 只是建议，隐藏、释放、重建窗口都是前端的事。

## 哪些状态属于前端

| 状态 | 归属 |
| --- | --- |
| 连接/会话、网卡选择、已保存设置、失败卡片、检测结果、编辑器可见性与值、网页状态、tooltip | **后端**（快照） |
| 正在输入的用户名/密码、自动登录/自动重连复选框、关闭到托盘复选框 | **前端**（初始化用 `form_seed`，之后不要被状态推送覆盖） |
| 静态 IPv4 编辑器里正在改的文本 | **前端**（打开时用快照填充，`ApplyNetwork` 时回传） |
| 窗口显示/隐藏、托盘、单实例、窗口重建 | **前端** |
| 剪贴板、导出文件、打开资源管理器/系统网络设置、文件对话框 | **前端** |
| 主题、布局、字体、快捷键、失败详情是否展开 | **前端** |
| 1 秒 tick 定时器、事件循环、线程间调度 | **前端** |

注意“关闭到托盘”复选框：它的持久化值来自 `SettingsView`，但切换时立即发
`SetMinimizeToTray`，让运行中的关闭按钮和新登录后的自动隐藏立刻按新值工作；不要等
“保存设置”。

## 状态文字与失败处理

- 所有状态/错误中文文案由后端给出（`state_text`、`detail`、`failure.*`、
  `settings.config_status`、`network_editor.message`、`probe.*`）。前端直接显示，不要用
  `phase`/`code` 重新拼一套中文，否则同一状态会出现两个说法。
- `phase` 用于样式与本地化；`code`/`stage` 用于详情展示，不适合当用户主文案。
- `connection.online == true` 时不显示失败卡片（旧 GUI 的做法）；失败后卡片保留到下一次
  尝试覆盖，不随窗口关闭消失。
- 需要在本地生成文案的只有前端自己的操作错误，例如剪贴板复制失败、导出文件失败、
  `platform::open_network_settings()` 失败。参考 GUI：`无法打开网络设置：{error}`、
  `复制失败：{error}`。
- 忙碌状态由后端给：`probe.busy`、`network_editor.busy`。前端不要自己推断“应该忙了”。
- 会话活动时用 `connection.session_active` 禁用登录表单，避免重复提交；`Connecting` /
  `WaitingForNetwork` / `Reconnecting` 阶段同样要禁用。

## 最小 Rust 示例

完整可运行版：把 `Cargo.toml` 指向本仓库的 `drcom-service`，用 `cargo run` 启动。示例只
负责打印快照，真实前端把 `render`/`hide` 换成自己的界面即可。

```toml
[package]
name = "my-drcom-frontend"
version = "0.1.0"
edition = "2024"

[dependencies]
drcom-service = { path = "../drcom-rs/crates/drcom-service" }
```

```rust
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use drcom_service::{Command, Notification, Service, ServiceOptions, Snapshot};

fn main() {
    let service = Service::new(ServiceOptions::default());

    // 1. 订阅：sink 在后台线程上同步调用，这里转进自己的队列。
    let (sender, receiver) = mpsc::channel();
    service.subscribe(Arc::new(move |note| {
        let _ = sender.send(note);
    }));

    // 2. 只读一次表单种子；先整份渲染一次，不等第一个通知。
    let seed = service.form_seed();
    render(&service.snapshot());

    // 3. 用户点“连接”时提交；结果从通知回来。
    service.execute(Command::Login {
        username: seed.username,
        password: seed.password.unwrap_or_default(),
        auto_reconnect: seed.auto_reconnect,
    });

    // 4. 前端持有 1 秒节拍；退出时发 Command::Shutdown，收到 ExitReady 后 break。
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Notification::State { snapshot }) => render(&snapshot),
            Ok(Notification::HideToTray { note }) => hide(&note),
            Ok(Notification::ExitReady) => break,
            Err(RecvTimeoutError::Timeout) => service.tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn render(snapshot: &Snapshot) {
    // 真实前端在这里更新控件；示例只打印关键字段。
    println!(
        "{} / {} / {}",
        snapshot.connection.state_text, snapshot.connection.online_duration, snapshot.tooltip
    );
}

fn hide(note: &str) {
    // 真实前端在这里隐藏/释放窗口，并把 note 放进托盘提示。
    println!("隐藏到托盘：{note}");
}
```

不链接 Rust 也没关系：同样的流程可以写成“读初始快照 → 发命令 → 处理通知”，只是把
函数调用换成 JSON 行，见 [ipc-protocol.md](ipc-protocol.md)。

## 其他语言：走 daemon

- 启动 `drcom-daemon`（stdio 模式或回环 TCP），先读它给的初始快照，再循环读通知、写命令。
- 命令与通知的字段就是 `api.rs` 里的 `Command` / `Notification`（serde 序列化后带
  `command` / `event` 标签），信封与帧格式见 [ipc-protocol.md](ipc-protocol.md)。
- 读快照里的 `api_version` 判断兼容性。
- 传输只有 stdio 与回环 TCP，没有 TLS/鉴权；登录命令会携带密码，**不要**把 daemon 暴露到
  非回环地址。
- daemon 自己 tick，客户端不需要也不应该模仿 tick。

## 新前端检查清单

- [ ] 构造 `Service` 后先 `subscribe`，再渲染一次 `snapshot()`
- [ ] `form_seed()` 只用于初始化表单，状态推送不覆盖用户名/密码/复选框
- [ ] 每秒调用 `tick()`
- [ ] 连接/下线/刷新/检测/网络设置/保存/清除密码/复制诊断/退出都映射到对应 `Command`
- [ ] `probe.busy` 时禁用检测按钮；`network_editor.busy` 时禁用保存与返回
- [ ] 编辑器用 `network_editor.visible` 控制显示，关闭时发 `CloseNetworkEditor`
- [ ] “关闭到托盘”切换时立即发 `SetMinimizeToTray`
- [ ] “清除已保存密码”成功后清空密码输入并取消自动登录
- [ ] 复制/导出诊断：`DiagnosticsText` 的文本由前端放进剪贴板或文件
- [ ] `HideToTray`：隐藏/释放窗口，`note` 用于托盘提示
- [ ] 退出：`Shutdown` → 等 `ExitReady` → 结束事件循环；不提前 `exit`
- [ ] sink 里不阻塞、不直接操作界面线程
- [ ] 不持久化、不打印、不复制密码；诊断文本里不拼凭据
- [ ] 跨进程接入时检查 `api_version`，并遵守 `ipc-protocol.md` 的安全约束

## 常见坑

- 把 `execute(Login)` 当成同步请求，随后立刻读快照判断“已连接”——认证是异步的，以通知为准。
- 用 `State` 推送重置正在输入的表单，用户打字打到一半被清空。
- 自己格式化 `online_duration`，和后端显示不一致。
- 忘了 tick：时长不动、登录后不自动进托盘。
- 在 sink 里直接改 UI：GUI 会跨线程访问组件，轻则卡顿重则崩溃。
- 退出时直接关进程：下线报文没发出去，账号可能被服务器认为仍在线。
- 试图从快照或 IPC 找密码——它永远不在那里。
- 自己拼错误文案：与后端状态不一致，用户看到两个版本的说法。
