#!/usr/bin/env bash
#
# DrCom 校园网助手（Rust 版）Linux x86_64 发布包构建脚本。
#
# 流程：构建 release 二进制 → 校验 ELF 格式 → 生成暂存目录 → 打包 tar.gz → 输出 SHA256。
#
# 产物（dist/ 下）：
#   drcom-campus-linux-x86_64-v<版本>/                  解压即用的目录（含 install.sh）
#   drcom-campus-linux-x86_64-v<版本>.tar.gz            发布压缩包
#   drcom-campus-linux-x86_64-v<版本>.tar.gz.sha256     校验文件
#
# 版本号取自 Cargo.toml。用法：bash scripts/package-linux.sh（可在任意目录执行）
set -euo pipefail

log() { printf '==> %s\n' "$*"; }
die() {
    printf '错误：%s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
用法：bash scripts/package-linux.sh

构建 Linux x86_64 发布包，产物在 dist/ 下：
  drcom-campus-linux-x86_64-v<版本>/
  drcom-campus-linux-x86_64-v<版本>.tar.gz
  drcom-campus-linux-x86_64-v<版本>.tar.gz.sha256

版本号取自 Cargo.toml。需要 Rust 工具链（~/.cargo/bin）、file、tar、gzip、sha256sum。
EOF
}

case "${1:-}" in
    -h | --help)
        usage
        exit 0
        ;;
    "") ;;
    *) die "未知参数：$1（本脚本不接受参数，用 --help 查看说明）" ;;
esac

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
# 与 README 的构建说明保持一致：rustup 的 cargo 不在默认 PATH 里时也能找到。
export PATH="$HOME/.cargo/bin:$PATH"

cd "$REPO_ROOT"
[ -f Cargo.toml ] || die "在 $REPO_ROOT 找不到 Cargo.toml；脚本必须放在仓库的 scripts/ 目录中"
[ -f src/main.rs ] || die "在 $REPO_ROOT 找不到 src/main.rs，仓库不完整"

command -v cargo >/dev/null 2>&1 ||
    die "找不到 cargo：请先安装 Rust 工具链（https://rustup.rs/），并确认 ~/.cargo/bin 在 PATH 中"
for tool in file tar gzip sha256sum install stat; do
    command -v "$tool" >/dev/null 2>&1 || die "缺少系统命令：$tool"
done

ARCH="$(uname -m)"
[ "$ARCH" = "x86_64" ] || die "本发布包只面向 x86_64 Linux，当前架构：$ARCH"

# 只认 [package] 节里的 version，[dependencies] 里的 =1.17.1 之类不会被误读。
VERSION="$(awk -F'"' '
    $0 == "[package]" { section = $0 }
    section == "[package]" && /^version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)"
[ -n "$VERSION" ] || die "无法从 Cargo.toml 的 [package] 节读取版本号"

NAME="drcom-campus-linux-x86_64-v${VERSION}"
DIST="$REPO_ROOT/dist"
STAGE="$DIST/$NAME"
TARBALL_NAME="$NAME.tar.gz"
TARBALL="$DIST/$TARBALL_NAME"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
RELEASE_DIR="$TARGET_DIR/release"
GUI_BIN="$RELEASE_DIR/drcom-campus"
CLI_BIN="$RELEASE_DIR/drcom-cli"

log "[1/5] 构建 release 二进制（cargo build --release）"
if ! cargo build --release --offline; then
    log "离线构建失败，改用联网构建重试"
    cargo build --release
fi

log "[2/5] 校验二进制与打包素材"
check_elf() {
    local path="$1" label="$2" info
    [ -f "$path" ] || die "缺少 $label：$path"
    [ -x "$path" ] || die "$label 没有可执行权限：$path"
    info="$(file -b "$path")"
    case "$info" in
        *"ELF 64-bit"*"x86-64"*) log "    $label：$info" ;;
        *) die "$label 不是 x86-64 ELF 可执行文件：$info" ;;
    esac
}
check_elf "$GUI_BIN" "drcom-campus"
check_elf "$CLI_BIN" "drcom-cli"

# 打包前先确认二进制能在本机执行；--help 不需要显示服务器，也不会读取配置或发送报文。
"$CLI_BIN" --help >/dev/null 2>&1 || die "drcom-cli --help 执行失败，二进制可能无法在本机运行"
log "    drcom-cli --help 执行正常"

for required in drcom.ini.example LICENSE docs/linux.md docs/install.md packaging/install.sh packaging/drcom-campus.desktop assets/icon-128.png; do
    [ -f "$REPO_ROOT/$required" ] || die "缺少打包所需文件：$required"
done

log "[3/5] 生成暂存目录 dist/$NAME"
mkdir -p "$DIST"
rm -rf -- "$STAGE" "$TARBALL" "$TARBALL.sha256"
mkdir -p "$STAGE/docs" "$STAGE/share/icons/hicolor/128x128/apps"
install -m 0755 "$GUI_BIN" "$STAGE/drcom-campus"
install -m 0755 "$CLI_BIN" "$STAGE/drcom-cli"
install -m 0644 "$REPO_ROOT/drcom.ini.example" "$STAGE/drcom.ini.example"
install -m 0644 "$REPO_ROOT/LICENSE" "$STAGE/LICENSE"
install -m 0644 "$REPO_ROOT/docs/linux.md" "$STAGE/docs/linux.md"
install -m 0644 "$REPO_ROOT/docs/install.md" "$STAGE/docs/install.md"
install -m 0755 "$REPO_ROOT/packaging/install.sh" "$STAGE/install.sh"
install -m 0644 "$REPO_ROOT/packaging/drcom-campus.desktop" "$STAGE/drcom-campus.desktop"
install -m 0644 "$REPO_ROOT/assets/icon-128.png" "$STAGE/share/icons/hicolor/128x128/apps/drcom-campus.png"

cat >"$STAGE/RELEASE-NOTES.txt" <<EOF
DrCom 校园网助手（Rust 版）— Linux x86_64 构建 v${VERSION}

本包内容：
  drcom-campus            图形界面（Slint）
  drcom-cli               命令行客户端
  drcom.ini.example       配置文件模板
  install.sh              用户级安装脚本（安装到 ~/.local，无需 root）
  drcom-campus.desktop    桌面菜单入口
  share/icons/...         应用图标
  docs/linux.md           Linux 使用、构建与打包说明
  docs/install.md         一行安装与账号配置教程

Linux 版支持：
  - 与 Windows 版相同的 Dr.COM 认证与保活协议（challenge → login → keep-alive → logout）
  - 图形界面、系统托盘（StatusNotifierItem）、单实例运行
  - 静默后台运行：drcom-campus --silent 不显示窗口直接登录保活；没有托盘时关闭窗口
    也转入后台静默保活，再次启动程序唤回窗口；开机自启条目自带 --silent
  - 账号密码保存在 ~/.local/share/drcom-campus（文件权限 0600）
  - 自动登录、自动重连、开机自启（~/.config/autostart）
  - 认证服务器 /32 主机路由管理（需要 root 或 sudo）
  - 网络诊断：校内 ping、外网 IPv4/IPv6 HTTPS 检测（需要 ping、curl）
  - 通过 NetworkManager（nmcli）设置静态 IPv4（需要 polkit 授权）
  - 一行安装：curl -fsSL <install.sh> | bash，国内镜像可用 ghproxy.net 等

已知限制：
  - Linux 移植版尚未像 Windows 版那样经过长期真实环境实测，首个版本请留意认证与保活表现
  - GNOME 需要 AppIndicator/KStatusNotifierItem 扩展才能显示托盘图标；没有托盘时窗口关闭后
    仍在后台运行，用再次启动程序唤回（取消“关闭后静默运行”再关闭即可退出）
  - 非特权运行时认证套接字的出接口绑定会退回为源地址绑定
  - 修改静态 IPv4 需要 NetworkManager，并可能弹出 polkit 授权框
  - 发布包按构建机的 glibc 链接；系统过旧时请从源码构建
  - ELF 二进制没有 Windows 版本资源（图标由桌面入口提供）

详见 docs/linux.md 与 docs/install.md。
EOF
chmod 0644 "$STAGE/RELEASE-NOTES.txt"

log "[4/5] 生成压缩包与 SHA256"
tar -C "$DIST" -czf "$TARBALL" "$NAME"
[ -s "$TARBALL" ] || die "压缩包生成失败：$TARBALL"
( cd "$DIST" && sha256sum "$TARBALL_NAME" >"$TARBALL_NAME.sha256" )

log "[5/5] 打包完成"
SIZE_BYTES="$(stat -c '%s' "$TARBALL")"
if command -v numfmt >/dev/null 2>&1; then
    SIZE_HUMAN="$(numfmt --to=iec-i --suffix=B "$SIZE_BYTES")"
else
    SIZE_HUMAN="$SIZE_BYTES 字节"
fi
SHA256="$(cut -d' ' -f1 "$TARBALL.sha256")"
printf '  发布目录  %s\n' "$STAGE"
printf '  压缩包    %s（%s）\n' "$TARBALL" "$SIZE_HUMAN"
printf '  校验文件  %s\n' "$TARBALL.sha256"
printf '  SHA256    %s\n' "$SHA256"
printf '  解压后执行 ./install.sh 即可安装到 ~/.local（无需 root）\n'
