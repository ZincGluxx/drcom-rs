# 三端集成本地验证记录

日期：2026-11-17；开发分支：`macos`；基线：workspace `e207997`。

## 已执行

- Windows 全工作区 `cargo check --workspace --all-targets` 成功。
- Windows `cargo clippy --locked --workspace --all-targets -- -D warnings` 成功。
- Windows `cargo test --locked --workspace --all-targets`：**195 passed / 0 failed**（core 132、daemon 集成 5、GUI 9、IPC 15、service 34）。GUI 使用 thread-local headless backend，断言不再因没有显示环境而跳过。
- Apple Silicon 和 Intel：分别执行完整 workspace / all-targets 的跨目标 Clippy，`-D warnings` 成功。包含 Slint UI、平台 Rust 代码与 Unix 专属测试的编译检查，**没有执行 Mac 二进制**。
- Linux 全 workspace（包含 Slint GUI 与 Unix 专属测试）：跨目标 Clippy / all-targets / `-D warnings` 成功。Windows 不具备 Linux sysroot，首次原样检查在 D-Bus/fontconfig pkg-config 失败；仅为无链接的类型检查使用 Cargo `links=dbus` / `links=fontconfig` build-script metadata override（声明库名 `dbus-1` / `fontconfig`）。这不是原生库链接验证，也不把这些 override 写进仓库或 CI；CI 必须安装真实 `libdbus-1-dev` / `libfontconfig1-dev`。
- Python 打包单元测试：**8 passed**；检查各平台格式与架构、bundle/deb/portable 布局、许可声明、标签版本、准确发布资产集合与 checksum 篡改。
- actionlint 1.7.11 成功；ShellCheck 0.11.0 和 Bash 语法检查成功。
- Windows 正式 release 配置：`RUSTFLAGS=-C target-feature=+crt-static` + `cargo build --locked --release --workspace --bins --target x86_64-pc-windows-msvc` 成功；实际生成三个 release EXE。
- 使用新打包脚本和真实 Inno Setup 6.7.3 构建 Windows portable ZIP 与安装 EXE 成功；CLI/daemon `--help` 成功，未发起认证。

## 未执行，不能据此宣称通过

- 未向远程推送分支、未触发 GitHub Actions、未创建版本标签或 GitHub Release。
- 未在 macOS 原生链接、执行 codesign/hdiutil、安装 DMG 或验证 Gatekeeper。
- 未在 Linux 原生运行 dpkg、隔离 HOME 的完整安装集成测试或 GUI；这些步骤配置在 native runner。
- 未操作真实 Keychain/Secret Service、真实自启项、实际修改网卡/路由。
- 未用真实校园网账号做三端认证、断线恢复或长时保活。

## 审查加固

- Unix 单实例采用私有用户目录 + 生命周期 flock；socket 仅负责唤醒，防止同时重启时互相删除 live endpoint。新增 stale-socket 并发测试由 Unix native runner 执行。
- Linux 旧分支复用而非重写；改进明文凭据策略、网卡绑定失败策略和已有路由归属。
- Linux 升级不自动重新开启用户禁用的登录启动项。
- macOS/Linux 保留最小化原生窗口；不替换、不注入、不 swizzle winit 的内部 AppKit delegate。macOS Cmd-Q / Dock Quit 不保证走服务下线，应按 [平台说明](platforms.md) 使用菜单栏图标“退出程序”。

源代码和 CI 已准备好，原生 runner 成功及校园网实测是后续发布验收，不应把静态编译检查描述成原生测试结果。
