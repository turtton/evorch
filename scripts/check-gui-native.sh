#!/usr/bin/env bash
# Real Linux window/input regression in a private Xvfb; never uses the caller's display.
# Build first: cargo build -p gui --bin native_qa_window
# Usage: scripts/check-gui-native.sh [OUTPUT_DIR]
# GUI_QA_MODE=runtime uses evorch-gui --demo and requires working bubblewrap.
# Dependencies: Xvfb, Openbox, xdotool, ImageMagick, jq, git, coreutils.
set -Eeuo pipefail

fail() { printf 'Native GUI QA: %s\n' "$*" >&2; exit 1; }
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    sed -n '2,6p' "$0"
    exit 0
fi
[[ $# -le 1 ]] || fail 'Expected at most one output directory.'
[[ $(uname -s) == Linux ]] || fail 'This check requires Linux.'
for dependency in Xvfb openbox xdotool jq git timeout mktemp realpath; do
    command -v "$dependency" >/dev/null || fail "Missing dependency: $dependency"
done
# Support ImageMagick 7 and the ImageMagick 6 packages on Ubuntu CI runners.
if command -v magick >/dev/null; then
    image_command=(magick)
    capture_command=(magick import)
else
    for dependency in convert import; do
        command -v "$dependency" >/dev/null || fail "Missing ImageMagick dependency: $dependency"
    done
    image_command=(convert)
    capture_command=(import)
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
mode=${GUI_QA_MODE:-fixture}
case "$mode" in
    fixture) default_bin=$repo_root/target/debug/native_qa_window ;;
    runtime)
        default_bin=$repo_root/target/debug/evorch-gui
        command -v bwrap >/dev/null || fail "Missing dependency: bwrap"
        ;;
    *) fail "GUI_QA_MODE must be fixture or runtime, got: $mode" ;;
esac
gui_bin=$(realpath -- "${GUI_BIN:-$default_bin}")
[[ -x "$gui_bin" ]] || fail "GUI_BIN is not executable: $gui_bin (build the selected GUI_QA_MODE binary first)."
out=$(realpath -m -- "${1:-${GUI_QA_OUTPUT_DIR:-$repo_root/target/gui-native}}")
mkdir -p -- "$out"
# Each run owns a new evidence directory, so stale screenshots cannot satisfy checks.
out=$(mktemp -d "$out/run-XXXXXXXX")
qa_tmp=$(mktemp -d /tmp/evorch-gui-native.XXXXXXXX)
app_pid="" wm_pid="" xvfb_pid="" window="" qa_display=""
caller_display=${DISPLAY:-}
cleanup() {
    local result=$?
    trap - EXIT INT TERM
    if (( result != 0 )) && [[ "$window" =~ ^[0-9]+$ && -n "$qa_display" ]]; then
        run "${capture_command[@]}" -window "$window" "$out/failure.png" 2>/dev/null || true
        if [[ -f "$qa_tmp/saved-layout.json" ]]; then
            cp "$qa_tmp/saved-layout.json" "$out/failure-layout.json" || true
        fi
    fi
    for pid in "$app_pid" "$wm_pid" "$xvfb_pid"; do
        if [[ -n "$pid" ]]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -rf -- "$qa_tmp"
    if (( result != 0 )); then
        printf 'FAILED (exit %s). Evidence and logs: %s\n' "$result" "$out" >&2
        tail -n 30 "$out/app.log" >&2 2>/dev/null || true
    fi
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'printf "Failed at line %s: %s\n" "$LINENO" "$BASH_COMMAND" >&2' ERR
printf 'Native GUI QA evidence: %s\n' "$out"

mkdir -p "$qa_tmp/config" "$qa_tmp/data" "$qa_tmp/cache" "$qa_tmp/runtime" "$qa_tmp/work"
chmod 700 "$qa_tmp/runtime"
# env -i excludes provider keys, user Git hooks, session buses and Wayland sockets.
# HOME is omitted; all supported persisted state uses the private XDG directories.
qa_env=(env -i "PATH=$PATH" "LANG=C.UTF-8" "LC_ALL=C.UTF-8"
    "XDG_CONFIG_HOME=$qa_tmp/config" "XDG_DATA_HOME=$qa_tmp/data"
    "XDG_CACHE_HOME=$qa_tmp/cache" "XDG_RUNTIME_DIR=$qa_tmp/runtime"
    "TMPDIR=$qa_tmp" "GIT_CONFIG_NOSYSTEM=1" "GIT_CONFIG_GLOBAL=/dev/null"
    "SHELL=/bin/sh" "LIBGL_ALWAYS_SOFTWARE=1" "GALLIUM_DRIVER=llvmpipe"
    "WGPU_BACKEND=vulkan" "WINIT_UNIX_BACKEND=x11" "WINIT_X11_SCALE_FACTOR=1")
# Nix/other nonstandard installations need these paths to load graphics libraries.
for variable in LD_LIBRARY_PATH LIBGL_DRIVERS_PATH VK_ICD_FILENAMES VK_DRIVER_FILES FONTCONFIG_FILE FONTCONFIG_PATH; do
    if [[ -n ${!variable:-} ]]; then qa_env+=("$variable=${!variable}"); fi
done

# -displayfd atomically allocates a free display. No display client is run until
# it has supplied a valid number, different from the operator's DISPLAY.
timeout --kill-after=3s 120s "${qa_env[@]}" Xvfb -displayfd 3 \
    -screen 0 1600x1000x24 -dpi 96 -nolisten tcp \
    3>"$qa_tmp/display" >"$out/xvfb.log" 2>&1 &
xvfb_pid=$!
for ((attempt=0; attempt<100; attempt++)); do
    [[ -s "$qa_tmp/display" ]] && break
    kill -0 "$xvfb_pid" 2>/dev/null || fail "Xvfb exited; see $out/xvfb.log"
    sleep 0.1
done
read -r display_number < "$qa_tmp/display" || fail 'Xvfb did not allocate a display within 10 seconds.'
[[ "$display_number" =~ ^[0-9]+$ ]] || fail 'Xvfb returned an invalid display number.'
qa_display=:$display_number
[[ "$caller_display" != "$qa_display" && "$caller_display" != "$qa_display.0" ]] \
    || fail 'Refusing to use the caller display.'
qa_env+=("DISPLAY=$qa_display")
run() { timeout --kill-after=2s 10s "${qa_env[@]}" "$@"; }

# Use an explicit small WM configuration; Openbox does not read user shortcuts.
cat > "$qa_tmp/openbox.xml" <<'XML'
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <focus><focusNew>yes</focusNew><followMouse>no</followMouse></focus>
  <placement><policy>Smart</policy></placement>
  <desktops><number>1</number></desktops>
  <keyboard><keybind key="A-F4"><action name="Close"/></keybind></keyboard>
  <theme><name>Clearlooks</name></theme>
</openbox_config>
XML
timeout --kill-after=3s 110s "${qa_env[@]}" openbox --sm-disable --config-file "$qa_tmp/openbox.xml" \
    >"$out/openbox.log" 2>&1 &
wm_pid=$!
for ((attempt=0; attempt<100; attempt++)); do
    if run xdotool get_desktop >/dev/null 2>&1; then break; fi
    kill -0 "$wm_pid" 2>/dev/null || fail "Openbox exited; see $out/openbox.log"
    sleep 0.1
done
run xdotool get_desktop >/dev/null || fail 'Openbox did not become ready.'

# Put the three shortcut targets in the same deck: focusing them must change the
# selected tab, giving both a semantic saved-layout assertion and a visible one.
cat > "$out/input-layout.json" <<'JSON'
{
  "version": 3,
  "panels": {
    "agent-main": {"id":"agent-main","kind":"agent","title":"Conversation"},
    "terminal-main": {"id":"terminal-main","kind":"terminal","title":"Terminal"},
    "tasks-main": {"id":"tasks-main","kind":"tasks","title":"Tasks"},
    "notifications-main": {"id":"notifications-main","kind":"notifications","title":"Notifications"},
    "sidebar-main": {"id":"sidebar-main","kind":"sidebar","title":"Projects"},
    "subagents-home": {"id":"subagents-home","kind":"subagent_region","title":"Subagents"},
    "diff-main": {"id":"diff-main","kind":"diff","title":"Diff"}
  },
  "main": {"root":{"type":"tabs","panels":["agent-main","terminal-main","tasks-main","notifications-main","sidebar-main","subagents-home","diff-main"],"active":0},"floating":[]},
  "extra_windows": []
}
JSON
window_title="evorch-native-qa-$$-${RANDOM}"
app_args=(--window-title "$window_title" --layout "$out/input-layout.json"
    --save-layout "$qa_tmp/saved-layout.json")
if [[ "$mode" == runtime ]]; then
    app_args+=(--demo --settings "$qa_tmp/ui.toml" --state "$qa_tmp/sidebar.json")
fi
(
    cd "$qa_tmp/work"
    exec timeout --kill-after=3s 90s "${qa_env[@]}" "$gui_bin" "${app_args[@]}"
) >"$out/app.log" 2>&1 &
app_pid=$!
for ((attempt=0; attempt<150; attempt++)); do
    window=$(run xdotool search --onlyvisible --name "^${window_title}$" 2>/dev/null || true)
    [[ "$window" =~ ^[0-9]+$ ]] && break
    kill -0 "$app_pid" 2>/dev/null || fail 'GUI executable exited before creating its window.'
    sleep 0.1
done
[[ "$window" =~ ^[0-9]+$ ]] || fail 'Expected one visible GUI window within 15 seconds.'
run xdotool windowactivate --sync "$window"
run xdotool mousemove 1590 990

capture() {
    local name=$1 colors
    # Give asynchronous render/input two repaint periods; bounded retries below
    # additionally wait for the semantic state, rather than relying on this delay.
    sleep 0.5
    run "${capture_command[@]}" -window "$window" "$out/$name.png"
    colors=$(run "${image_command[@]}" "$out/$name.png" -format '%k' info:)
    [[ "$colors" =~ ^[0-9]+$ && "$colors" -gt 16 ]] || fail "Blank/unrendered screenshot: $name"
}
save_and_assert_active() {
    local panel=$1 name=$2
    rm -f "$qa_tmp/saved-layout.json"
    # The app dispatches one shortcut per frame. Give the preceding focus/click
    # its own frame before saving, and retry the idempotent save if rendering lags.
    sleep 0.25
    run xdotool key --clearmodifiers ctrl+s
    for ((attempt=0; attempt<40; attempt++)); do
        if [[ -s "$qa_tmp/saved-layout.json" ]] && jq -e --arg panel "$panel" \
            '[.. | objects | select(.type? == "tabs") | .panels[.active]] | index($panel) != null' \
            "$qa_tmp/saved-layout.json" >/dev/null 2>&1; then
            cp "$qa_tmp/saved-layout.json" "$out/$name.json"
            return
        fi
        sleep 0.2
        run xdotool key --clearmodifiers ctrl+s
    done
    fail "Expected selected panel $panel after input; layout was not saved or tab did not change."
}
assert_changed() {
    local before=$1 after=$2 changed
    changed=$(run "${image_command[@]}" "$out/$before.png" "$out/$after.png" \
        -compose difference -composite -colorspace gray -threshold 0 -format '%[fx:mean*w*h]' info:)
    awk -v pixels="$changed" 'BEGIN { exit !(pixels >= 100) }' \
        || fail "Expected visible change between $before and $after, found $changed changed pixels."
    printf '%s -> %s: %s changed pixels\n' "$before" "$after" "$changed" >> "$out/assertions.log"
}

# Ctrl+1/2/3 must reach real GUI input dispatch, and Ctrl+S must persist the result.
run xdotool key --clearmodifiers ctrl+1
save_and_assert_active agent-main 01-conversation
capture 01-conversation
run xdotool key --clearmodifiers ctrl+2
save_and_assert_active terminal-main 02-terminal
capture 02-terminal
assert_changed 01-conversation 02-terminal
run xdotool key --clearmodifiers ctrl+3
save_and_assert_active tasks-main 03-tasks
capture 03-tasks
assert_changed 02-terminal 03-tasks
run xdotool key --clearmodifiers ctrl+1
save_and_assert_active agent-main 04-conversation
capture 04-conversation
assert_changed 03-tasks 04-conversation

# At 96 DPI the first tab is below the diagnostics button; click its neighbour.
# Saved layout makes a misplaced click fail instead of accepting any changed pixel.
run xdotool mousemove --window "$window" 170 38 click 1
run xdotool mousemove 1590 990
save_and_assert_active terminal-main 05-mouse-terminal
capture 05-mouse-terminal
assert_changed 04-conversation 05-mouse-terminal

run xdotool key --clearmodifiers ctrl+shift+r
sleep 0.5
save_and_assert_active agent-main 06-default-workbench
jq -e '.main.root.type == "split"' "$out/06-default-workbench.json" >/dev/null \
    || fail 'Ctrl+Shift+R did not restore the default split layout.'
capture 06-default-workbench
assert_changed 05-mouse-terminal 06-default-workbench

# Request a size below the supported floor; the WM must clamp the client area.
run xdotool windowsize "$window" 640 480
sleep 0.5
run xdotool getwindowgeometry --shell "$window" > "$out/window-geometry.txt"
width=$(sed -n 's/^WIDTH=//p' "$out/window-geometry.txt")
height=$(sed -n 's/^HEIGHT=//p' "$out/window-geometry.txt")
[[ "$width" =~ ^[0-9]+$ && "$height" =~ ^[0-9]+$ ]] || fail 'Window size could not be read.'
(( width == 960 && height == 600 )) || fail "Expected WM-clamped client size 960x600, got ${width}x${height}."
capture 07-minimum-size
printf 'Minimum client size: %sx%s\n' "$width" "$height" >> "$out/assertions.log"

# The explicit Openbox close binding sends WM_DELETE_WINDOW to eframe.
# xdotool windowclose destroys the X window and does not exercise clean close.
run xdotool key --clearmodifiers alt+F4
app_result=0
wait "$app_pid" || app_result=$?
app_pid=
(( app_result == 0 )) || fail "GUI close returned $app_result (124 indicates timeout)."
printf 'PASS: startup, rendered pixels, keyboard/mouse tabs, saved/reset layout, minimum size, clean close.\n' \
    | tee "$out/result.txt"
printf 'Evidence: %s\n' "$out"
