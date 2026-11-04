# DrCom 校园网助手（Rust 版）Linux 使用说明

> 本文是 Linux 移植版的安装、使用与构建说明。想快速上手，先看
> [install.md](install.md)（一行安装 + 账号配置教程）。Windows 版的完整功能说明、
> 截图与协议测试见项目主文档 [README](https://github.com/ZincGluxx/drcom-rs)。

## 这是什么

DrCom 校园网助手（Rust 版）是吉林大学 Dr.COM 校园网认证客户端的 Rust + Slint 重写版。
本文对应的 **Linux x86_64 构建** 包含两个可执行入口：

- `drcom-campus`：图形界面客户端，带系统托盘与单实例保护。
- `drcom-cli`：命令行客户端，适合脚本、SSH 与无桌面环境。

协议实现、配置格式与命令行参数与 Windows 版一致：完整走
challenge → login → keep-alive → logout 流程，逐字节对应社区 B 世代 Python 参考实现。
Linux 版替换的是平台相关部分——网卡枚举、认证套接字的出接口绑定、路由管理、
凭据存储、开机自启、剪贴板、网络诊断与静态 IPv4 设置。

发布包自带两个二进制，不需要系统安装 Rust。

## 系统要求

| 项目 | 要求 |
| --- | --- |
| 系统与架构 | x86_64 Linux，glibc 动态链接。发布包按构建机的 glibc 版本链接；系统过旧会提示 `GLIBC_x.xx not found`，请改为从源码构建 |
| 图形界面 | 可用的 X11 或 Wayland 桌面会话；纯 SSH / 容器 / 服务器环境请使用 `drcom-cli` |
| 必需命令 | `ip`（iproute2）、`curl`、`ping`（iputils）。认证本身不需要 root 权限 |
| 推荐命令 | `wl-clipboard`（Wayland）或 `xclip` / `xsel`（X11），用于图形界面里的“复制检测结果”；`xdg-open` 用于打开诊断文件所在目录 |
| 可选组件 | NetworkManager（`nmcli`）用于界面里的静态 IPv4 设置（修改时由 polkit 授权）；polkit 也用于授权 NetworkManager 的网络修改 |
| 特权操作 | 添加认证服务器 /32 主机路由（`--auth-route-apply` 或配置 `auth_route = manage`）需要 root / sudo |
| 从源码构建 | Rust 工具链（rustup，edition 2024，建议 1.85+）与 C 链接器（gcc / clang）；不需要 Windows SDK |

常见发行版安装依赖（Arch 示例）：

```bash
sudo pacman -S --needed iproute2 curl iputils
# 可选
sudo pacman -S --needed wl-clipboard networkmanager
```

Debian / Ubuntu 对应包名为 `iproute2`、`curl`、`iputils-ping`、`wl-clipboard`、
`network-manager`。GNOME 还需要托盘扩展，见下文“系统托盘”。

## 安装

### 一行命令安装（推荐）

能直连 GitHub 时：

```bash
curl -fsSL https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
```

国内网络走镜像（脚本和发布包都会自动尝试国内镜像）：

```bash
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
```

需要顺便启用**静默开机自启**（登录桌面后不弹窗、后台自动认证）：

```bash
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash -s -- --autostart
```

安装脚本把程序装到当前用户目录，不需要 root，可重复执行覆盖升级：

- `~/.local/bin/drcom-campus`、`~/.local/bin/drcom-cli`
- `~/.local/share/icons/hicolor/128x128/apps/drcom-campus.png`
- `~/.local/share/applications/drcom-campus.desktop`（应用菜单中的“DrCom 校园网助手（Rust）”）

设置了 `XDG_DATA_HOME` / `XDG_BIN_HOME` 时，会放到对应目录下。默认**不会**新增开机自启，
已存在的自启条目会被升级为 `--silent` 静默启动。安装脚本会自动把 `~/.local/bin`
写入已存在的 `~/.bashrc` / `~/.zshrc`（幂等，重新打开终端或 `source ~/.bashrc`
后生效；不想改 shell 配置可加 `--no-modify-path`），否则终端里会提示
`drcom-cli: 未找到命令`：

```bash
export PATH="$HOME/.local/bin:$PATH"
```

其它选项：`--version <版本>` 指定版本、`--bin-dir <目录>` 自定义安装目录、
`--uninstall` 卸载（保留账号配置）、`--no-mirror` 只直连 GitHub。完整说明见
[docs/install.md](install.md)。

### 手动下载发布包

发布包文件名形如 `drcom-campus-linux-x86_64-v1.2.7.tar.gz`。从
<https://github.com/ZincGluxx/drcom-rs/releases/latest> 下载后（国内可用
`https://ghproxy.net/https://github.com/ZincGluxx/drcom-rs/releases/latest`）：

```bash
# 校验（可选，在压缩包所在目录执行）
sha256sum -c drcom-campus-linux-x86_64-v1.2.7.tar.gz.sha256

tar xzf drcom-campus-linux-x86_64-v1.2.7.tar.gz
cd drcom-campus-linux-x86_64-v1.2.7
./install.sh              # 只安装程序
./install.sh --autostart  # 同时启用静默开机自启
```

也可以不安装，直接运行解压目录里的 `./drcom-campus` / `./drcom-cli`。

### 从源码构建

需要 Rust 工具链（[rustup](https://rustup.rs/)）：

```bash
git clone https://github.com/ZincGluxx/drcom-rs.git
cd drcom-rs
cargo build --release --offline   # 首次构建尚未缓存依赖时去掉 --offline
cargo test --offline
./target/release/drcom-campus     # 图形界面
./target/release/drcom-cli --help # 命令行
```

静态检查与打包：

```bash
cargo clippy --offline --all-targets -- -D warnings
cargo fmt --check
bash scripts/package-linux.sh     # 产出 dist/ 下的发布包
```

构建只需要 Rust 工具链与 C 链接器；Windows 专用的图标 / 版本资源处理在 Linux 上自动跳过，
因此 ELF 可执行文件里没有 Windows 版本资源。

## 图形界面

从应用菜单启动“DrCom 校园网助手（Rust）”，或直接运行 `drcom-campus`。首次启动在窗口里
填写校园网账号密码即可连接；认证服务器默认 `10.100.61.3`，可在 `drcom.ini` 里修改。

- **单实例**：程序已在运行时再次启动，会把已运行的窗口唤到前台（Linux 上通过
  `$XDG_RUNTIME_DIR` 下的 Unix 域套接字实现）。
- **自绘标题栏**：窗口不使用系统原生标题栏，标题栏由程序绘制：拖动标题栏移动窗口；
  左侧显示应用名，右侧 `—` 最小化（有托盘 → 托盘，无托盘 → 任务栏）、`×` 关闭
  （按下面的“关闭行为”处理）。
- **关闭行为**：勾选“关闭到托盘”（没有托盘时显示为“关闭后静默运行”，默认开启）时，
  关闭按钮让程序**在后台静默运行**：有托盘时窗口隐藏到托盘，托盘菜单可唤回或退出；
  没有托盘时窗口最小化到任务栏，点击任务栏图标或再次启动本程序即可唤回。
  取消勾选后关闭按钮直接退出。
- **静默启动**：`drcom-campus --silent` 启动后不显示窗口，按已保存的账号与“自动登录”设置
  在后台完成认证并持续保活；之后再次运行 `drcom-campus` 会唤出窗口。
- **认证网卡记忆**：登录成功后记住网卡（`adapter.json`），重启和重连优先使用；
  失效后自动探测其他物理网卡。
- **开机自启**：默认关闭。在界面中勾选“开机自启”后，会在
  `~/.config/autostart/drcom-campus.desktop` 写入自启条目，指向当前运行的可执行文件；
  换了安装位置后需要重新勾选一次。
- **配色**：启动时向 GNOME（`gsettings`）或 KDE（`kreadconfig`）查询一次桌面明暗配色；
  需要固定配色时用 `DRCOM_THEME=light` 或 `DRCOM_THEME=dark` 覆盖。
- **诊断结果**：“复制”需要 `wl-copy`、`xclip` 或 `xsel`；“保存”会把结果写入数据目录，
  并用 `xdg-open` 打开所在目录。

## 命令行

`drcom-cli` 与图形界面共用同一套认证引擎，默认只输出连接阶段与错误，`--verbose` 临时
打印协议细节。完整参数见：

```bash
drcom-cli --help
```

常用示例：

```bash
# 交互式保活：认证成功后持续在线，按回车（或输入 q 回车）停止并下线
drcom-cli --user 20230001 --pass 'secret'

# 密码从环境变量读取，避免进入 shell 历史
DRCOM_PASSWORD=secret drcom-cli --user 20230001

# 只认证一次，成功后立即下线（脚本 / 自检）
drcom-cli --user 20230001 --pass secret --once

# 保活 3600 秒后自动下线
drcom-cli --user 20230001 --pass secret --duration 3600

# 打印详细协议信息
drcom-cli --user 20230001 --pass secret --verbose

# 打印认证服务器 /32 路由计划（不修改路由表；计划属于详细输出，需要 --verbose）
drcom-cli --user 20230001 --pass secret --verbose --auth-route

# 登录前添加认证服务器 /32 主机路由，下线时移除（需要 root）
sudo drcom-cli --user 20230001 --pass secret --auth-route-apply
```

`--auth-route` 只打印计划、随后仍会继续认证；计划属于详细输出，默认的 `--quiet` 只显示阶段与
错误，需要加 `--verbose` 才能看到。真正修改路由表要用 `--auth-route-apply`（需要 root），
或在 `drcom.ini` 里设置 `auth_route = manage`。`--once` 会忽略路由相关选项。

其他常用参数：`--server`（可写多个地址）、`--config`（指定配置文件）、`--local-ip`、
`--mac`、`--port`、`--local-port`、`--dns`、`--dhcp`、`--hostname`、`--quiet`。

> **在脚本或服务中使用**：`drcom-cli` 把标准输入读到 EOF 视为停止请求，因此从
> `/dev/null` 重定向（systemd 的默认行为）或在没有输入的管道中运行会立即下线。
> 一次性任务请用 `--once`；需要持续保活时请保持标准输入打开，例如
> `tail -f /dev/null | drcom-cli ...`。

## 静默后台运行

### 图形界面

登录时不想看到窗口，用 `--silent` 启动图形界面：

```bash
drcom-campus --silent          # 或 DRCOM_SILENT=1 drcom-campus
```

前提是在窗口里保存过账号并勾选“自动登录”。程序启动后不显示主窗口，按保存的凭据完成
认证并在后台持续保活；窗口一直隐藏也没关系，**再次运行 `drcom-campus` 会把已有实例的
窗口唤出**（单实例机制）。

在界面中勾选“开机启动”后，写入的 autostart 条目会自动带 `--silent`，因此登录桌面时
不会弹窗，而是直接在后台登录。关闭窗口时程序继续在后台静默运行：

- 有托盘（KDE、装了 AppIndicator 扩展的 GNOME 等）：窗口隐藏到托盘，托盘菜单可唤回/退出；
  点击标题栏的 `—` 同样最小化到托盘。
- 没有托盘（例如未装扩展的 GNOME）：窗口最小化到任务栏，点击任务栏图标或再次启动
  `drcom-campus` 即可唤回。
- 彻底退出：唤回窗口后取消勾选“关闭后静默运行”，再点关闭按钮，或从托盘菜单退出。

### 命令行

`drcom-cli` 在终端里运行时按回车即可停止；在 systemd、`nohup` 等没有标准输入的环境
中，stdin 的 EOF 会被视为停止信号（见上文“命令行”的说明），因此持续后台保活需要用
`--duration 0` 并保持 stdin 打开，例如：

```bash
nohup sh -c 'tail -f /dev/null | drcom-cli --user 20230001 --pass secret' \
    >/dev/null 2>&1 &
```

需要长期后台运行时，更推荐用带桌面的图形版 `--silent`（自动登录 + 保活），或自行把
上面的命令包成 systemd 用户服务并保持标准输入打开。

## 配置文件

与 Windows 版使用同一种 INI 格式（模板见 [drcom.ini.example](../drcom.ini.example)，
即发行包根目录的 `drcom.ini.example`）。按以下顺序查找第一个存在的文件：

1. `drcom-campus` / `drcom-cli` 可执行文件所在目录的 `drcom.ini`
2. 当前工作目录的 `drcom.ini`
3. `$XDG_CONFIG_HOME/drcom-campus/drcom.ini`（默认 `~/.config/drcom-campus/drcom.ini`）

全部键都可以省略：认证服务器有内置默认值，账号密码也可以只在界面里输入（推荐，避免明文
落盘）。用 `--config` 可以显式指定文件；`drcom-cli` 找到配置文件时会打印实际读取的路径。

注意：`drcom.ini` 里填写的密码是明文。建议把文件设为 `chmod 600`，或干脆不在文件里写密码。

## 数据与凭据

数据目录：`$XDG_DATA_HOME/drcom-campus`（默认 `~/.local/share/drcom-campus`），包含：

| 文件 | 内容 |
| --- | --- |
| `preferences.dat` | 账号、密码与自动登录 / 自动重连 / 关闭到托盘等设置 |
| `adapter.json` | 最近一次成功认证的网卡（用于优先选择） |
| `drcom-诊断-*.txt` | 手动保存的诊断结果 |

日常运行不写日志文件；内存中的诊断摘要退出后清空。

**与 Windows 的凭据保护不同**：Linux 没有 DPAPI。本程序把设置保存为 JSON 明文，但会以
`0600` 权限创建文件、`0700` 权限创建目录，只有当前用户可读。能读取你用户文件（或被提权）
的进程仍可读到密码——需要更强保护时请不要在界面中保存密码，并确保磁盘加密。

## 与 Windows 版的差异

| 功能 | Windows 版 | Linux 版 |
| --- | --- | --- |
| 托盘图标 | 系统通知区域 | StatusNotifierItem（KDE / XFCE 等原生支持；GNOME 需扩展） |
| 关闭到托盘 | 隐藏窗口、托盘常驻 | 同左；没有可用托盘时关闭后仍在后台静默保活，再次启动唤出窗口 |
| 凭据保护 | DPAPI 加密 | 明文 JSON + `0600` 文件权限 |
| 开机自启 | 注册表 `Run` 项 | `~/.config/autostart/drcom-campus.desktop` |
| 配置搜索位置 | exe 目录、工作目录、`%APPDATA%\DrComCampus` | exe 目录、工作目录、`~/.config/drcom-campus`（XDG） |
| 数据目录 | `%LOCALAPPDATA%\DrComCampusRust` | `~/.local/share/drcom-campus`（XDG） |
| 认证服务器路由 | `route.exe` / IP Helper API | `ip route add/del`；需要 root 或 CAP_NET_ADMIN |
| 网络诊断 | 系统 `curl.exe` / `ping.exe` | PATH 中的 `curl` / `ping` |
| 剪贴板 | 系统剪贴板 API | `wl-copy` / `xclip` / `xsel`，未安装时提示 |
| 静态 IPv4 | `netsh` + PowerShell 提权 | NetworkManager（`nmcli`）+ polkit 授权 |
| 单实例 | 命名互斥体 + 消息 | `$XDG_RUNTIME_DIR` 下的 Unix 域套接字 |
| 出接口绑定 | `IP_UNICAST_IF` + 接口索引 | `SO_BINDTOIFINDEX`；无权限时退回源地址绑定 |
| 版本信息 | 可执行文件带图标 / 版本资源 | ELF 无 Windows 资源，图标随包分发 |

## 系统托盘

Linux 桌面没有 Windows 那种统一的通知区域，程序通过 **StatusNotifierItem**（KDE 与多数
面板原生支持）提供托盘图标。GNOME 默认不显示 StatusNotifierItem，需要安装并启用扩展：

```bash
# Arch
sudo pacman -S --needed gnome-shell-extension-appindicator
# Debian / Ubuntu / Fedora 的包名通常也是 gnome-shell-extension-appindicator
```

安装后在“扩展”中启用 “AppIndicator and KStatusNotifierItem Support”，必要时注销并重新登录。

有托盘时关闭窗口会**隐藏到托盘**：dock / 任务栏里不会留下窗口项，只保留托盘图标；
从托盘菜单“显示主窗口”或再次启动程序即可唤回窗口。没有托盘时退化为最小化到任务栏并
继续保活。想彻底退出，从托盘菜单选“退出程序”，或先唤回窗口、取消勾选
“关闭后静默运行”再点关闭按钮。

v1.2.7 起窗口的 Wayland app_id / X11 WM_CLASS 固定为 `drcom-campus`，与
`drcom-campus.desktop` 一致，GNOME 能正确识别窗口、托盘与 dock 条目，不会再出现
“关不掉、打不开”的幽灵窗口。启动时程序会先询问会话总线是否存在
`org.kde.StatusNotifierWatcher`；没有时直接按无托盘方式运行，不再向终端打印告警。

## 打包发布

仓库自带打包脚本，在仓库根目录执行：

```bash
bash scripts/package-linux.sh
```

脚本会：

1. 用 `cargo build --release` 构建两个二进制（优先 `--offline`，缺依赖时自动联网重试）；
2. 用 `file` 校验两个二进制都是 x86-64 ELF 可执行文件；
3. 在 `dist/drcom-campus-linux-x86_64-v<版本>/` 生成暂存目录（二进制、`drcom.ini.example`、
   `LICENSE`、`docs/linux.md`、`install.sh`、桌面入口、图标与 `RELEASE-NOTES.txt`）；
4. 产出 `dist/drcom-campus-linux-x86_64-v<版本>.tar.gz` 与同名 `.sha256`，并打印路径、
   大小与校验和。

校验与安装：

```bash
sha256sum -c dist/drcom-campus-linux-x86_64-v1.0.4.tar.gz.sha256
tar xzf dist/drcom-campus-linux-x86_64-v1.0.4.tar.gz -C /tmp
cd /tmp/drcom-campus-linux-x86_64-v1.0.4 && ./install.sh
```

版本号取自 `Cargo.toml` 的 `[package]` 段，不需要手动同步。发布包只面向 x86_64，
在其它架构上脚本会直接报错。

## 已知限制

- Linux 移植版沿用与 Windows 版相同的协议实现，但尚未像 Windows 版那样经过长期真实环境
  实测；首个版本请留意认证与保活表现。
- GNOME 需要 AppIndicator 扩展才能显示托盘图标（见上文）；没有托盘时关闭窗口会最小化到
  任务栏并在后台保活，点击任务栏图标或再次启动程序即可唤回。
- 非特权运行时无法用 `SO_BINDTOIFINDEX` 真正绑定出接口（需要 `CAP_NET_RAW`），程序会
  退回“绑定网卡源 IPv4”的方式；多网卡且路由异常时，请在界面里手动选择网卡，或用
  `--local-ip` / `drcom.ini` 指定。
- 添加认证服务器主机路由需要 root / sudo；界面内自动添加的要求相同。
- 静态 IPv4 设置只支持 NetworkManager（`nmcli`），修改时会请求 polkit 授权；其它网络
  管理方式请手动设置。
- 发布包按构建机的 glibc 链接，较旧发行版可能无法运行；此时请从源码构建。
- 复制诊断结果需要 `wl-clipboard` 或 `xclip` / `xsel`，未安装时程序会提示。
- 需要 X11 或 Wayland 桌面会话才能显示图形界面；无桌面环境请用 `drcom-cli`。
- 只提供 tar.gz 发布包，暂无 deb / rpm / Flatpak / AUR 包。
- `drcom-cli` 把 stdin EOF 视为停止信号，在 systemd 等无输入环境下不会自动持续保活
  （见上文“命令行”）。

## 故障排查

- **界面打不开**：确认处于桌面会话（`echo $DISPLAY` / `$WAYLAND_DISPLAY`），在终端运行
  `drcom-campus` 查看错误输出。
- **托盘图标不显示**：GNOME 用户检查 AppIndicator 扩展是否安装并启用；其它桌面确认面板
  支持 StatusNotifierItem。程序启动时会在会话总线上查找 `org.kde.StatusNotifierWatcher`，
  找不到就按无托盘方式运行（关闭窗口改为最小化到任务栏并后台保活）。若仍看到
  `Slint: Failed to create system tray icon`，说明扩展已注册但未生效，注销重新登录即可。
- **认证失败**：先用 `drcom-cli --verbose` 观察阶段输出；确认认证服务器地址与账号正确，
  且网卡已获取校园网 IPv4（`ip -j addr`、`ip route`）。
- **复制检测结果失败**：安装 `wl-clipboard` 或 `xclip`。
- **路由操作提示权限不足**：用 `sudo drcom-cli --auth-route-apply`，或先不启用 `auth_route`。
- **提示 `GLIBC_2.xx not found`**：在目标系统上从源码构建。
- **静态 IPv4 修改失败**：确认使用 NetworkManager（`nmcli general status`），并按桌面
  提示完成授权。

## 许可

本项目以 [GNU AGPL v3.0](../LICENSE)（GNU Affero 通用公共许可证第 3 版）授权。
使用、修改、分享时请保留作者署名（ZincGluxx）与 [NOTICE](../NOTICE) 中的上游归属说明；
仅限在你本人有合法账号的校园网内使用。旧版本（CC BY-NC 4.0 时期发布的版本）仍适用其
当时的许可条款。
