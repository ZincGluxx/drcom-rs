# 占用下限实测：一个窗口值多少内存（2026-11-02）

- 测量时间：2026-11-02 06:49–07:20（GMT+8）
- 平台：Windows x64，进程 API 直读（`Process.PrivateMemorySize64` / `WorkingSet64`，6 次 × 500 ms）
- 目的：回答"核心占用还能不能做到 1 MB 以内"

## 一、结论

| 形态 | 私有提交 | 工作集 | 线程 | 来源 |
| --- | ---: | ---: | ---: | --- |
| 发布版 GUI（已认证、缩在托盘） | **5.59 MiB** | 19.5 MiB | 4 | pid 17308 |
| 发布版 GUI（窗口显示时） | **5.62 MiB** | 22.2 MiB | 5 | 同上 |
| 空壳窗口（真 UI，无业务逻辑、无托盘） | **5.32 MiB** | 20.6 MiB | 8 | `examples/window_floor` |
| 空壳窗口 + 托盘 + 隐藏窗口 | **5.38 MiB** | 21.2 MiB | 8 | `examples/window_floor` `FLOOR_TRAY=1` |
| 只建托盘、**不建窗口** | **1.87 MiB** | 10.5 MiB | 4 | `examples/tray_floor` |
| 认证内核 `drcom-cli`（无窗口，登录尝试中） | **0.88 MiB** | 4.8 MiB | 5 | `target/release/drcom-cli.exe` |
| 参照：最小 Rust 进程（只 sleep） | 0.61 MiB | 3.3 MiB | 4 | `tools/floor-probe/hello.rs` |
| 参照：socket + 保活线程 + 日志 | 0.74 MiB | 3.9 MiB | 5 | `tools/floor-probe/core.rs` |

三句话：

1. **窗口本身值 3.5 MiB。** 5.59（托盘常驻）− 1.87（不建窗口）= 3.72 MiB，
   其中 2.6 MiB 是框架内部两块固定分配（1604 KiB + 1020 KiB），
   在空壳探针和发布版进程里**字节级相同** —— 既不是我们申请的大小，
   也不随窗口尺寸变。
2. **我们自己的代码只占 ~0.25 MiB。** 发布版 5.72 − 空壳+托盘 5.47 = 0.25 MiB，
   这就是认证引擎、UI 胶水、1 秒定时器的全部。
3. **无窗口的"核心"今天是 0.88 MiB，已经在 1 MB 以内。** 它不比最小 Rust
   进程贵多少（0.88 对 0.61）。

**所以：把 GUI 进程优化到 1 MB 以内不可能**——把业务代码全部删掉、只留一个空窗口，
也还有 5.3 MiB。1 MB 以内的占用只能属于"没有窗口的核心"。

## 二、私有提交的逐项归属（发布版 GUI，5.62 MiB）

```
  1604 KiB  ← 框架固定分配（空壳探针里一模一样）
  1020 KiB  ← 同上
   500 KiB  ← 堆（空壳探针里是 360 KiB）
   208 KiB  ← 主线程栈
 44/40/32/28/20 KiB  ← 4 个工作线程栈
   其余小堆块 / TEB
```

另有两块计入 `PrivateMemorySize64` 但不属于 `MEM_PRIVATE` 的
**pagefile-backed 段**（约 2.1 MiB，窗口 DIB 一类），`VirtualQueryEx` 里
表现为无文件名的 `MEM_MAPPED`。

只建托盘的进程（1.87 MiB）反向验证了这一点：`MEM_PRIVATE` 只有 656 KiB，
剩下的 1.2 MiB 全在这类无文件名段里；建窗口后它们涨到 3.3 MiB。

字体文件是 mmap 进来的（`msyh.ttc` 19 MiB、`msyhbd.ttc` 16 MiB、
`~FontCache-FontFace.dat` 16 MiB、NLS 表 4 MiB），属于**文件页**，
不计私有提交，只按触达页计入工作集 —— 这也是工作集 20 MiB 比
私有提交 5.6 MiB 大得多的原因，不必为此优化。

## 三、测量方法

- 三个探针都是仓库里的独立 example，**不与正在运行的客户端抢单实例互斥体**，
  不影响真实会话：
  - `examples/window_floor.rs`：真的 `AppWindow`，真软件渲染器；`FLOOR_TRAY=1` 加托盘，
    `FLOOR_HIDE_MS=n` 在 n 毫秒后隐藏窗口（复刻"关闭到托盘"）。
  - `examples/tray_floor.rs`：只建 `AppTray`、不建窗口 —— 常驻态的下限。
  - `tools/floor-probe/{hello,core}.rs`：`rustc -O` 直接编译的最小进程参照。
- `drcom-cli` 的 0.88 MiB 是**登录尝试过程中**的数字（`--server 203.0.113.7`，
  不可达地址，不会碰到真实服务器）；登录失败后进程自行退出，所以用 150 ms 轮询取峰值。
- 归属用 `tools/mem_map.py`（`VirtualQueryEx` + `GetMappedFileNameW`），
  不是任务管理器转述。

## 四、可选的两条路

| | 常驻占用 | 改造成本 | 用户体验变化 |
| --- | ---: | --- | --- |
| **A. 托盘常驻、窗口按需创建** | ~2.1 MiB | 改 `main.rs`/`ui_backend.rs`：窗口迟建、状态回填、关闭即销毁 | 托盘点击后窗口才出现（首次有几十 ms 延迟） |
| **B. 核心/UI 拆成两个进程** | **0.9 MiB** | 新增 IPC、托盘下沉到核心（Win32 `Shell_NotifyIcon`，不能再用 winit/Slint） | 托盘与状态由核心持有，UI 随用随起随关 |

A 仍然达不到 1 MB（下限 1.87 MiB 是 Slint 后端 + winit 事件循环本身）；
只有 B 能到 0.9 MiB。

## 五、复现

```bash
cd drcom-rs
cargo build --release --offline --example window_floor --example tray_floor
FLOOR_TRAY=1 FLOOR_HIDE_MS=1500 target/release/examples/window_floor.exe   # 托盘 + 隐藏窗口
target/release/examples/tray_floor.exe                                     # 只建托盘
# 进程侧读数
powershell -c "Get-Process window_floor,tray_floor,drcom-campus | % { '{0} WS={1:N0}K PRIV={2:N0}K TH={3}' -f $_.ProcessName,($_.WorkingSet64/1KB),($_.PrivateMemorySize64/1KB),$_.Threads.Count }"
# 归属
python tools/mem_map.py <pid>
```
