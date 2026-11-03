# 第三方组件与上游归属声明（Third-Party Notices）

本项目（DrCom 校园网助手 Rust 版 / drcom-campus）在 Dr.COM 认证协议的实现上参考了下列开源项目。
本文件用于满足署名与许可要求，不改变各上游项目自身的许可条款。

## 1. drcoms/jlu-drcom-client

- 项目主页：<https://github.com/drcoms/jlu-drcom-client>
- 上游许可：GNU Affero General Public License v3.0（见上游仓库根目录 `LICENSE`）
- 本项目参考的内容：
  - `newclient.py`、`jlu-drcom-py3`：B 世代认证流程、登录帧分段与各字段取值；
  - `client.py` / `newclient.py`：challenge 计算、响应校验、心跳保活节奏；
  - 上述实现对 Dr.COM 服务端字段约定（固定偏移、魔数、填充规则）的整理。
- 使用方式：上述逻辑由本项目以 Rust 重新实现（Slint 桌面端），**未直接包含上游源码文件**；
  `reference/` 下的 Python 脚本为协议参考向量的生成/校验工具，同样据上游实现整理。
  `src/reference_login.rs` 与上游 B 世代实现逐字节对应，由测试钉住。
- 上游作者与贡献者保留其著作权。若上游作者认为本项目的署名或使用方式需要调整，
  请通过本项目仓库 Issue 联系，我们会及时更正。

> 说明：上游 README 另行注明其 Java 版 jar 产物采用 CC BY-NC-SA 4.0；本项目只参考其**源码**中的协议逻辑，
> 未使用、未分发其 jar 产物。

## 2. 运行时依赖

本项目通过 crates.io 引用的第三方库（`slint`、`socket2`、`serde`、`base64`、`image` 等）
各自遵循其自身许可（多为 MIT / Apache-2.0），详见各 crate 包内的 `LICENSE` 文件或
<https://crates.io> 上的元数据。这些依赖不改变本项目整体的 AGPL-3.0 授权。

---

最后更新：2026-11（许可由 CC BY-NC 4.0 变更为 AGPL-3.0 时同步添加）
