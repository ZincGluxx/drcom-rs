#!/usr/bin/env bash
#
# DrCom 校园网助手（Rust 版）— Linux x86_64 一键安装脚本
#
# 从 GitHub Releases 下载发布包并安装到当前用户目录（不需要 root）：
#   - ~/.local/bin/drcom-campus、drcom-cli
#   - ~/.local/share/applications/drcom-campus.desktop
#   - ~/.local/share/icons/hicolor/128x128/apps/drcom-campus.png
#
# 用法（直连 GitHub）：
#   curl -fsSL https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
#
# 用法（国内网络，脚本与发布包都走镜像）：
#   curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
#
# 需要同时启用静默开机自启时：
#   curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash -s -- --autostart
#
# 可选参数：
#   --version <v>       安装指定版本（默认 ${DEFAULT_VERSION}）
#   --autostart         写入 ~/.config/autostart/drcom-campus.desktop（--silent 静默登录）
#   --no-autostart      不创建 / 删除开机自启条目
#   --bin-dir <目录>     自定义二进制安装目录（默认 ~/.local/bin）
#   --no-mirror         不使用国内镜像，只直连 github.com
#   --mirror <前缀>     额外优先尝试的镜像前缀，例如 https://ghproxy.net
#   --uninstall         卸载已安装的程序与桌面入口（保留账号配置）
#   --help              显示本帮助
#
# 测试 / 自建镜像：环境变量 DRCOM_INSTALL_BASE_URL 指向存放发布包的目录
# （例如 http://127.0.0.1:8000 或 https://your-mirror/drcom），脚本会优先从那里下载。
set -euo pipefail

REPO="ZincGluxx/drcom-rs"
DEFAULT_VERSION="1.2.5"
ASSET_PREFIX="drcom-campus-linux-x86_64"
MIRRORS=(
    "https://ghproxy.net"
    "https://gh-proxy.com"
    "https://ghfast.top"
    "https://github.moeyy.xyz"
    "https://mirror.ghproxy.com"
)

VERSION="${DEFAULT_VERSION}"
BIN_DIR=""
AUTOSTART="auto"
UNINSTALL=0
USE_MIRRORS=1
MODIFY_PATH=1
EXTRA_MIRROR=""
BASE_URL="${DRCOM_INSTALL_BASE_URL:-}"

say() { printf '==> %s\n' "$*"; }
warn() { printf '警告：%s\n' "$*" >&2; }
die() {
    printf '错误：%s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '3,40p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || true
    cat <<'EOF'
用法：
  curl -fsSL https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash
  curl -fsSL https://ghproxy.net/https://raw.githubusercontent.com/ZincGluxx/drcom-rs/linux/install.sh | bash -s -- --autostart

参数：
  --version <v>    安装指定版本（默认 1.1.0）
  --autostart      安装并写入静默开机自启条目
  --no-autostart   不创建 / 删除开机自启条目
  --bin-dir <目录>  自定义二进制安装目录（默认 ~/.local/bin）
  --no-mirror      不使用国内镜像，只直连 github.com
  --mirror <前缀>   额外优先尝试的镜像前缀（如 https://ghproxy.net）
  --no-modify-path 不自动把 ~/.local/bin 写入 shell 配置（默认会自动加入）
  --uninstall      卸载程序与桌面入口（保留账号配置）
  --help           显示本帮助
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || die "--version 需要一个版本号"
            VERSION="$2"
            shift 2
            ;;
        --autostart)
            AUTOSTART="enable"
            shift
            ;;
        --no-autostart)
            AUTOSTART="disable"
            shift
            ;;
        --bin-dir)
            [ $# -ge 2 ] || die "--bin-dir 需要一个目录"
            BIN_DIR="$2"
            shift 2
            ;;
        --no-mirror)
            USE_MIRRORS=0
            shift
            ;;
        --no-modify-path)
            MODIFY_PATH=0
            shift
            ;;
        --mirror)
            [ $# -ge 2 ] || die "--mirror 需要一个前缀"
            EXTRA_MIRROR="${2%/}"
            shift 2
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --yes | -y)
            shift
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        *)
            die "未知参数：$1（用 --help 查看说明）"
            ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || die "本脚本只支持 Linux"
ARCH="$(uname -m)"
[ "$ARCH" = "x86_64" ] || die "发布包只提供 x86_64 架构（当前：$ARCH）；其它架构请从源码构建：https://github.com/$REPO"

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
[ -n "$BIN_DIR" ] || BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
BIN_DIR="${BIN_DIR%/}"
DESKTOP_FILE="$DATA_HOME/applications/drcom-campus.desktop"
ICON_FILE="$DATA_HOME/icons/hicolor/128x128/apps/drcom-campus.png"
AUTOSTART_FILE="$CONFIG_HOME/autostart/drcom-campus.desktop"

# 桌面入口的 Exec 需要一层字符串转义 + 一层 Exec 引号转义；规则与程序的
# autostart 模块一致，安装路径含空格或特殊字符也不会被拆开。
exec_quote() {
    local path="$1" quoted='"' character index
    for ((index = 0; index < ${#path}; index++)); do
        character="${path:index:1}"
        case "$character" in
            '\') quoted+='\\\\' ;;
            '"' | "$(printf '\140')" | '$') quoted+="\\$character" ;;
            *) quoted+="$character" ;;
        esac
    done
    printf '%s"' "$quoted"
}

write_desktop_entry() {
    local target="$1" exec_line="$2"
    mkdir -p "$(dirname "$target")"
    cat >"$target" <<EOF
[Desktop Entry]
Type=Application
Name=DrCom 校园网助手（Rust）
Comment=吉林大学 Dr.COM 校园网认证客户端
Exec=$exec_line
Icon=drcom-campus
StartupWMClass=drcom-campus
Terminal=false
Categories=Network;
EOF
}

write_autostart_entry() {
    mkdir -p "$(dirname "$AUTOSTART_FILE")"
    cat >"$AUTOSTART_FILE" <<EOF
[Desktop Entry]
Type=Application
Name=DrCom 校园网助手
Exec=$(exec_quote "$BIN_DIR/drcom-campus") --silent
StartupWMClass=drcom-campus
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
}

update_desktop_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

if [ "$UNINSTALL" = 1 ]; then
    say "卸载 DrCom 校园网助手"
    rm -f "$BIN_DIR/drcom-campus" "$BIN_DIR/drcom-cli" "$DESKTOP_FILE" "$ICON_FILE" "$AUTOSTART_FILE"
    update_desktop_caches
    say "已卸载程序与桌面入口"
    printf '账号与偏好设置仍保留在：%s\n' "$DATA_HOME/drcom-campus"
    printf '如需彻底清除：rm -rf %s\n' "$(printf '%q' "$DATA_HOME/drcom-campus")"
    exit 0
fi

command -v curl >/dev/null 2>&1 || die "缺少 curl，请先安装（如 sudo pacman -S curl / sudo apt install curl）"
command -v tar >/dev/null 2>&1 || die "缺少 tar"
command -v sha256sum >/dev/null 2>&1 || warn "缺少 sha256sum，将跳过发布包校验"

ASSET="${ASSET_PREFIX}-v${VERSION}.tar.gz"
RELEASE_URL="https://github.com/$REPO/releases/download/v${VERSION}/${ASSET}"

asset_urls() {
    if [ -n "$BASE_URL" ]; then
        printf '%s\n' "${BASE_URL%/}/${ASSET}"
        return 0
    fi
    printf '%s\n' "$RELEASE_URL"
    if [ -n "$EXTRA_MIRROR" ]; then
        printf '%s\n' "$EXTRA_MIRROR/$RELEASE_URL"
    fi
    if [ "$USE_MIRRORS" = 1 ]; then
        local mirror
        for mirror in "${MIRRORS[@]}"; do
            printf '%s\n' "${mirror%/}/$RELEASE_URL"
        done
    fi
}

TMPDIR_INSTALL="$(mktemp -d "${TMPDIR:-/tmp}/drcom-install.XXXXXX")" ||
    die "无法创建临时目录"
trap 'rm -rf "$TMPDIR_INSTALL"' EXIT

download() {
    curl -fsSL --connect-timeout 10 --max-time 300 --retry 2 --retry-delay 1 -o "$2" "$1"
}

say "下载 DrCom 校园网助手 v${VERSION}（Linux x86_64）"
DOWNLOADED=0
while IFS= read -r url; do
    [ -n "$url" ] || continue
    printf '    尝试 %s\n' "$url"
    if ! download "$url" "$TMPDIR_INSTALL/$ASSET"; then
        continue
    fi
    if download "${url}.sha256" "$TMPDIR_INSTALL/$ASSET.sha256" 2>/dev/null; then
        if ! (cd "$TMPDIR_INSTALL" && sha256sum -c "$ASSET.sha256" >/dev/null 2>&1); then
            warn "SHA256 校验失败，换下一个下载地址"
            rm -f "$TMPDIR_INSTALL/$ASSET" "$TMPDIR_INSTALL/$ASSET.sha256"
            continue
        fi
        printf '    SHA256 校验通过\n'
    else
        warn "未获取到 .sha256 校验文件，跳过校验"
    fi
    DOWNLOADED=1
    break
done < <(asset_urls)

if [ "$DOWNLOADED" != 1 ]; then
    die "发布包下载失败。可手动从以下地址下载后执行包内的 install.sh：
  https://github.com/$REPO/releases/latest
  国内镜像：https://ghproxy.net/https://github.com/$REPO/releases/latest"
fi

say "解压发布包"
tar -xzf "$TMPDIR_INSTALL/$ASSET" -C "$TMPDIR_INSTALL" ||
    die "解压失败，下载的发布包可能不完整"
PKG_DIR="$TMPDIR_INSTALL/${ASSET_PREFIX}-v${VERSION}"
if [ ! -x "$PKG_DIR/drcom-campus" ] || [ ! -x "$PKG_DIR/drcom-cli" ]; then
    die "发布包内容不完整（缺少 drcom-campus / drcom-cli）"
fi

say "安装到 $BIN_DIR"
mkdir -p "$BIN_DIR"
install -m 0755 "$PKG_DIR/drcom-campus" "$BIN_DIR/drcom-campus"
install -m 0755 "$PKG_DIR/drcom-cli" "$BIN_DIR/drcom-cli"

mkdir -p "$(dirname "$ICON_FILE")"
if [ -f "$PKG_DIR/share/icons/hicolor/128x128/apps/drcom-campus.png" ]; then
    install -m 0644 "$PKG_DIR/share/icons/hicolor/128x128/apps/drcom-campus.png" "$ICON_FILE"
else
    warn "发布包中没有图标，桌面入口仍可使用"
fi
write_desktop_entry "$DESKTOP_FILE" "$(exec_quote "$BIN_DIR/drcom-campus")"
update_desktop_caches

case "$AUTOSTART" in
    enable)
        write_autostart_entry
        say "已启用静默开机自启：$AUTOSTART_FILE"
        ;;
    disable)
        rm -f "$AUTOSTART_FILE"
        say "未启用开机自启"
        ;;
    auto)
        # 旧版本装过的自启条目升级成新路径 + --silent，但不擅自新增。
        if [ -f "$AUTOSTART_FILE" ]; then
            write_autostart_entry
            say "已更新开机自启条目（--silent 静默登录）"
        fi
        ;;
esac

say "安装完成"
if ! "$BIN_DIR/drcom-cli" --help >/dev/null 2>&1; then
    warn "drcom-cli 在当前系统上无法运行，可能需要从源码构建（见 docs/linux.md）"
fi

# 把安装目录写进 shell 配置，否则新用户会在终端里遇到 "drcom-cli: 未找到命令"
# （图形界面不受影响，桌面入口用的是绝对路径）。只改已经存在的 rc 文件，
# 用 --no-modify-path 可以关闭这一行为。
ensure_path_in_shells() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) return 0 ;;
    esac
    if [ "$MODIFY_PATH" != 1 ]; then
        printf '\n提示：%s 不在 PATH 中，请自行加入：\n' "$BIN_DIR"
        printf '    export PATH="%s:$PATH"\n\n' "$BIN_DIR"
        return 0
    fi
    local rc updated=0
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        [ -f "$rc" ] || continue
        if grep -qF 'DrCom 校园网助手：把用户级二进制目录加入 PATH' "$rc" 2>/dev/null; then
            updated=1
            continue
        fi
        cat >>"$rc" <<EOF

# DrCom 校园网助手：把用户级二进制目录加入 PATH（由安装脚本添加）
case ":\$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) export PATH="$BIN_DIR:\$PATH" ;;
esac
EOF
        printf '已把 %s 加入 PATH：%s（重新打开终端或执行 source 后生效）\n' "$BIN_DIR" "$rc"
        updated=1
    done
    if [ "$updated" = 0 ]; then
        printf '\n提示：%s 不在 PATH 中，请手动加入：\n' "$BIN_DIR"
        printf '    export PATH="%s:$PATH"\n\n' "$BIN_DIR"
    fi
}

ensure_path_in_shells

cat <<EOF
下一步：
  1. 打开图形界面：应用菜单里的“DrCom 校园网助手（Rust）”，或运行 $BIN_DIR/drcom-campus
  2. 填写校园网账号 / 密码，按需勾选“自动登录”“开机启动”“关闭到托盘（或关闭后静默运行）”，点“保存设置”
  3. 已启用 --autostart 时，下次登录桌面会自动静默认证；想立即后台运行：
       $BIN_DIR/drcom-campus --silent
  4. 命令行用法：$BIN_DIR/drcom-cli --help
  5. 卸载：重新执行本脚本并加 --uninstall

文档：https://github.com/$REPO/blob/linux/docs/linux.md
EOF
