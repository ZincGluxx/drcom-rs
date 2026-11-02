# DrCom 校园网助手（Rust 版）Linux 安装与配置教程

本教程面向 Linux x86_64 用户，三步完成：**一行命令安装 → 打开界面填账号 → 勾选自动登录 / 开机启动**。
更完整的平台细节、构建与排障见 [linux.md](linux.md)。

## 1. 系统要求

| 项目 | 要求 |
| --- | --- |
| 系统 | x86_64 GNU/Linux（glibc 动态链接） |
| 桌面 | X11 或 Wayland（图形界面需要；纯服务器请用 `drcom-cli`） |
| 必需命令 | `curl`、`ip`（iproute2）、`ping`（iputils） |
| 可选组件 | `wl-clipboard` / `xclip`（复制诊断结果）、NetworkManager（界面内改静态 IPv4） |

Arch：

```bash
sudo pacman -S --needed curl iproute2 iputils
```

Debian / Ubuntu：

```bash
sudo apt install curl iproute2 iputils-ping
```

## 2. 一行命令安装

能直连 GitHub：

```bash
curl -fsSL https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
```

国内网络（脚本与发布包都会走国内镜像，失败会自动换下一个镜像）：

```bash
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
```

想顺带启用**静默开机自启**（登录桌面不弹窗、后台自动认证）：

```bash
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash -s -- --autostart
```

安装内容（全部在用户目录，不需要 root，可重复执行覆盖升级）：

- `~/.local/bin/drcom-campus`：图形界面
- `~/.local/bin/drcom-cli`：命令行客户端
- `~/.local/share/applications/drcom-campus.desktop`：应用菜单入口
- `~/.local/share/icons/hicolor/128x128/apps/drcom-campus.png`：图标

安装脚本支持的其它参数：

| 参数 | 作用 |
| --- | --- |
| `--version <版本>` | 安装指定版本（默认最新发布版本） |
| `--autostart` / `--no-autostart` | 启用 / 不启用静默开机自启 |
| `--bin-dir <目录>` | 自定义二进制安装目录（默认 `~/.local/bin`） |
| `--mirror <前缀>` | 额外优先尝试的下载镜像 |
| `--no-mirror` | 只直连 github.com |
| `--uninstall` | 卸载程序与桌面入口（保留账号配置） |
| `--no-modify-path` | 不自动修改 shell 配置（默认会把 `~/.local/bin` 写入已存在的 `~/.bashrc` / `~/.zshrc`） |

安装后脚本会自动把 `~/.local/bin` 加入 PATH（修改已存在的 `~/.bashrc` / `~/.zshrc`，
带幂等标记，不会重复添加）；**重新打开终端或执行 `source ~/.bashrc` 后生效**，
之后即可直接运行 `drcom-cli` / `drcom-campus`。如果不想让安装脚本改 shell 配置，
加 `--no-modify-path`，然后手动把下面一行加入对应 rc 文件：

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## 3. 首次配置（图形界面，约 30 秒）

1. 从应用菜单打开 **DrCom 校园网助手（Rust）**，或运行 `drcom-campus`。
2. 填写校园网**账号**和**密码**。
3. 按需勾选：
   - **自动登录**：启动时用保存的账号密码自动认证；
   - **自动重连**：掉线后自动恢复（默认开）；
   - **开机启动**：登录桌面时自动运行（配合 `--silent` 即静默登录）；
   - **关闭到托盘** / **关闭后静默运行**：关闭窗口后最小化到托盘/任务栏并在后台保活。
4. 点 **保存设置**。看到“网络概览”里有网卡、IPv4、网关即表示环境就绪，点 **连接** 即可上网。

> 配置文件位置（可选）：`~/.config/drcom-campus/drcom.ini`。不写这个文件也能用，
> 认证服务器默认 `10.100.61.3`。模板见发行包里的 `drcom.ini.example`。

### 静默后台运行

- 勾选“自动登录”后，用 `drcom-campus --silent` 启动：不显示窗口，直接在后台认证并保活。
- 开机自启条目默认带 `--silent`，登录桌面自动静默认证。
- 想唤出窗口：点击任务栏里的程序图标，或再次运行 `drcom-campus`（单实例会唤醒已有窗口）。
- 想彻底退出：唤出窗口后取消“关闭后静默运行”，再点关闭按钮。

## 4. 命令行用法

```bash
# 交互式保活：认证成功后持续在线，按回车停止并下线
drcom-cli --user 20230001 --pass 'secret'

# 密码用环境变量传入，避免进入 shell 历史
DRCOM_PASSWORD=secret drcom-cli --user 20230001

# 只认证一次后立即下线（脚本自检）
drcom-cli --user 20230001 --pass secret --once

# 保活 1 小时后下线
drcom-cli --user 20230001 --pass secret --duration 3600

# 查看认证服务器 /32 路由计划（不修改路由表）
drcom-cli --user 20230001 --pass secret --verbose --auth-route

# 应用 /32 主机路由（需要 root）
sudo drcom-cli --user 20230001 --pass secret --auth-route-apply
```

## 5. 验证、升级与卸载

```bash
drcom-campus --help     # 图形界面参数
drcom-cli --help        # 命令行参数
```

- **升级**：重新执行第 2 步的一行命令即可覆盖安装到最新版。
- **卸载**：重新执行一行命令并加 `--uninstall`，例如：

```bash
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash -s -- --uninstall
```

账号与偏好保存在 `~/.local/share/drcom-campus`（文件权限 0600），卸载不会删除；
需要彻底清除时手动 `rm -rf ~/.local/share/drcom-campus`。

## 6. 国内下载说明

安装脚本会按顺序尝试：GitHub 直连 → `ghproxy.net` → `gh-proxy.com` →
`ghfast.top` → `github.moeyy.xyz` → `mirror.ghproxy.com`，任一成功即停止，
并用发布包 `.sha256` 校验。镜像只负责传输，发布包本身带 SHA256 校验。

- 已经有代理：直接直连 GitHub 即可，脚本会走系统代理环境变量。
- 自建镜像 / 内网分发：把 `drcom-campus-linux-x86_64-v<版本>.tar.gz` 与
  `.sha256` 放到任意 HTTP 目录，设置 `DRCOM_INSTALL_BASE_URL`：

```bash
DRCOM_INSTALL_BASE_URL=https://your-mirror/drcom bash install.sh
```

## 7. 常见问题

| 现象 | 处理 |
| --- | --- |
| 安装脚本下载失败 | 换第 6 节的镜像；或手动从 Releases 下载后执行包内 `install.sh` |
| `GLIBC_2.xx not found` | 发布包按构建机 glibc 链接；在该系统上从源码构建（见 [linux.md](linux.md)） |
| 托盘图标不显示（GNOME） | 安装并启用 AppIndicator 扩展；没有托盘时关闭窗口会最小化到任务栏并在后台保活 |
| 关闭后任务栏出现点不开的窗口 | v1.1.0 在 Wayland 下隐藏窗口会留下失效的 dock 项；升级到 v1.1.1 后关闭改为最小化，点击任务栏图标即可打开 |
| 界面打不开 | 确认处于桌面会话（`echo $DISPLAY` / `$WAYLAND_DISPLAY`）；无桌面请用 `drcom-cli` |
| 认证失败 | 用 `drcom-cli --verbose` 看阶段输出；确认账号、服务器地址与网卡 IPv4 |
| 路由操作权限不足 | 用 `sudo drcom-cli --auth-route-apply`，或保持默认 `auth_route = off` |
