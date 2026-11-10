# Dr.COM JSON-lines IPC 协议（drcom-ipc）

本文定义 `drcom-daemon` 与外部程序之间的线协议。目标是：**任意语言、任意进程都能
使用同一套后端能力，而不需要链接 Rust、不能有 GUI、也不重新实现认证逻辑。**

- 协议名：`drcom-ipc`
- 信封协议版本 `protocol_version`：**1**（`drcom-ipc` crate 自己维护）
- 前端契约版本 `api_version`：**1**（`drcom-service` 的 `API_VERSION`，两者独立演进）
- 程序版本 `app_version`：当前为 `1.2.1`（`CARGO_PKG_VERSION`）
- 实现位置：[crates/drcom-ipc](../crates/drcom-ipc)、[crates/drcom-daemon](../crates/drcom-daemon)
- 冻结的契约以 [crates/drcom-service/src/api.rs](../crates/drcom-service/src/api.rs) 为准

> 本文描述的是**已经实现并测试**的协议，不是设想。除用 `{...}` / `…` 明确省略了
> 完整快照内容的节略示例外，文中 JSON 均为合法 JSON；完整快照见 §6。
> 示例里的账号、IP、MAC 是脱敏后的示意值。

---

## 1. 它解决什么问题

`drcom-service` 是与前端无关的应用层，但它是一个 Rust 库：

- Rust 前端（Slint GUI）可以直接在进程内链接它；
- Python / PowerShell / C# / 其他进程无法链接它，也不应该为了登录一次校园网而
  复刻一遍 Dr.COM 协议、保活、网卡选择与失败诊断。

`drcom-daemon` 是无界面宿主：它创建一个 `drcom_service::Service`，每秒 `tick()`
一次，把这套能力按本文的 JSON-lines 协议暴露在 **stdio** 或 **回环 TCP** 上。
`drcom-ipc` 是协议实现 crate：信封类型、行帧、服务循环、多客户端广播、阻塞客户端。

```text
              进程内 Rust 前端                         其他进程 / 其他语言
        ┌────────────────────────┐              ┌──────────────────────────┐
        │ drcom-gui (Slint)      │              │ Python / PowerShell / …  │
        └───────────┬────────────┘              └────────────┬─────────────┘
                    │ Command / Snapshot                    │ 一行一个 JSON
                    ▼                                       ▼
        ┌────────────────────────┐              ┌──────────────────────────┐
        │ drcom-service          │◀─────────────│ drcom-daemon + drcom-ipc │
        │ 应用状态机（无 UI 依赖）│  同一份实现   │ stdio / 127.0.0.1 TCP    │
        └───────────┬────────────┘              └──────────────────────────┘
                    ▼
        ┌────────────────────────┐
        │ drcom-core             │
        │ 协议 / 会话 / 网卡 / …  │
        └────────────────────────┘
```

**不是**新写一套后端：命令、快照、通知与 GUI 在进程内拿到的是同一批类型。

## 2. 为什么不是 FFI / HTTP

| 方案 | 不采用的原因 |
| --- | --- |
| **FFI（cdylib + C ABI）** | Rust 没有稳定 ABI；跨语言要额外处理生命周期、字符串所有权、回调线程与 panic 边界；每个语言要写并维护绑定；后端每次升级前端都要重新链接/重编译。 |
| **HTTP/JSON API** | 需要引入 HTTP 服务器与客户端栈（本项目要求离线、零新增依赖）；本地单用户场景还要额外处理端口、鉴权与进程生命周期；状态推送还要再叠 SSE/WebSocket。 |
| **JSON-lines over stdio/loopback** | 每行一个对象，标准库就能读写；行协议可在终端直接观察和排查；stdio 天然绑定宿主进程的生命周期；不需要任何第三方包。 |

以后若需要 Windows 命名管道 / Unix domain socket，可以复用本文的信封，只换传输层。

## 3. 传输方式

### 3.1 stdio（默认，推荐）

宿主进程把 daemon 作为子进程拉起：

```powershell
drcom-daemon --stdio --no-startup-probe --no-auto-login
```

- daemon 的 **stdin = 客户端请求**；
- daemon 的 **stdout = 协议输出（只有 JSON 行，没有任何日志/提示）**；
- daemon 的 **stderr = 日志与状态**（人类可读，允许随时出现）；
- **stdin EOF 视为优雅关闭请求**：daemon 执行下线、发出 `exit_ready`、退出码 0；
- 宿主若不主动关闭 stdin，daemon 会一直运行直到收到 `shutdown` 请求或进程被杀。

stdio 是最私密的通道：只有创建它的父进程能读写，且不存在监听端口。

### 3.2 TCP 回环

```powershell
drcom-daemon --listen 127.0.0.1:47800 --no-auto-login
drcom-daemon --listen 127.0.0.1:0 --no-startup-probe --no-auto-login
```

- `--listen` 绑定成功后立即在 **stderr** 打印一行：

  ```text
  drcom-daemon listening 127.0.0.1:<实际端口>
  ```

  `PORT=0` 表示由系统分配临时端口，以这一行为准。
- **只允许回环地址**：`127.0.0.1`、`::1`、`localhost`。给出 `0.0.0.0`、局域网 IP
  或公网 IP 会直接拒绝并以退出码 2 结束（登录命令会携带明文密码，协议不提供 TLS/鉴权）。
- 多个客户端可以同时连接、共享同一个 `Service`。每个 TCP 连接建立时都会收到自己的
  `ready` + 当前 `state`，随后的通知广播给所有连接（见 §11）。
- IPv6 回环写作 `--listen [::1]:47800`。

### 3.3 怎么选

| 场景 | 建议 |
| --- | --- |
| 一个宿主程序拉起后端（脚本、桌面应用、测试） | **stdio**：生命周期最简单，无端口、无监听面。 |
| 多个本地进程共享一个后端（托盘守护 + 前端 + 监控） | **TCP 回环**：注意任何本机进程都能连上，见 §13。 |
| 跨机器 / 公网 | **不支持，也不要转发端口。** |

## 4. 帧格式

**一行 = 一个紧凑 UTF-8 JSON 对象 + `\n`。** 没有长度前缀、没有多行对象、没有分块。

- 行内不能出现裸换行；JSON 字符串里的换行由 `\n` 转义表示。
- 接收方兼容 `\r\n`（会去掉行尾 `\r`）；**空行会被忽略**。
- 每行开头的 UTF-8 BOM 会被容忍（对 Windows 客户端友好），其余位置不支持。
- **单行上限 1 MiB（1 048 576 字节，不含行尾换行）**。超限时服务端返回 `id = 0`
  的错误响应；如果能在允许范围内把这一行剩余部分读到行尾，连接保持可用，
  否则关闭连接。
- 不是合法 UTF-8 的行按同上规则返回错误并尽量保持连接。
- 写入方必须**一次写完整行**（服务端实现里 `write_all` 一整行），避免多线程下
  两行内容交错。
- **请求和通知会交错出现在同一条连接上**：客户端必须按 `id` 匹配响应，不能假设
  下一行就是响应。

示例：

```text
{"id":1,"type":"snapshot"}
{"id":2,"type":"ping"}
```

## 5. 连接建立：ready 与初始 state

连接（或 stdio 启动）后，服务端**立即**发送两行：

1. `ready`：协议与版本信息；
2. `state`：当前完整快照。

```json
{"event":"ready","protocol":"drcom-ipc","protocol_version":1,"api_version":1,"app_version":"1.2.1"}
{"event":"state","snapshot":{"api_version":1,"app_version":"1.2.1","connection":{"phase":"disconnected","state_text":"未连接","detail":"网络已就绪，填写账号密码后即可连接","online":false,"session_active":false,"online_seconds":null,"online_duration":"--"},"settings":{"username":"20230001","auto_login":true,"auto_reconnect":true,"minimize_to_tray":true,"start_with_windows":false,"config_status":"已记住你的账号"},"adapter":{"available":true,"name":"以太网","ipv4":"10.100.61.20","mac":"AA:BB:CC:DD:EE:FF","gateway":"10.100.61.1","dns":"10.10.10.10","ipv6":"--"},"failure":{"message":"本次运行尚未发生连接问题","time":"--","stage":"--","code":"--","duration":"--","retry":"--"},"probe":{"busy":false,"campus":"未检测","ipv4":"未检测","ipv6":"未检测"},"network_editor":{"visible":false,"adapter_name":"","ip":"","mask":"","gateway":"","dns":"","message":"","busy":false},"campus_page":"","tooltip":""}}
```

`ready` 字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `event` | string | 固定 `"ready"` |
| `protocol` | string | 固定 `"drcom-ipc"` |
| `protocol_version` | u32 | 信封/帧协议版本，当前 `1` |
| `api_version` | u32 | `drcom-service` 前端契约版本，当前 `1` |
| `app_version` | string | daemon 构建版本（`CARGO_PKG_VERSION`） |

注意：

- 初始 `state` 之后可能**很快又出现新的 `state`**（后端启动时刷新网卡/探测会发布快照）。
  客户端应持续渲染最新的 `state`，不要假设"第二行之后就没有初始通知"。
- TCP 的每个新连接都会收到自己的 `ready` + 当时的 `state`。
- 客户端可以在收到 `ready` 后立即发请求（不必等 `state`），但不能把 `state` 当成响应。

## 6. 快照 Snapshot

`state` 通知和 `snapshot` 请求返回的是**完整快照**：应用它就能重绘整个前端，不需要
diff、回放或缓存历史。所有字段都是显示/判断用的最终值；中文字符串可以直接展示。

### 6.1 顶层字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `api_version` | u32 | 契约版本，应为 `1` |
| `app_version` | string | 后端构建版本 |
| `connection` | object | 连接状态（见 6.2） |
| `settings` | object | 已保存的账号/启动设置（见 6.3） |
| `adapter` | object | 当前选中的物理网卡（见 6.4） |
| `failure` | object | 当前进程生命周期内最近一次失败卡片（见 6.5） |
| `probe` | object | "检测网络"结果（见 6.6） |
| `network_editor` | object | 静态 IPv4 编辑器的后端状态（见 6.7） |
| `campus_page` | string | Dr.COM 网页只读状态文本，可能为空 |
| `tooltip` | string | 托盘/状态栏摘要文本，可能为空 |

### 6.2 `connection`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `phase` | string | 机器可读阶段：`disconnected` / `connecting` / `connected` / `reconnecting` / `disconnecting` / `waiting_for_network` |
| `state_text` | string | 展示用状态：`未连接` / `连接中` / `已连接` / `重连中` / `断开中` / `等待网络` |
| `detail` | string | 状态下一行说明 |
| `online` | bool | 仅当 UDP 会话已认证且心跳正常时为 `true` |
| `session_active` | bool | 存在会话**或**正在建立会话（前端用它切换"连接/下线"按钮） |
| `online_seconds` | u64 \| null | 在线秒数；不在线时为 `null` |
| `online_duration` | string | 展示用在线时长，例如 `3 分 12 秒`、`--` |

### 6.3 `settings`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `username` | string | 已保存的账号，空串表示未保存 |
| `auto_login` | bool | 已保存的自动登录设置 |
| `auto_reconnect` | bool | 已保存的自动重连设置 |
| `minimize_to_tray` | bool | 已保存的"关闭到托盘"设置 |
| `start_with_windows` | bool \| null | 开机自启状态；读不到注册表时为 `null` |
| `config_status` | string \| null | 保存/读取偏好后的反馈文本 |

**快照没有密码字段。** 密码只在 `login` / `save_settings` 命令里出现（见 §13）。

### 6.4 `adapter`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `available` | bool | 是否找到可用物理网卡 |
| `name` | string | 网卡友好名，或 `未检测到物理网卡` |
| `ipv4` | string | IPv4 地址或 `--` |
| `mac` | string | 大写冒号分隔 MAC 或 `--` |
| `gateway` | string | 逗号分隔网关列表或 `--` |
| `dns` | string | 逗号分隔 DNS 列表或 `--` |
| `ipv6` | string | 全局 IPv6 摘要或 `--` |

### 6.5 `failure`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `message` | string | 失败原因（可能带处理建议） |
| `time` | string | 发生时刻 `YYYY-MM-DD HH:MM:SS` 或 `--` |
| `stage` | string | 失败阶段，例如 `认证`、`网络通信` |
| `code` | string | 稳定错误码，例如 `DRCOM-21`、`NETWORK-TimedOut` |
| `duration` | string | 本次尝试耗时 |
| `retry` | string | 下一步动作（不重试/将在 N 秒后重试…） |

### 6.6 `probe`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `busy` | bool | 检测进行中；前端应禁用按钮 |
| `campus` | string | 校内 Ping 结果，未检测为 `未检测` |
| `ipv4` | string | 公网 IPv4 HTTPS 结果 |
| `ipv6` | string | 公网 IPv6 HTTPS 结果 |

### 6.7 `network_editor`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `visible` | bool | 编辑器是否应显示 |
| `adapter_name` | string | 目标网卡名 |
| `ip` / `mask` / `gateway` / `dns` | string | 表单初值 |
| `message` | string | 校验/执行反馈 |
| `busy` | bool | 提权 `netsh` 正在执行 |

完整示例（脱敏、示意值）：

```json
{
  "api_version": 1,
  "app_version": "1.2.1",
  "connection": {
    "phase": "disconnected",
    "state_text": "未连接",
    "detail": "网络已就绪，填写账号密码后即可连接",
    "online": false,
    "session_active": false,
    "online_seconds": null,
    "online_duration": "--"
  },
  "settings": {
    "username": "20230001",
    "auto_login": true,
    "auto_reconnect": true,
    "minimize_to_tray": true,
    "start_with_windows": false,
    "config_status": "已记住你的账号"
  },
  "adapter": {
    "available": true,
    "name": "以太网",
    "ipv4": "10.100.61.20",
    "mac": "AA:BB:CC:DD:EE:FF",
    "gateway": "10.100.61.1",
    "dns": "10.10.10.10",
    "ipv6": "--"
  },
  "failure": {
    "message": "本次运行尚未发生连接问题",
    "time": "--",
    "stage": "--",
    "code": "--",
    "duration": "--",
    "retry": "--"
  },
  "probe": {
    "busy": false,
    "campus": "未检测",
    "ipv4": "未检测",
    "ipv6": "未检测"
  },
  "network_editor": {
    "visible": false,
    "adapter_name": "",
    "ip": "",
    "mask": "",
    "gateway": "",
    "dns": "",
    "message": "",
    "busy": false
  },
  "campus_page": "",
  "tooltip": ""
}
```

> 线上实际传输是**紧凑单行**，上面的缩进只是为了阅读。

## 7. 消息类型

### 7.1 请求 Request

```json
{"id":1,"type":"snapshot"}
```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | u64 | 客户端自选请求 id，服务端原样回显。建议从 1 开始；`0` 保留给"解析失败"的错误响应。 |
| `type` | string | `command` / `snapshot` / `ping` / `shutdown` |

四种请求：

| `type` | 额外字段 | 说明 |
| --- | --- | --- |
| `snapshot` | — | 只读地取当前快照，不改变任何状态 |
| `ping` | — | 健康检查，返回协议/版本信息 |
| `command` | `command`：一个命令对象 | 1:1 转发给 `Service::execute` |
| `shutdown` | — | 执行 `Command::Shutdown`，回应后 daemon 优雅退出 |

示例：

```json
{"id":1,"type":"snapshot"}
{"id":2,"type":"ping"}
{"id":3,"type":"command","command":{"command":"refresh_adapter"}}
{"id":4,"type":"shutdown"}
```

- 同一连接上可以连续发多个请求，服务端按到达顺序逐个处理、逐个回应；客户端应按
  `id` 匹配。不要依赖"响应顺序与通知顺序"。
- 一个慢命令（例如枚举网卡、提权 `netsh`）只会阻塞**当前连接**的后续请求；
  TCP 的其他连接不受影响。

### 7.2 响应 Response

成功时 `ok = true`，并且恰好携带一个结果字段：

```json
{"id":1,"ok":true,"snapshot":{...}}
{"id":2,"ok":true,"info":{"protocol":"drcom-ipc","protocol_version":1,"api_version":1,"app_version":"1.2.1"}}
{"id":3,"ok":true,"text":"DrCom 校园网助手 1.2.1\n检测结果生成时间：…"}
```

失败时：

```json
{"id":42,"ok":false,"error":"invalid request: unknown variant `warp`, expected one of `command`, `snapshot`, `ping`, `shutdown`"}
```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | u64 | 回显请求 id；解析失败且读不到 id 时为 `0` |
| `ok` | bool | 是否成功 |
| `snapshot` | object \| 省略 | 成功且产生状态变化时的完整快照 |
| `text` | string \| 省略 | 成功且命令返回文本时（当前只有 `diagnostics_text`） |
| `info` | object \| 省略 | `ping` 的协议/版本信息（字段同 `ready` 去掉 `event`） |
| `error` | string \| 省略 | 失败原因，面向开发者，不保证稳定 |

`info` 示例：

```json
{"protocol":"drcom-ipc","protocol_version":1,"api_version":1,"app_version":"1.2.1"}
```

### 7.3 服务端通知 Notification

与 `ready` 共用"带 `event` 字段的 JSON 对象"这一外形，但方向是服务端 → 客户端，
并且**没有 `id`**。`drcom-service` 的通知被原样转发：

| `event` | 额外字段 | 含义 |
| --- | --- | --- |
| `state` | `snapshot` | 新快照可用。完整替换前端状态。 |
| `hide_to_tray` | `note` | 前端"关闭到托盘"（登录成功后自动隐藏）。纯桌面概念，非 GUI 客户端可忽略但不要断开连接。 |
| `exit_ready` | — | 后端已完成退出流程，daemon 随后关闭所有连接。 |

示例：

```json
{"event":"state","snapshot":{...}}
{"event":"hide_to_tray","note":"已连接，正在保持在线"}
{"event":"exit_ready"}
```

客户端必须**忽略未知的 `event`**（未来可能增加事件），不能因此断开或报错。
`exit_ready` 之后不再有响应；TCP 连接随后关闭，stdio 下 stdout 到达 EOF。

## 8. 命令 Command

`type:"command"` 的 `command` 对象与 `drcom-service` 的 `Command` 枚举一一对应：
内部标签是 `"command"`，变体名为 `snake_case`。**命令是 fire-and-forget 的**：

- 命令的"结果"通常是紧跟其后的 `snapshot` 响应（如果未产生状态变化也是当前快照）；
- 长操作（认证、探针、`netsh`）在后台线程执行，进展通过后续 `state` 通知体现；
- 校验反馈出现在后续快照的 `failure` / `settings.config_status` /
  `network_editor.message` / `connection.detail` 等字段里；
- 只有 `diagnostics_text` 通过 `CommandOutcome::Text` 直接返回文本。

| 命令 | 请求示例 | 结果 |
| --- | --- | --- |
| 登录 | `{"command":"login","username":"20230001","password":"••••","auto_reconnect":true}` | `snapshot`；认证在后台进行 |
| 下线 | `{"command":"logout"}` | `snapshot` |
| 刷新网卡 | `{"command":"refresh_adapter"}` | `snapshot`；随后可能有 `state` |
| 检测网络 | `{"command":"probe","kind":"all"}` | `snapshot`；`kind` = `campus` / `internet` / `all` |
| 打开静态 IP 编辑器 | `{"command":"open_network_editor"}` | `snapshot`（`network_editor.visible = true`） |
| 关闭静态 IP 编辑器 | `{"command":"close_network_editor"}` | `snapshot` |
| 应用静态 IPv4 | `{"command":"apply_network","ip":"10.100.61.20","mask":"255.255.255.0","gateway":"10.100.61.1","dns":"10.10.10.10"}` | `snapshot`；提权执行，`network_editor.busy` 反映进度 |
| 保存设置 | `{"command":"save_settings","username":"20230001","password":"••••","auto_login":true,"auto_reconnect":true,"autostart":false,"minimize_to_tray":true}` | `snapshot`；密码由服务层按平台策略持久化 |
| 清除保存的密码 | `{"command":"clear_saved_password","username":"20230001","auto_reconnect":true,"minimize_to_tray":true}` | `snapshot` |
| 设置关闭到托盘 | `{"command":"set_minimize_to_tray","enabled":true}` | `snapshot` |
| 诊断文本 | `{"command":"diagnostics_text"}` | **`text`**（网卡 + 最近失败 + 日志摘要） |
| 退出后端 | `{"command":"shutdown"}` | `snapshot`，随后 daemon 退出（等价于 `type:"shutdown"`） |

完整请求示例：

```json
{"id":10,"type":"command","command":{"command":"login","username":"20230001","password":"s3cret","auto_reconnect":true}}
{"id":11,"type":"command","command":{"command":"probe","kind":"all"}}
{"id":12,"type":"command","command":{"command":"apply_network","ip":"10.100.61.20","mask":"255.255.255.0","gateway":"10.100.61.1","dns":"10.10.10.10"}}
{"id":13,"type":"command","command":{"command":"diagnostics_text"}}
```

## 9. 错误处理

错误是**按请求**返回的，连接通常继续可用：

| 情况 | 响应 |
| --- | --- |
| JSON 语法错误 | `{"id":0,"ok":false,"error":"invalid JSON: …"}` |
| 顶层不是对象 / 缺 `type` / 未知 `type` | 能读到数字 `id` 就用它，否则 `0` |
| 未知命令变体 / 字段类型错误 | 用请求的 `id` 返回 `ok:false` |
| 行超过 1 MiB | `id = 0`，错误文本包含 `1048576`；整行被读掉则连接保持 |
| 行不是合法 UTF-8 | `id = 0`，错误文本包含 `UTF-8`；整行被读掉则连接保持 |

无法恢复到行边界的输入（例如超大行且长时间没有换行）会在报错后**关闭连接**；
连接级 I/O 错误也会直接关闭。客户端应把"连接关闭"视为可恢复故障：重新连接、
重读 `ready` + `state` 即可重新同步。

其他约定：

- 服务端**没有请求超时**：像 `apply_network` 这类命令可能耗时较长，超时策略由客户端
  自己决定；客户端超时后不保证该请求没有被执行。
- `error` 字符串用于诊断与显示，**不要**用字符串匹配做业务分支；`ok` 才是稳定信号。
- 命令执行失败（例如账号密码被拒）通常不是 `ok:false`，而是 `ok:true` + 后续快照里的
  `failure` 卡片；`ok:false` 表示"请求本身没被受理"。

## 10. 多客户端语义（TCP）

- daemon 只有一个 `Service`、一个 1 秒 ticker；所有连接共享同一份状态。
- 任一客户端发送的命令都会改变全局状态，广播给**所有**客户端（包括发起者）。
- `state` 通知与命令响应可能交错：同一命令可能先收到 `state`、再收到 `snapshot` 响应，
  也可能反过来（不同线程写入）。两者都是完整快照，以最后收到的为准即可。
- 每个客户端有独立的有界通知队列（64 条）。客户端如果长期不读、队列写满，会被
  从广播中**安全移除**（不会阻塞后端，也不会影响其他客户端）；它的请求/响应仍可用，
  但需要重新连接才能重新收到通知。死连接同理。
- 任一客户端的 `shutdown` 会结束**整个 daemon**（退出码 0），并向所有客户端广播
  `exit_ready`。这是单后端、单用户模型，没有"只关我的连接"。

## 11. tick 与状态更新节奏

- daemon 每 **1 秒**调用一次 `Service::tick()`（在线时长、自动隐藏、待执行的自动登录等）。
- 产生 `state` 通知的时机（由服务层保证）：命令改变状态后；会话在线时每个 tick；
  后台任务（认证、探针、网卡刷新、`netsh`）取得进展时。
- 未连接且无操作时，通常**不会**每秒推送 `state`；需要时用 `snapshot` 请求主动读取。
- 快照是完整状态：客户端应无条件替换视图，不要只应用 diff，也不要假设通知顺序。

## 12. 版本与兼容性

三个版本号彼此独立：

| 名称 | 所在位置 | 当前值 | 升级规则 |
| --- | --- | --- | --- |
| `protocol_version` | `drcom-ipc` crate | `1` | 信封、帧格式或请求/响应外形发生**破坏性**变化时 +1 |
| `api_version` | `drcom-service` 的 `API_VERSION` | `1` | `Command` / `Snapshot` / `Notification` 契约发生破坏性变化时 +1 |
| `app_version` | daemon 构建 | `1.2.1` | 程序发布版本，仅用于诊断 |

兼容规则：

1. 加字段/加通知事件是**兼容**变更（serde 默认忽略未知字段；客户端应忽略未知事件）。
2. 删除字段、改变字段类型/含义、增加必填字段、改变现有命令语义是破坏性变更，
   必须提升对应版本号。
3. 客户端应在 `ready` 里检查 `protocol` 与 `protocol_version`，并核对
   `api_version == 1`；不匹配时明确报"版本不兼容"，不要盲目发送命令。
4. `error` 文案、中文显示文本、`app_version` 不构成机器可依赖的接口。
5. 当前没有能力协商/特性位；未来若增加，只能以新增字段的方式出现。

## 13. 安全说明

请按"本机同一用户信任"模型使用：

- **只支持 stdio 与回环 TCP。** `--listen` 拒绝任何非回环地址；不要用端口转发、
  反向代理或 VPN 把端口暴露出去。
- **没有认证、没有 TLS、没有权限隔离。** 本机任何能连接该回环端口的进程都能：
  发送登录/保存设置命令（其中含明文密码）、读取快照（含账号名、网卡/IP/MAC）、
  发送 `shutdown` 结束 daemon。
- 因此**优先用 stdio**；需要 TCP 时只在本机使用，并注意同机其他程序的权限。
- `login` 与 `save_settings` 命令会携带**明文密码**（JSON 字符串）。
  - daemon 不记录请求内容，日志只包含连接/监听/错误等状态行；
  - 密码不会出现在 `Snapshot` 或 `ready` 中；
  - `drcom-service` 的 `form_seed()`（含已保存密码）**不通过 IPC 暴露**；
  - 密码是否落盘、如何加密由服务层的偏好存储（Windows DPAPI）决定；
  - 客户端自己也应避免把含密码的行写入终端历史/日志。
- `save_settings` 的 `autostart` 会写开机自启；诊断脚本建议只用
  `--no-startup-probe --no-auto-login` 并避免 `save_settings`/`login`。
- `diagnostics_text` 返回网卡、错误码与内存日志摘要，不含密码。
- 快照里的 `settings.username` 是**已保存账号名**，属于个人信息；转发/截图时注意脱敏。

## 14. `drcom-daemon` 命令行参考

```text
用法：
  drcom-daemon [--stdio] [选项]
  drcom-daemon --listen <ip:port> [选项]

传输：
  --stdio                 通过标准输入/输出提供协议（默认）
  --listen <ip:port>      在回环地址上监听 TCP；端口 0 表示自动分配，
                          实际地址打印到 stderr
  --no-startup-probe      启动时不访问 Dr.COM 网页状态（只读探测）
  --no-auto-login         忽略配置里的自动登录；诊断/脚本务必加上，
                          避免自动抢占真实校园网会话
  --help, -h              显示帮助并退出
  --version, -V           显示版本并退出
```

- 不带参数等价于 `--stdio`。
- `--stdio` 与 `--listen` 同时出现是参数错误（退出码 2）。
- `--listen` 也接受 `--listen=<ip:port>` 与 `localhost:<port>`。
- 其他选项：不读取额外配置；`settings: None` 表示沿用与 GUI 相同的 `drcom.ini`
  发现/加载路径（已保存账号会出现在 `snapshot.settings.username`，密码不会）。
- `--help` / `--version` 输出到 **stdout**（此时还没有开始服务，不违反"stdout 只有协议"）；
  运行后的所有日志都到 **stderr**。
- 成功监听后 stderr 固定出现 `drcom-daemon listening <addr>`。

退出码：

| 退出码 | 含义 |
| --- | --- |
| `0` | 正常/优雅退出：收到 `shutdown`、stdin EOF，或后端已确认下线 |
| `1` | 运行时错误：绑定失败、协议 I/O 不可恢复等（stderr 有原因） |
| `2` | 参数错误：未知参数、非法/非回环监听地址、传输冲突 |
| 其他 | 被系统/用户强制终止（例如 Ctrl+C、`taskkill`），**不保证已下线** |

生命周期：

```text
启动 ──► ready ──► state ──► 请求/通知循环 ──┬─► shutdown 请求（先写响应）
                                          ├─► stdin EOF（stdio）
                                          └─► 后端主动 ExitReady
                                                   │
                    退出前：释放被缓冲的通知 ──► exit_ready ──► 关闭连接 ──► 退出 0
```

收到 `shutdown` 时，daemon 会先写完该请求的响应，再等待服务层完成下线（有活动会话时
最多约 10 秒；已离线时很快），然后广播 `exit_ready` 并退出。若服务层未及时确认，
daemon 仍会退出并在 stderr 说明。**强杀进程不会执行下线流程**，请优先用请求或关闭
stdin。

## 15. 最小客户端示例

完整、可直接运行的脚本在 [examples/ipc/](../examples/ipc/)：`client.py` 与
`client.ps1`（都只用标准库，且用 `--no-startup-probe --no-auto-login` 拉起 daemon）。
下面只是最小骨架。

### Python 3

```python
import json
import subprocess

proc = subprocess.Popen(
    [r"target\debug\drcom-daemon.exe", "--stdio", "--no-startup-probe", "--no-auto-login"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
)

def send(message):
    proc.stdin.write((json.dumps(message, separators=(",", ":")) + "\n").encode("utf-8"))
    proc.stdin.flush()

def read():
    return json.loads(proc.stdout.readline())

ready = read()          # {"event":"ready", ...}
state = read()          # {"event":"state", "snapshot": ...}
send({"id": 1, "type": "snapshot"})
while True:
    message = read()
    if message.get("id") == 1:
        break
print(message["snapshot"]["connection"]["state_text"])
send({"id": 2, "type": "shutdown"})
print("exit:", proc.wait(timeout=10))
```

### Windows PowerShell

```powershell
$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = "target\debug\drcom-daemon.exe"
$psi.Arguments = "--stdio --no-startup-probe --no-auto-login"
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$process = [System.Diagnostics.Process]::Start($psi)

$ready = $process.StandardOutput.ReadLine() | ConvertFrom-Json
$state = $process.StandardOutput.ReadLine() | ConvertFrom-Json
$process.StandardInput.WriteLine('{"id":1,"type":"snapshot"}')
$process.StandardInput.Flush()
while ($true) {
    $message = $process.StandardOutput.ReadLine() | ConvertFrom-Json
    if ($message.PSObject.Properties.Name -contains 'id' -and $message.id -eq 1) { break }
}
$message.snapshot.connection.state_text
$process.StandardInput.WriteLine('{"id":2,"type":"shutdown"}')
$process.StandardInput.Flush()
if (-not $process.WaitForExit(10000)) { $process.Kill() }
$process.ExitCode
```

运行示例（先构建）：

```powershell
cargo build --offline -p drcom-daemon
python examples\ipc\client.py
powershell -ExecutionPolicy Bypass -File examples\ipc\client.ps1
```

> Windows PowerShell 5.1 的控制台若显示中文乱码，先执行 `chcp 65001`（不影响协议，
> 只影响终端显示）。两个示例都带超时和 `finally` 清理，不会留下 daemon 进程。

## 16. 一次完整会话（节选）

```text
# daemon -> 客户端（连接建立）
{"event":"ready","protocol":"drcom-ipc","protocol_version":1,"api_version":1,"app_version":"1.2.1"}
{"event":"state","snapshot":{...}}

# 客户端 -> daemon
{"id":1,"type":"ping"}

# daemon -> 客户端（info 为 ping 结果）
{"id":1,"ok":true,"info":{"protocol":"drcom-ipc","protocol_version":1,"api_version":1,"app_version":"1.2.1"}}

# 客户端 -> daemon
{"id":2,"type":"command","command":{"command":"refresh_adapter"}}

# 命令执行期间可能先收到新的 state（完整快照）
{"event":"state","snapshot":{...}}

# daemon -> 客户端（命令响应）
{"id":2,"ok":true,"snapshot":{...}}

# 客户端 -> daemon
{"id":3,"type":"shutdown"}

# daemon -> 客户端（先响应，再通知，再关闭）
{"id":3,"ok":true,"snapshot":{...}}
{"event":"state","snapshot":{...}}
{"event":"exit_ready"}
```

## 17. 已知限制

- 仅本机：stdio 或回环 TCP，没有远程/鉴权/TLS。
- 没有请求取消、请求超时或"只订阅某类通知"的过滤；每个连接收到全部通知。
- 一个连接上的请求按顺序处理；慢命令会阻塞该连接后续请求。
- 慢客户端会被移出通知广播（队列 64 条），需要重连重新同步。
- `shutdown` 是全局的；没有"仅关闭我的连接"。
- 目前 daemon 只面向 Windows（项目目标平台）；协议本身与平台无关。
- 命名管道 / Unix socket 尚未实现；若需要，复用同一套信封。

## 18. 代码索引

| 内容 | 位置 |
| --- | --- |
| 信封、行帧、1 MiB 上限 | `crates/drcom-ipc/src/protocol.rs` |
| 服务循环、握手、请求分发 | `crates/drcom-ipc/src/server.rs` |
| 多 TCP 客户端广播 | `crates/drcom-ipc/src/hub.rs` |
| 阻塞客户端（stdio/TCP） | `crates/drcom-ipc/src/client.rs` |
| daemon 参数、stdio/TCP 宿主、tick、退出 | `crates/drcom-daemon/src/main.rs` |
| 示例客户端 | `examples/ipc/client.py`、`examples/ipc/client.ps1` |
| 冻结的契约 | `crates/drcom-service/src/api.rs` |
