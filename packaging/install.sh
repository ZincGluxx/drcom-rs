#!/usr/bin/env bash
# User-only portable installer. Compatible with origin/linux's archive layout.
set -euo pipefail
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: bash install.sh [--autostart|--no-autostart] [--bin-dir PATH]' \
      '                       [--no-modify-path] [--uninstall]' \
      'Installs GUI, CLI and daemon under ~/.local. Never enables a daemon service.' \
      '--no-autostart leaves any existing autostart entry unchanged.' \
      'Uninstall retains account/preferences data.'
}
: "${HOME:?HOME must be set}"
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}"
autostart=auto
uninstall=0
modify_path=1
while [ "$#" -gt 0 ]; do
    case "$1" in
        --autostart) autostart=enable; shift ;;
        --no-autostart) autostart=keep; shift ;;
        --bin-dir) [ "$#" -ge 2 ] || fail '--bin-dir needs a path'; bin_dir="$2"; shift 2 ;;
        --no-modify-path) modify_path=0; shift ;;
        --uninstall) uninstall=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) fail "Unknown option: $1" ;;
    esac
done
# Absolute paths avoid cwd-dependent desktop launchers. Reject characters desktop
# entry syntax cannot represent, and percent expansion in the Exec field.
for path in "$bin_dir" "$data_dir" "$config_dir"; do
    case "$path" in /*) ;; *) fail "Expected an absolute path: $path" ;; esac
    case "$path" in *$'\n'*|*$'\r'*) fail 'Unsupported path characters' ;; esac
done
bin_dir="${bin_dir%/}"
[ -n "$bin_dir" ] || fail 'Refusing to install into /'
apps_dir="$data_dir/applications"
icon_dir="$data_dir/icons/hicolor/128x128/apps"
autostart_file="$config_dir/autostart/drcom-campus.desktop"
if [ "$uninstall" = 1 ]; then
    # Delete only the explicit installed filenames, never account data/directories.
    rm -f -- "$bin_dir/drcom-campus" "$bin_dir/drcom-cli" "$bin_dir/drcom-daemon" \
      "$apps_dir/drcom-campus.desktop" "$icon_dir/drcom-campus.png" "$autostart_file"
    printf 'Removed programs and launchers. Account data retained under %s/drcom-campus\n' "$data_dir"
    exit 0
fi
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
if [ -f "$script_dir/drcom-campus" ]; then
    source_dir="$script_dir"
    icon="$script_dir/share/icons/hicolor/128x128/apps/drcom-campus.png"
else
    source_dir="${CARGO_TARGET_DIR:-$repo_root/target}/release"
    icon="$repo_root/crates/drcom-gui/assets/icon-128.png"
fi
for program in drcom-campus drcom-cli drcom-daemon; do
    [ -x "$source_dir/$program" ] || fail "Missing executable: $source_dir/$program"
done
[ -f "$icon" ] || fail "Missing icon: $icon"
install -d -m 0755 "$bin_dir" "$apps_dir" "$icon_dir" "$data_dir/drcom-campus-dist"
for program in drcom-campus drcom-cli drcom-daemon; do
    install -m 0755 "$source_dir/$program" "$bin_dir/$program"
done
install -m 0644 "$icon" "$icon_dir/drcom-campus.png"
for name in LICENSE NOTICE THIRD-PARTY-NOTICES.md BUILD-INFO.json RELEASE-NOTES.txt; do
    if [ -f "$script_dir/$name" ]; then
        install -m 0644 "$script_dir/$name" "$data_dir/drcom-campus-dist/$name"
    elif [ -f "$repo_root/$name" ]; then
        install -m 0644 "$repo_root/$name" "$data_dir/drcom-campus-dist/$name"
    fi
done
# Desktop Entry has two escaping layers (value parsing and Exec parsing).
exec_quote() {
    local value="$1" out='"' char i
    for ((i=0; i<${#value}; i++)); do
        char="${value:i:1}"
        case "$char" in
            \\) out+="\\\\\\\\" ;;
            '"'|'`'|'$') out+="\\\\\\$char" ;;
            '%') out+='%%' ;;
            *) out+="$char" ;;
        esac
    done
    printf '%s"' "$out"
}
write_entry() {
    local target="$1" suffix="$2"
    printf '[Desktop Entry]\nType=Application\nName=DrCom Campus\nExec=%s%s\nIcon=drcom-campus\nTerminal=false\nCategories=Network;\n' \
      "$(exec_quote "$bin_dir/drcom-campus")" "$suffix" >"$target"
    chmod 0644 "$target"
}
write_entry "$apps_dir/drcom-campus.desktop" ''
# Only explicit opt-in may enable/rewrite autostart. In particular, preserve an
# existing Hidden=true or X-GNOME-Autostart-enabled=false entry on normal upgrade.
if [ "$autostart" = enable ]; then
    mkdir -p -- "$(dirname -- "$autostart_file")"
    write_entry "$autostart_file" ' --silent'
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$apps_dir" || true
fi
if [ "$modify_path" = 1 ]; then
    # Use Bash's %q rather than interpolating user paths as shell code. Only
    # Bash/Zsh rc files are changed; POSIX .profile does not support all %q output.
    printf -v quoted_bin '%q' "$bin_dir"
    for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
        [ -f "$rc" ] || continue
        if ! grep -qF '# drcom-campus user PATH' "$rc"; then
            # Expand PATH when the user's rc file runs, not during installation.
            # shellcheck disable=SC2016
            printf '\n# drcom-campus user PATH\nexport PATH=%s:"$PATH"\n' "$quoted_bin" >>"$rc"
        fi
    done
fi
printf 'Installed GUI, CLI and daemon to %s\n' "$bin_dir"
printf 'Open DrCom Campus from the application menu. Reopen your shell if PATH changed.\n'
