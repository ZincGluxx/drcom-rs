# drcom-campus — Dr.COM 校园网助手（Rust 版）

吉林大学 Dr.COM 校园网认证客户端的 Rust + Slint 重写版。Crate 名 `drcom-campus`，
产物为 GUI 客户端 `drcom-campus.exe` 与命令行工具 `drcom-cli.exe`。

## 特性

- **完整认证流程**：challenge → login → keep-alive → logout，逐字节对应 B 世代
  Python 参考实现（`newclient.py` / `jlu-drcom-py3`），已对真实认证服务器实测通过。
- **小而省**：单文件安装包约 6.5 MB，运行工作集约 24 MiB，稳态 CPU ≈ 0
  （对比 C# + Avalonia 版：安装包 11.6 MB、工作集 48 MiB，差值主要来自 Avalonia
  的原生绘图栈，Slint 自带渲染器不带这一套）。
- **托盘常驻**：关闭窗口最小化到托盘，托盘菜单退出；单实例保护（重复启动唤醒已运行实例）。
- **凭据本地保存**：DPAPI 加密存于 `%LOCALAPPDATA%\DrComCampusRust\preferences.dat`。
- **诊断能力**：内置网络双栈体检、事件日志（`drcom.log` 自动轮转）、一键导出诊断报告。
- **协议回归测试**：`reference/` 下有参考向量与校验脚本，测试基线 78 lib + 15 bin。

## 构建（Windows，MSVC）

```bash
cargo test --offline        # 测试基线：78 库测试 + 15 bin 测试
cargo build --release --offline
```

安装包用 Inno Setup 打包：

```bash
"C:/Program Files (x86)/Inno Setup 6/ISCC.exe" setup.iss
```

## 配置

命令行版读取 `drcom.ini`（查找顺序：exe 所在目录 → 当前目录 →
`%APPDATA%\DrComCampus\`），模板见 `drcom.ini.example`。全部键可省略——
省略 `auth_server` 时使用内置认证服务器，账号密码可在界面输入。

## 图标 / 资源

`assets/` 下的 `icon.ico` 源自 C# 版 `DrComCampus/Resources/icon.ico`；
`build.rs` 调用 Windows SDK 的 `rc.exe` 编译资源并内嵌进 exe。

## 目录结构

```
src/          协议、适配器、会话、诊断、UI 后端等
ui/           Slint UI 描述文件（app.slint）
assets/       图标资源
reference/    协议参考向量与 Python 校验脚本
examples/     UI 预览示例
```

## 相关仓库

- 协议参考实现（四代八套）：[jlu-drcom-client](https://github.com/ZincGluxx/jlu-drcom-client)
