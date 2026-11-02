# v1.1.0 — Linux 移植版、静默后台运行与一键安装

首个 **Linux x86_64 正式构建**，与 Windows 版共用同一套 Dr.COM 认证引擎与 Slint 界面。

## 🐧 Linux 支持

- 完整认证 / 保活协议：challenge → login → keep-alive → logout，与 Windows 版逐字节一致。
- 平台层移植：网卡枚举（`ip` + sysfs）、出接口绑定（SO_BINDTOIFINDEX）、主机路由
  （`ip route`）、系统托盘（StatusNotifierItem）、单实例（Unix 域套接字）、
  开机自启（XDG autostart）、本地时间、服务器 GBK 中文消息、静态 IPv4（NetworkManager）
  与 XDG 配置/数据目录。
- 桌面集成：无托盘桌面（如未装扩展的 GNOME）自动降级；启动时读取 GNOME / KDE 明暗配色。
- 无托盘时关闭窗口不再退出，而是**后台静默保活**，再次启动程序即可唤出窗口。

## 🔇 静默后台运行

- `drcom-campus --silent`（或 `DRCOM_SILENT=1`）：不显示窗口，按保存的账号与“自动登录”
  设置直接后台认证保活；再次运行程序唤出窗口。
- 开机自启条目默认带 `--silent`：登录桌面不弹窗，直接静默登录。
- 启动前先探测会话总线是否有 StatusNotifierWatcher，没有托盘时不再打印失败告警。

## ⚡ 一行安装

```bash
# 直连 GitHub
curl -fsSL https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash

# 国内网络（脚本与发布包走镜像）
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash

# 安装 + 静默开机自启
curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash -s -- --autostart
```

安装到当前用户目录（`~/.local`），不需要 root，可重复执行升级，支持 `--uninstall`。
安装脚本会自动把 `~/.local/bin` 写入已存在的 `~/.bashrc` / `~/.zshrc` / `~/.profile`，
重开终端后即可直接运行 `drcom-cli` / `drcom-campus`（`--no-modify-path` 可关闭）。
教程见 [docs/install.md](https://github.com/ZincGluxx/drcom-rs/blob/linux/docs/install.md)，平台细节见 [docs/linux.md](https://github.com/ZincGluxx/drcom-rs/blob/linux/docs/linux.md)。

## 📦 发布物

| 文件 | 说明 |
| --- | --- |
| `drcom-campus-linux-x86_64-v1.1.0.tar.gz` | GUI + CLI + 图标 + 桌面入口 + 文档 |
| `drcom-campus-linux-x86_64-v1.1.0.tar.gz.sha256` | SHA256 校验 |

## ✅ 质量

- 133 项测试全部通过（106 lib + 27 bin，含 Linux 平台层新增测试）。
- `cargo clippy --all-targets -- -D warnings` 与 `cargo fmt --check` 干净。
- 本机实测：真实认证服务器接受挑战（enp46s0 / 49.140.185.105）；GUI 静默启动无窗口、
  单实例二次启动 3 ms 返回；打包、安装脚本、卸载流程全部验证。

## ⚠️ 已知限制

- Linux 版尚未像 Windows 版那样经过长期真实账号保活实测；首个版本请留意在线状态。
- GNOME 需要 AppIndicator/KStatusNotifierItem 扩展才显示托盘；没有托盘时关闭窗口会转为
  后台静默运行，用再次启动程序唤出窗口。
- 发布包按构建机 glibc（Arch，最高 GLIBC_2.43）链接；较旧发行版请在目标机源码构建。
- Linux 凭据以 0600 权限明文 JSON 保存（平台无 DPAPI 等价物）。
- 静态 IPv4 修改依赖 NetworkManager；修改路由表需要 root / sudo。
- 本次未构建 Windows 安装包；Windows 用户可继续使用 v1.0.4 安装包，或用本版本源码构建。
