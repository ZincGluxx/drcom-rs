# Windows / macOS / Linux 支持说明

## 分支与接入来源

- 本次在独立 `macos` 分支实现，从当前前后端拆分 workspace (`e207997`) 开始；不重写 `main` / `windows` / `linux`。
- 已有 Linux 来源：`origin/linux` 的 `9735565e89c460b5bbf6e62041eb7095200e0ab2`。
- 复用其 sysfs + `ip -j` 网卡枚举、路由 JSON 解析、nmcli 静态设置、Unix 单实例唤醒、GNOME/KDE 配色探测、无托盘时最小化、`--silent` 与一行安装布局；适配到 `drcom-core` / `drcom-service` / `drcom-gui`。
- 安全差异：旧 Linux 的 0600 明文密码改为系统 Secret Service；网卡绑定权限失败不再静默降级为仅源地址绑定；已存在路由不作为本客户端新建路由清理。

## 平台能力

| 能力 | Windows | macOS | Linux |
| --- | --- | --- | --- |
| GUI / CLI / daemon | x64 | Apple Silicon / Intel | x64 |
| 物理网卡 | IP Helper / GUID | hardware ports + getifaddrs / BSD 名 | iproute2 + sysfs 设备 / 接口名 |
| 认证出口绑定 | IP_UNICAST_IF | IP_BOUND_IF | SO_BINDTODEVICE |
| 凭据 | DPAPI | 登录 Keychain | Secret Service |
| 下次登录自启 | HKCU Run | 用户 LaunchAgent | XDG autostart |
| 静态 IPv4 编辑 | netsh + UAC | 打开系统网络设置 | NetworkManager / polkit |
| 关闭到后台 | 托盘释放窗口 | 最小化保留 Dock 原生窗口 | 最小化保留任务栏/Dock |
| 剪贴板 | Win32 | pbcopy | wl-copy / xclip / xsel |

协议帧沿用已验证的校园部署，不因操作系统变更擅改协议字段。新的 macOS/Linux 环境仍需在具有合法账号的校园网内验证认证、长时保活、断网恢复和睡眠唤醒。

## Windows

使用安装 EXE 或解压 portable ZIP；三个程序分别是 GUI、CLI 和 IPC 后端，不默认安装系统服务。保持现有 DPAPI 文件、单实例与托盘行为。安装/卸载不会删除用户账号配置。

## macOS

1. 按 CPU 下载 `macos-aarch64`（M 系列）或 `macos-x86_64`（Intel）的 DMG。
2. 将 `DrCom Campus.app` 拖入 Applications，再启动和保存自启，避免 LaunchAgent 指向磁盘映像或临时目录。
3. 下载包为 ad-hoc 签名，**没有 Developer ID 签名、没有 Apple 公证**；Gatekeeper 可能阻止首次打开。核验来源和 SHA256 后按系统提示在“隐私与安全性”单独允许；不要全局关闭 Gatekeeper。
4. 密码存于登录钥匙串，非敏感选项位于 `~/Library/Application Support/DrComCampusRust/preferences.dat`。钥匙串锁定/授权被拒会明确报错；仍可手动输入密码登录。
5. 自启条目位于 `~/Library/LaunchAgents/io.github.drcom-campus-rust.plist`，只影响下一次登录，不立即拉起第二个进程。

CLI 示例：

```sh
'/Applications/DrCom Campus.app/Contents/MacOS/drcom-cli' --help
'/Applications/DrCom Campus.app/Contents/MacOS/drcom-daemon' --help
```

应用内静态 IP 编辑不支持 macOS，网络设置按钮打开系统网络设置；这是显式能力差异，不是假成功。

关闭到后台和静默启动会保留最小化原生窗口，避免 Finder 对已运行应用发送 Reopen 但进程没有新启动时无法恢复。菜单栏“显示主窗口”和再次执行二进制都走明确恢复路径。退出时建议使用应用菜单栏图标里的“退出程序”，它会先发下线；**Cmd-Q / Dock Quit 的系统直接终止尚不能保证走服务下线流程**，需原生实机验收，强制结束后服务器可能等待心跳超时。此版本不替换或修改 winit 的内部 AppKit delegate。

## Linux

### 依赖和安装

复用已有 Linux 包布局：`drcom-campus-linux-x86_64-v<VERSION>.tar.gz`，根部保留 GUI / CLI / installer，新增 daemon 与全部许可声明。支持 Ubuntu/Debian DEB，其他 glibc 桌面使用 tar 包；**不是静态二进制，也不是 AppImage**。

- 必需枚举工具：iproute2（`ip`）、sysfs；物理接口需要 backing device，不把 VPN / loopback / bridge 冒充校园物理网卡。
- GUI：GTK3、Ayatana AppIndicator、X11/Wayland、xkbcommon、D-Bus、libxdo、字体。推荐安装 `fonts-noto-cjk` 以显示中文。
- 保存密码：有登录会话 D-Bus 和已解锁的 Secret Service（如 GNOME Keyring；KWallet 需兼容 Secret Service）。不可用时保存明确失败，不写明文。
- 诊断：curl、ping；网络编辑：nmcli + 可用的 NetworkManager/polkit；剪贴板：Wayland `wl-clipboard`，X11 `xclip` / `xsel`。
- GNOME 默认没有托盘宿主也能打开窗口；可选 AppIndicator 扩展显示托盘。关闭到后台保留 Dock 可恢复入口，不依赖不存在的托盘。

解压后安装：

```sh
cd drcom-campus-linux-x86_64-v<VERSION>
bash install.sh
```

安装到当前用户目录，不需要 sudo；不启用系统 daemon，不清除账号。若需要一行下载入口，在本分支推送后可下载根 `install.sh`，**先检查脚本再执行**；默认取最新 Release，可传 `--version` 固定版本，强制验证 tar 包 SHA256。旧 linux 分支的下载入口可用 `--version` 取得同名新包（旧入口只安装 GUI/CLI）。

### 网卡绑定权限

新版本不会吞掉 SO_BINDTODEVICE 的 EPERM/EACCES；否则 Linux 弱主机路由可能让凭据从另一接口出去。某些内核/安全策略允许普通用户首次绑定，某些需要 `CAP_NET_RAW`。如果收到明确的绑定权限错误，请由管理员按本机策略，仅对**可信并已验证**的需要联网程序赋予能力，不要 sudo 启动整个桌面 GUI：

```sh
# 按实际安装位置选择需要授权的二进制；不能照抄给不可信文件
sudo setcap cap_net_raw=ep /absolute/path/to/drcom-campus
# 若使用 CLI / daemon，分别授予对应程序；更新二进制后能力可能需重新授予
getcap /absolute/path/to/drcom-campus
# 撤销：sudo setcap -r /absolute/path/to/drcom-campus
```

设置文件能力还可能被挂载选项、容器或桌面安全策略禁止；此时应让管理员调整策略，而不是禁用出口校验。可选的主机路由管理另外需要路由修改权限，默认关闭，不自动提权。

### 旧 Linux 偏好迁移

旧分支可能在 `$XDG_DATA_HOME/drcom-campus/preferences.dat`（默认 `~/.local/share/drcom-campus/preferences.dat`）保存明文密码。
新实现可读取用户名/非敏感选项，但**不会自动使用或导入旧明文密码**，会提示重新输入并安全保存；新位置是 `$XDG_CONFIG_HOME/DrComCampusRust`（默认 `~/.config/DrComCampusRust`）。旧文件不会擅自删除，请在确认新设置已安全保存后自行移除旧明文文件。旧 `~/.config/drcom-campus/drcom.ini` 仍被搜索。

## 配置、状态与诊断

配置查找保持“可执行文件旁 → 工作目录 → 平台用户目录”。INI 由用户自己管理，若在其中填写密码仍是用户主动创建的明文配置，推荐留空。密钥环失败只恢复非敏感选项并关闭自动登录，不把失败当作成功。

IPC v1 的 `start_with_windows` 字段为了现有前端兼容保留原名字，语义现为当前平台的登录自启状态，不是宣称 macOS/Linux 使用 Windows 注册表。

## Actions 与发布验收

完整原生工具和命令见 [packaging/BUILDING.md](../packaging/BUILDING.md)。

1. 将 `macos` 分支提交并推送后自动原生测试和打包出 Artifacts；也可在 Actions 手动运行。分支推送与手动运行都不创建 Release。
2. 确认 Windows x64、macOS arm64/x64、Linux x64 四个原生 CI 作业的格式、Clippy、tests、打包都通过。
3. 在校内三端实机验收：连接/下线、拔网重连、无托盘桌面恢复窗口、账号保存/清除、钥匙串锁定错误、自启、静态设置授权取消。
4. 选择未使用的新版本，更新 workspace version 与 Cargo.lock；推送精确 `v<version>` 标签自动发布全部 8 个资产和 SHA256。不要覆盖旧标签或旧 Release。

本机 Windows 的交叉 `cargo check` 只能证明 Rust 类型/平台分支可编译，不能证明 Mac 原生链接、DMG、公证或 Linux 包运行成功。原生 CI 与校园网实测结果应分别记录，不得混为已验证。
