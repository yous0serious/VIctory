#!/usr/bin/env bash
# Runs victory.exe (or another Windows exe) under Proton's Wine on Linux,
# with vkd3d-proton (D3D12) and DXVK (DXGI) from the Proton build.
# See docs/linux-proton.md.
#
#   scripts/proton-run.sh config
#   scripts/proton-run.sh run /path/to/game/<the title's exe> --execute
#
# As cargo's runner (its first argument is the test exe):
#   CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER=$PWD/scripts/proton-run.sh \
#       cargo xwin test --workspace --target x86_64-pc-windows-msvc
#
# Environment:
#   PROTON       a Proton folder (default: Proton - Experimental from Steam)
#   PREFIX       the Proton prefix to use (default: ~/.local/share/victory/proton)
#   VICTORY_EXE  the exe to run (default: this checkout's release build)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [[ -z "${PROTON:-}" ]]; then
    for d in "$HOME/.steam/steam/steamapps/common/Proton - Experimental" \
             "$HOME/.local/share/Steam/steamapps/common/Proton - Experimental" \
             "$HOME/.var/app/com.valvesoftware.Steam/data/Steam/steamapps/common/Proton - Experimental"; do
        if [[ -x "$d/files/bin/wine" ]]; then PROTON="$d"; break; fi
    done
fi
if [[ -z "${PROTON:-}" || ! -x "$PROTON/files/bin/wine" ]]; then
    echo "Proton not found: install Proton Experimental in Steam or set PROTON" >&2
    exit 1
fi
PREFIX="${PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton}"

# First run: let Proton build the prefix (it installs vkd3d-proton and DXVK).
if [[ ! -f "$PREFIX/pfx/system.reg" ]]; then
    mkdir -p "$PREFIX"
    STEAM_COMPAT_DATA_PATH="$PREFIX" \
    STEAM_COMPAT_CLIENT_INSTALL_PATH="${STEAM_COMPAT_CLIENT_INSTALL_PATH:-$HOME/.steam/steam}" \
    SteamAppId=0 SteamGameId=0 \
        python3 "$PROTON/proton" run cmd /c exit >/dev/null 2>&1 || true
fi

export WINEPREFIX="$PREFIX/pfx"
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core=n,b;${WINEDLLOVERRIDES:-}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEFSYNC=1
# vkd3d-proton's pipeline cache, out of the working directory.
export VKD3D_SHADER_CACHE_PATH="${VKD3D_SHADER_CACHE_PATH:-$PREFIX/vkd3d-cache}"
mkdir -p "$VKD3D_SHADER_CACHE_PATH"

WINE="$PROTON/files/bin/wine"
if [[ -n "${VICTORY_EXE:-}" ]]; then
    exec "$WINE" "$VICTORY_EXE" "$@"
elif [[ "${1:-}" == *.exe && -f "${1:-}" ]]; then
    exec "$WINE" "$@"
else
    exec "$WINE" "$ROOT/target/x86_64-pc-windows-msvc/release/victory.exe" "$@"
fi
