#!/usr/bin/env bash
# Linux x86_64 release downloader; installs GUI + CLI + daemon via package installer.
# Inspect this script before piping into Bash. SHA256 is required, not optional.
set -euo pipefail
repo='ZincGluxx/drcom-rs'
version=latest
base_url="${DRCOM_INSTALL_BASE_URL:-}"
mirror=''
args=()
bin_dir="${XDG_BIN_HOME:-${HOME:?HOME must be set}/.local/bin}"
uninstall=0
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: bash install.sh [--version VERSION|latest] [--mirror HTTPS_PREFIX]' \
      '                       [--no-mirror] [--autostart|--no-autostart]' \
      '                       [--bin-dir PATH] [--no-modify-path] [--uninstall]' \
      'Default: latest non-prerelease on GitHub. No third-party mirror is used by default.' \
      'DRCOM_INSTALL_BASE_URL can point to an asset directory (requires --version).' \
      'Download and SHA256 verification must both succeed. Account data is retained.'
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) [ "$#" -ge 2 ] || fail '--version needs a value'; version="${2#v}"; shift 2 ;;
        --mirror) [ "$#" -ge 2 ] || fail '--mirror needs a prefix'; mirror="${2%/}"; shift 2 ;;
        --no-mirror) mirror=''; shift ;;
        --bin-dir) [ "$#" -ge 2 ] || fail '--bin-dir needs a path'; bin_dir="$2"; args+=("$1" "$2"); shift 2 ;;
        --autostart|--no-autostart|--no-modify-path) args+=("$1"); shift ;;
        --uninstall) uninstall=1; shift ;;
        --yes|-y) shift ;;
        --help|-h) usage; exit 0 ;;
        *) fail "Unknown option: $1" ;;
    esac
done
[ "$(uname -s)" = Linux ] || fail 'This installer supports Linux only'
[ "$(uname -m)" = x86_64 ] || fail 'Only Linux x86_64 release assets are provided'
case "$bin_dir" in /*) ;; *) fail '--bin-dir must be absolute' ;; esac
bin_dir="${bin_dir%/}"
[ -n "$bin_dir" ] || fail 'Refusing to install into /'
if [ "$uninstall" = 1 ]; then
    data="${XDG_DATA_HOME:-$HOME/.local/share}"
    config="${XDG_CONFIG_HOME:-$HOME/.config}"
    # Only explicit installed files; never remove account data or arbitrary trees.
    rm -f -- "$bin_dir/drcom-campus" "$bin_dir/drcom-cli" "$bin_dir/drcom-daemon" \
      "$data/applications/drcom-campus.desktop" \
      "$data/icons/hicolor/128x128/apps/drcom-campus.png" \
      "$config/autostart/drcom-campus.desktop"
    printf 'Programs removed; account data retained under %s/drcom-campus\n' "$data"
    exit 0
fi
for tool in curl tar mktemp; do command -v "$tool" >/dev/null || fail "Missing tool: $tool"; done
if command -v sha256sum >/dev/null 2>&1; then
    hash_command=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    hash_command=shasum
else
    fail 'Install sha256sum (coreutils) or shasum; refusing an unchecked download'
fi
if [ "$version" = latest ]; then
    [ -z "$base_url" ] || fail 'Custom asset directories require --version'
    latest="https://github.com/$repo/releases/latest"
    [ -z "$mirror" ] || latest="$mirror/$latest"
    effective="$(curl -fLsS --connect-timeout 15 --max-time 60 -o /dev/null -w '%{url_effective}' "$latest")" || fail 'Cannot resolve latest release'
    version="${effective##*/}"
    version="${version#v}"
fi
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || fail "Invalid release version: $version"
stem="drcom-campus-linux-x86_64-v$version"
asset="$stem.tar.gz"
url="https://github.com/$repo/releases/download/v$version/$asset"
if [ -n "$base_url" ]; then
    url="${base_url%/}/$asset"
elif [ -n "$mirror" ]; then
    url="$mirror/$url"
fi
# Native GitHub requests are HTTPS; explicit custom local test/mirror URLs may be HTTP.
temp="$(mktemp -d "${TMPDIR:-/tmp}/drcom-install.XXXXXXXX")"
if [ -z "$temp" ] || [ ! -d "$temp" ]; then
    fail 'Cannot create temporary directory'
fi
trap 'rm -rf -- "$temp"' EXIT
fetch() { curl -fLsS --connect-timeout 15 --max-time 300 --retry 2 --output "$2" "$1"; }
printf 'Downloading DrCom Campus v%s (Linux x86_64)\n' "$version"
fetch "$url" "$temp/$asset" || fail 'Release download failed'
fetch "$url.sha256" "$temp/$asset.sha256" || fail 'Checksum download failed; refusing installation'
# Do not trust arbitrary filenames or extra entries supplied in a checksum file.
read -r expected filename <"$temp/$asset.sha256" || fail 'Invalid checksum file'
[[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || fail 'Invalid SHA256 digest'
[ "$filename" = "$asset" ] || fail 'Unexpected filename in checksum file'
if [ "$hash_command" = sha256sum ]; then
    actual="$(sha256sum "$temp/$asset")"
else
    actual="$(shasum -a 256 "$temp/$asset")"
fi
[ "${actual%% *}" = "$expected" ] || fail 'SHA256 mismatch; refusing installation'
printf 'SHA256 verified.\n'
# Check member paths before extraction. Official packages contain regular files
# and directories only; reject links/devices as well as traversal paths.
tar -tzf "$temp/$asset" >"$temp/members"
while IFS= read -r member; do
    case "$member" in "$stem"|"$stem/"*) ;; *) fail "Unsafe archive member: $member" ;; esac
    case "/$member/" in *'/../'*|*'/./'*) fail "Unsafe archive member: $member" ;; esac
done <"$temp/members"
tar -tvzf "$temp/$asset" >"$temp/member-types"
if grep -Ev '^[-d]' "$temp/member-types" >/dev/null; then
    fail 'Archive contains unsupported links/devices'
fi
tar -xzf "$temp/$asset" --no-same-owner -C "$temp"
[ -f "$temp/$stem/install.sh" ] || fail 'Missing package installer'
bash "$temp/$stem/install.sh" "${args[@]}"
