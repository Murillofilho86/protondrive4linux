#!/usr/bin/env bash
# Regenerate the pictures in docs/screenshots/ from the app's own example data.
#
# `protondrive4linux-gui --screenshots` draws every page with made-up folders,
# files and conflicts and never reads yours (see src/bin/gui/shots.rs). This runs
# it on top of that with a scratch HOME - inside a private X server when
# `xvfb-run` is installed, so nothing flashes on your screen and it can draw at
# 2x, and otherwise in your own X11 or XWayland session, where a window appears
# for a few seconds.
#
#   scripts/screenshots.sh              # build (release, gui feature) and draw
#   scripts/screenshots.sh --no-build   # reuse target/release/protondrive4linux-gui
#   scripts/screenshots.sh --hidpi      # twice the size (needs xvfb-run)
#   scripts/screenshots.sh --out DIR    # somewhere other than docs/screenshots
#
# PD4L_GUI_BIN=/path/to/protondrive4linux-gui uses that binary and skips the build.
set -euo pipefail

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
out="$root/docs/screenshots"
bin="${PD4L_GUI_BIN:-$root/target/release/protondrive4linux-gui}"
build=1
scale=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build) build=0 ;;
    --hidpi) scale=2 ;;
    --out) shift; out="${1:?--out needs a folder}" ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done

if [[ -z "${PD4L_GUI_BIN:-}" && $build -eq 1 ]]; then
  (cd "$root" && cargo build --release --features gui --bin protondrive4linux-gui)
fi
[[ -x "$bin" ]] || { echo "no GUI binary at $bin (run without --no-build)" >&2; exit 1; }

mkdir -p "$out"
scratch="$(mktemp -d)"
trap 'rm -rf -- "${scratch:?}"' EXIT
mkdir -p "$scratch/home"

# Whatever the app might look at lives in the scratch folder, not in your home.
sandbox=(env
  HOME="$scratch/home"
  XDG_CONFIG_HOME="$scratch/home/.config"
  XDG_DATA_HOME="$scratch/home/.local/share"
  XDG_STATE_HOME="$scratch/home/.local/state"
  XDG_CACHE_HOME="$scratch/home/.cache")

args=(--screenshots "$out")
[[ $scale -ne 1 ]] && args+=(--scale "$scale")

if command -v xvfb-run >/dev/null; then
  # A private X server. 2600x1700 leaves room for the 2x window.
  xvfb-run -a -s "-screen 0 2600x1700x24" \
    "${sandbox[@]}" WAYLAND_DISPLAY= XDG_SESSION_TYPE=x11 WINIT_UNIX_BACKEND=x11 \
    "$bin" "${args[@]}" \
    2> >(grep -v -E 'libEGL|vulkan|DRI3' >&2 || true)
elif [[ -n "${DISPLAY:-}" ]]; then
  if [[ $scale -ne 1 ]]; then
    echo "--hidpi needs xvfb-run (apt install xvfb): a desktop has no room for a window that big" >&2
    exit 1
  fi
  echo "no xvfb-run: drawing in your own X11/XWayland session (a window will appear for a few seconds)" >&2
  "${sandbox[@]}" "$bin" "${args[@]}" \
    2> >(grep -v -E 'libEGL|vulkan|DRI3|dbind|Gtk-WARN|appindicator' >&2 || true)
else
  echo "this needs an X11 or XWayland session (\$DISPLAY), or xvfb-run (apt install xvfb)" >&2
  exit 1
fi

if command -v optipng >/dev/null; then
  optipng -quiet -o2 "$out"/*.png
fi
ls -l "$out"
