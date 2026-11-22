#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/dev-install.sh [options]

Build and activate a Windows debug install from WSL2.

  --root PATH      Windows path or WSL path on a Windows drive
                   (default: %LOCALAPPDATA%\expos)
  --no-build       Reuse both exes in target/xwin-head
  --keep N         Keep the newest N developer versions (default: 5)
                   plus the current and any in-use versions
  --shortcut       Create or refresh the Windows Start Menu shortcut
  --data-dir PATH  Print a PowerShell EXPOS_DATA_DIR override for PATH
  --dry-run        Show the plan without building or writing files
  -h, --help       Show this help
EOF
}

die() {
    printf 'dev-install: %s\n' "$*" >&2
    exit 1
}

root_arg=
data_arg=
keep=5
build=true
shortcut=false
dry_run=false
while (($#)); do
    case $1 in
        --root|--data-dir|--keep)
            [[ -n ${2:-} && $2 != --* ]] || die "$1 requires a value"
            case $1 in
                --root) root_arg=$2 ;;
                --data-dir) data_arg=$2 ;;
                --keep) keep=$2 ;;
            esac
            shift
            ;;
        --no-build) build=false ;;
        --shortcut) shortcut=true ;;
        --dry-run) dry_run=true ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown option: $1. See --help." ;;
    esac
    shift
done

[[ $keep =~ ^(0|[1-9][0-9]{0,9})$ ]] && ((keep <= 2147483647)) ||
    die '--keep requires an integer from 0 to 2147483647'

to_windows_path() {
    local path=$1
    case $path in
        [A-Za-z]:\\*|[A-Za-z]:/*|\\\\*) printf '%s\n' "${path//\//\\}" ;;
        \~|\~/*) wslpath -aw "$HOME${path:1}" ;;
        *) wslpath -aw "$path" ;;
    esac
}

powershell_literal() {
    printf "'%s'" "${1//\'/\'\'}"
}

print_command() {
    printf ' %q' "$@"
    printf '\n'
}

command -v wslpath >/dev/null || die 'Run this script from WSL2 with wslpath available.'
if [[ -n $root_arg ]]; then
    win_root=$(to_windows_path "$root_arg")
else
    command -v cmd.exe >/dev/null || die 'WSL interoperability is required to read Windows LOCALAPPDATA.'
    c_drive=$(wslpath -au 'C:\')
    [[ -d $c_drive ]] || die 'The Windows C: drive is unavailable in WSL.'
    # Starting cmd.exe from the WSL filesystem can print a UNC path warning.
    local_app_data=$(cd -- "$c_drive" && cmd.exe /d /c 'echo %LOCALAPPDATA%' | tr -d '\r')
    [[ $local_app_data == [A-Za-z]:\\* ]] || die 'Could not resolve Windows LOCALAPPDATA. Use --root.'
    win_root=${local_app_data%\\}\\expos
fi
win_root=${win_root%\\}
[[ $win_root == [A-Za-z]:\\* ]] || die 'The install root must be on a local Windows drive.'
root_wsl=$(wslpath -au "$win_root")
win_root=$(wslpath -aw "$root_wsl")

data_dir=
if [[ -n $data_arg ]]; then
    data_dir=$(to_windows_path "$data_arg")
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$repo"
target=x86_64-pc-windows-msvc
target_dir=$repo/target/xwin-head
build_dir=$target_dir/$target/debug
tools_dir=$repo/target/xwin-tools
llvm_dir=/usr/lib/llvm-18/bin
wrapper=$tools_dir/llvm-rc

package_version=$(awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ { split($0, fields, "\""); print fields[2]; exit }
' Cargo.toml)
base=${package_version%%[-+]*}
[[ $base =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
    die "Could not read the expos package version from Cargo.toml: $package_version"
timestamp=$(date -u +%Y%m%dT%H%M%S%N)
read -r run_id </proc/sys/kernel/random/uuid
version=$base-alpha.dev.$timestamp.$run_id

channel=stable
[[ ${package_version%%+*} != *-* ]] || channel=beta
installer=$repo/release/Install-Expos.ps1
build_command=(cargo xwin build --locked -p expos -p expos-launcher --target "$target")

print_launch_hint() {
    printf '\nLaunch from PowerShell\n'
    if [[ -n $data_dir ]]; then
        printf '  $env:EXPOS_DATA_DIR = %s\n' "$(powershell_literal "$data_dir")"
    fi
    printf '  Start-Process -FilePath %s -WorkingDirectory %s\n' \
        "$(powershell_literal "$win_root\expos.exe")" "$(powershell_literal "$win_root")"
    printf '\nDefault data from Cargo version %s: %s\n' \
        "$package_version" '%LOCALAPPDATA%\expos\data\'"$channel"
    printf 'On first launcher start, an absent channel directory is seeded from legacy data if it exists.\n'
    printf 'To open the legacy directory directly, set this before launching:\n'
    printf '  $env:EXPOS_DATA_DIR = Join-Path $env:LOCALAPPDATA %s\n' "$(powershell_literal 'gpui-filer\v6')"
}

if $dry_run; then
    if $build; then
        [[ -e $wrapper ]] || printf 'Would create the llvm-rc wrapper at %s\n' "$wrapper"
        printf 'Would build with PATH=%q:$PATH CARGO_TARGET_DIR=%q' "$tools_dir:$llvm_dir" "$target_dir"
        print_command "${build_command[@]}"
    else
        printf 'Would reuse exes from %s\n' "$build_dir"
    fi
    printf 'Would mirror the locked gpui_windows shaders to the Windows C: drive and the install drive.\n'
    printf 'Would stage under %s and activate with Install-Expos.ps1 -FromDir.\n' "$win_root"'\.staging'
    printf 'Version directory: %s\nExpected current: v%s\n' "$win_root"'\v'"$version" "$version"
    printf 'Would keep the newest %s developer versions plus current and in-use versions.\n' "$keep"
    if $shortcut; then printf 'Would create or refresh the Start Menu shortcut.\n'; fi
    print_launch_hint
    exit 0
fi

command -v powershell.exe >/dev/null || die 'WSL interoperability is required to run powershell.exe.'
if $build; then
    command -v cargo >/dev/null || die 'Rust and cargo-xwin are required to build.'
else
    for exe in expos.exe expos-launcher.exe; do
        [[ -f $build_dir/$exe ]] || die "Missing $build_dir/$exe. Run without --no-build first."
    done
fi

# Cargo records the exact git commit after the # in the locked source URL.
zed_rev=$(awk '
    /^\[\[package\]\]/ { windows_package = 0 }
    /^name = "gpui_windows"$/ { windows_package = 1 }
    windows_package && /^source = "git\+/ { split($0, fields, /[#"]/); print fields[3]; exit }
' Cargo.lock)
[[ $zed_rev =~ ^[0-9a-f]{40}$ ]] || die 'Could not read the locked gpui_windows revision.'
cargo_home=${CARGO_HOME:-$HOME/.cargo}
[[ $cargo_home == /* ]] || cargo_home=$PWD/$cargo_home

find_zed_checkout() {
    local checkout found=
    for checkout in "$cargo_home"/git/checkouts/zed-*/*; do
        [[ -f $checkout/crates/gpui_windows/src/shaders.hlsl ]] || continue
        [[ $(git -C "$checkout" rev-parse HEAD) == "$zed_rev" ]] || continue
        [[ -z $found ]] || die "Multiple Zed checkouts match $zed_rev. Use the Cargo home of the build."
        found=$checkout
    done
    [[ -n $found ]] || return 1
    printf '%s\n' "$found"
}

if ! zed_checkout=$(find_zed_checkout); then
    $build || die "Zed checkout $zed_rev is unavailable. Use the Cargo home of the build."
    cargo fetch --locked --target "$target"
    zed_checkout=$(find_zed_checkout) || die "Could not locate the locked Zed checkout $zed_rev."
fi

if $build; then
    if [[ ! -e $wrapper ]]; then
        [[ -x $llvm_dir/llvm-rc ]] || die "Missing $llvm_dir/llvm-rc. Install LLVM 18 first."
        mkdir -p -- "$tools_dir"
        wrapper_temp=$(mktemp "$tools_dir/llvm-rc.XXXXXXXX")
        cat >"$wrapper_temp" <<'EOF'
#!/bin/sh
# GPUI's Windows resource script refers to paths relative to its crate directory.
exec /usr/lib/llvm-18/bin/llvm-rc /I "${EXPOS_DEV_GPUI_INCLUDE:?}" "$@"
EOF
        chmod +x -- "$wrapper_temp"
        mv -- "$wrapper_temp" "$wrapper"
    fi
    [[ -x $wrapper ]] || die "The llvm-rc wrapper is not executable: $wrapper"
    PATH="$tools_dir:$llvm_dir:$PATH" CARGO_TARGET_DIR="$target_dir" \
        EXPOS_DEV_GPUI_INCLUDE="$zed_checkout/crates/gpui" "${build_command[@]}"
fi

# The embedded Linux path is root-relative on Windows, using the launch drive.
shader_src=$zed_checkout/crates/gpui_windows/src
shader_files=(shaders.hlsl color_text_raster.hlsl alpha_correction.hlsl)
for shader in "${shader_files[@]}"; do
    [[ -f $shader_src/$shader ]] || die "Missing debug shader: $shader_src/$shader"
done

mirror_shaders() {
    local drive_wsl destination shader
    drive_wsl=$(wslpath -au "$1:\\")
    [[ -d $drive_wsl ]] || die "Windows drive $1: is unavailable in WSL."
    destination=${drive_wsl%/}/${shader_src#/}
    mkdir -p -- "$destination"
    for shader in "${shader_files[@]}"; do
        if ! cmp -s -- "$shader_src/$shader" "$destination/$shader"; then
            cp -- "$shader_src/$shader" "$destination/$shader"
        fi
    done
}

mirror_shaders C
install_drive=${win_root:0:1}
if [[ ${install_drive^^} != C ]]; then
    mirror_shaders "$install_drive"
fi

# The installer owns <root>/.staging. Prepare the source outside it so that
# first-install ownership checks and uninstall locking apply to the final stage.
payload=$(mktemp -d "$target_dir/dev-payload.XXXXXXXX")
cleanup_payload() {
    rm -f -- "$payload/expos.exe" "$payload/VERSION.txt" "$payload/launcher/expos-launcher.exe"
    if [[ -d $payload/launcher ]]; then rmdir -- "$payload/launcher"; fi
    rmdir -- "$payload"
}
trap cleanup_payload EXIT
mkdir -- "$payload/launcher"
cp --reflink=auto -- "$build_dir/expos.exe" "$payload/expos.exe"
cp --reflink=auto -- "$build_dir/expos-launcher.exe" "$payload/launcher/expos-launcher.exe"
printf '%s\n' "$version" >"$payload/VERSION.txt"

install_args=(-File "$(wslpath -aw "$installer")" -FromDir "$(wslpath -aw "$payload")"
              -Root "$win_root" -KeepDev "$keep")
if ! $shortcut; then install_args+=(-NoShortcut); fi
powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass "${install_args[@]}"
print_launch_hint
