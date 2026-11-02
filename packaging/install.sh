#!/usr/bin/env bash
#
# DrCom 校园网助手（Rust 版）Linux 用户级安装脚本。
#
# 安装内容（全部位于当前用户目录内，不需要 root，可重复执行）：
#   ~/.local/bin/drcom-campus                                     图形界面
#   ~/.local/bin/drcom-cli                                        命令行客户端
#   ~/.local/share/icons/hicolor/128x128/apps/drcom-campus.png    应用图标
#   ~/.local/share/applications/drcom-campus.desktop              桌面菜单入口
#
# 用法：
#   ./install.sh                在发行包解压目录中安装（已存在的开机自启会升级为静默）
#   ./install.sh --autostart    安装并启用静默开机自启（--silent 登录桌面自动认证）
#   ./install.sh --no-autostart 安装但不创建/删除开机自启
#   ./install.sh --uninstall    卸载程序与桌面入口（保留账号配置）
#   packaging/install.sh        在源码仓库中运行（需先 cargo build --release）
#   ./install.sh --help
#
# 环境变量：
#   HOME            安装位置以当前用户主目录为基准
#   XDG_DATA_HOME   图标和桌面入口的位置，默认 ~/.local/share
#   XDG_BIN_HOME    二进制安装目录，默认 ~/.local/bin
set -euo pipefail

die() {
    printf '错误：%s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
用法：./install.sh [选项]

把 drcom-campus、drcom-cli、应用图标与桌面入口安装到当前用户目录：
  ~/.local/bin/drcom-campus
  ~/.local/bin/drcom-cli
  ~/.local/share/icons/hicolor/128x128/apps/drcom-campus.png
  ~/.local/share/applications/drcom-campus.desktop

选项：
  --autostart       安装并启用静默开机自启（~/.config/autostart，--silent）
  --no-autostart    安装但不创建/删除开机自启（默认：已存在的自启条目会被升级）
  --bin-dir <目录>   自定义二进制安装目录（默认 ~/.local/bin）
  --uninstall       卸载程序与桌面入口（保留账号配置）
  -h, --help        显示本帮助

不需要 root 权限，可以重复执行覆盖安装。
EOF
}

BIN_DIR_OVERRIDE=""
AUTOSTART="auto"
UNINSTALL=0
while [ $# -gt 0 ]; do
    case "$1" in
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
            BIN_DIR_OVERRIDE="$2"
            shift 2
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "未知参数：$1（用 --help 查看说明）"
            ;;
    esac
done

[ -n "${HOME:-}" ] || die "HOME 未设置，无法确定安装目录"
if [ "$(id -u)" -eq 0 ]; then
    printf '提示：当前以 root 运行。本脚本安装到 $HOME，建议用普通用户执行。\n' >&2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

BIN_DIR="${BIN_DIR_OVERRIDE:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
BIN_DIR="${BIN_DIR%/}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS_DIR="$DATA_DIR/applications"
ICON_DIR="$DATA_DIR/icons/hicolor/128x128/apps"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}"
AUTOSTART_FILE="$CONFIG_DIR/autostart/drcom-campus.desktop"

# 桌面入口的 Exec 需要一层字符串转义 + 一层 Exec 引号转义，路径含空格也能用。
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

write_autostart_entry() {
    mkdir -p "$(dirname "$AUTOSTART_FILE")"
    cat >"$AUTOSTART_FILE" <<EOF
[Desktop Entry]
Type=Application
Name=DrCom 校园网助手
Exec=$(exec_quote "$BIN_DIR/drcom-campus") --silent
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
}

if [ "$UNINSTALL" = 1 ]; then
    rm -f "$BIN_DIR/drcom-campus" "$BIN_DIR/drcom-cli" \
        "$ICON_DIR/drcom-campus.png" "$APPS_DIR/drcom-campus.desktop" \
        "$AUTOSTART_FILE"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$APPS_DIR" >/dev/null 2>&1 || true
    fi
    printf '已卸载程序与桌面入口\n'
    printf '账号与偏好设置仍保留在：%s\n' "$DATA_DIR/drcom-campus"
    printf '如需彻底清除：rm -rf %s\n' "$(printf '%q' "$DATA_DIR/drcom-campus")"
    exit 0
fi

# 发行包解压目录里二进制就在脚本旁边；从源码仓库运行时在 target/release/。
if [ -x "$SCRIPT_DIR/drcom-campus" ] && [ -x "$SCRIPT_DIR/drcom-cli" ]; then
    BIN_SRC_DIR="$SCRIPT_DIR"
elif [ -x "$REPO_ROOT/target/release/drcom-campus" ] && [ -x "$REPO_ROOT/target/release/drcom-cli" ]; then
    BIN_SRC_DIR="$REPO_ROOT/target/release"
else
    die "找不到 drcom-campus / drcom-cli：请在发行包解压目录中运行，或先在源码仓库执行 cargo build --release"
fi

GUI_SRC="$BIN_SRC_DIR/drcom-campus"
CLI_SRC="$BIN_SRC_DIR/drcom-cli"
DESKTOP_SRC="$SCRIPT_DIR/drcom-campus.desktop"
ICON_SRC="$SCRIPT_DIR/share/icons/hicolor/128x128/apps/drcom-campus.png"
if [ ! -f "$DESKTOP_SRC" ]; then
    DESKTOP_SRC="$SCRIPT_DIR/../packaging/drcom-campus.desktop"
fi
if [ ! -f "$ICON_SRC" ]; then
    ICON_SRC="$SCRIPT_DIR/../assets/icon-128.png"
fi
[ -f "$DESKTOP_SRC" ] || die "缺少桌面入口文件 drcom-campus.desktop"
[ -f "$ICON_SRC" ] || die "缺少应用图标 icon-128.png"

install -d -m 0755 "$BIN_DIR" "$APPS_DIR" "$ICON_DIR"
install -m 0755 "$GUI_SRC" "$BIN_DIR/drcom-campus"
install -m 0755 "$CLI_SRC" "$BIN_DIR/drcom-cli"
install -m 0644 "$ICON_SRC" "$ICON_DIR/drcom-campus.png"

# 桌面入口里的 Exec 使用绝对路径：图形启动器不保证 ~/.local/bin 在 PATH 中。
# 通过 ENVIRON 把路径原样交给 awk，避免 sed 对路径中特殊字符的转义问题。
EXEC_PATH="$(exec_quote "$BIN_DIR/drcom-campus")"
EXEC_PATH="$EXEC_PATH" awk '
    /^Exec=drcom-campus$/ { print "Exec=" ENVIRON["EXEC_PATH"]; replaced = 1; next }
    { print }
    END { if (!replaced) print "Exec=" ENVIRON["EXEC_PATH"] }
' "$DESKTOP_SRC" >"$APPS_DIR/drcom-campus.desktop"
chmod 0644 "$APPS_DIR/drcom-campus.desktop"

# 刷新桌面数据库与图标缓存；缺少工具时跳过，不影响安装结果。
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$APPS_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$DATA_DIR/icons/hicolor" >/dev/null 2>&1 || true
fi

[ -x "$BIN_DIR/drcom-campus" ] && [ -x "$BIN_DIR/drcom-cli" ] ||
    die "安装校验失败：二进制未就位"

case "$AUTOSTART" in
    enable)
        write_autostart_entry
        printf '已启用静默开机自启：%s\n' "$AUTOSTART_FILE"
        ;;
    disable)
        rm -f "$AUTOSTART_FILE"
        ;;
    auto)
        # 已存在自启条目的用户会被升级为 --silent，但不擅自新增。
        if [ -f "$AUTOSTART_FILE" ]; then
            write_autostart_entry
            printf '已更新开机自启条目（--silent 静默登录）\n'
        fi
        ;;
esac

printf '已安装：\n'
printf '  %s\n' \
    "$BIN_DIR/drcom-campus" \
    "$BIN_DIR/drcom-cli" \
    "$ICON_DIR/drcom-campus.png" \
    "$APPS_DIR/drcom-campus.desktop"

printf '\n下一步：\n'
case ":${PATH:-}:" in
    *":$BIN_DIR:"*) ;;
    *)
        printf '  1. 把 %s 加入 PATH：export PATH="%s:$PATH"\n' "$BIN_DIR" "$BIN_DIR"
        ;;
esac
printf '  - 图形界面：从应用菜单启动“DrCom 校园网助手（Rust）”，或直接运行 drcom-campus\n'
printf '  - 命令行：  drcom-cli --help\n'
printf '  - 首次使用：在界面中填写校园网账号密码；配置文件模板是发行包里的 drcom.ini.example\n'
printf '  - 静默后台运行：drcom-campus --silent（需先保存账号并勾选“自动登录”）\n'
printf '  - 卸载：./install.sh --uninstall\n'
