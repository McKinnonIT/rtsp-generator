#!/usr/bin/env bash
#
# rtsp-gen-tui.sh — a gum-based TUI front end for rtsp-gen.
#
# This is an optional convenience wrapper, not part of the rtsp-generator build. It just calls
# into the real `rtsp-gen` binary for every action; it has no logic of its own beyond menus and
# prompts. See https://github.com/charmbracelet/gum for gum itself.
#
# Usage: rtsp-gen-tui.sh
#
# Env vars:
#   RTSP_GEN_BIN         path to the rtsp-gen binary (default: "rtsp-gen", resolved via $PATH)
#   RTSP_GEN_CONFIG_PATH path to config.yaml, only used by the "Edit config.yaml" menu item
#                        (default: /etc/rtsp-generator/config.yaml)

set -uo pipefail
# Deliberately no `-e`: this script is one big interactive menu loop where a user pressing Esc,
# Ctrl+C, or choosing "No" makes the just-run gum command exit non-zero on purpose. That's normal
# control flow here, not a failure, so every exit code that matters is checked explicitly instead.

BIN="${RTSP_GEN_BIN:-rtsp-gen}"
CONFIG_PATH="${RTSP_GEN_CONFIG_PATH:-/etc/rtsp-generator/config.yaml}"

require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: '$1' is required but not found on \$PATH." >&2
        if [ "$1" = "gum" ]; then
            echo "Install gum: https://github.com/charmbracelet/gum#installation" >&2
        fi
        exit 1
    fi
}
require gum
require "$BIN"

# gum needs a real terminal on both ends. Without one, `gum choose` et al. print
# "could not open a new TTY" — to stdout, not stderr — which a naive `choice=$(gum choose ...)`
# would silently capture as if it were a real menu selection. Fail fast and clearly instead of
# limping into that.
if [ ! -t 0 ] || [ ! -t 1 ]; then
    echo "error: this is an interactive TUI and needs a real terminal (stdin and stdout must" >&2
    echo "both be a TTY) — it can't run non-interactively (e.g. piped, or via cron)." >&2
    exit 1
fi

title() {
    gum style --border normal --margin "1 0" --padding "0 2" --border-foreground 212 --foreground 212 "$1"
}

warn() {
    gum style --foreground 3 "$1"
}

error() {
    gum style --foreground 1 "$1"
}

pause() {
    gum input --placeholder "Press enter to continue..." >/dev/null 2>&1 || true
}

# Runs a command, shows its combined output in a scrollable pager, and reports its exit code.
# Never treated as a script-ending failure (see the `-e` note above) — the exit code is just
# displayed, since some rtsp-gen exit codes (e.g. "already running") are informational, not
# errors, depending on what the user was trying to do.
capture_and_page() {
    local tmp ec
    tmp=$(mktemp)
    "$@" >"$tmp" 2>&1
    ec=$?
    if [ -s "$tmp" ]; then
        gum pager <"$tmp"
    fi
    rm -f "$tmp"
    gum style --faint "(exit code: $ec)"
    return 0
}

# Names of currently detected cameras, one per line (parses the plain --list table rather than
# requiring jq for --list --json).
list_camera_names() {
    "$BIN" --list 2>/dev/null | tail -n +2 | awk 'NF {print $1}'
}

menu_list_cameras() {
    title "List cameras"
    local args=(--list)
    gum confirm "Show full capabilities (--all)?" && args+=(--all)
    gum confirm "JSON output?" && args+=(--json)
    capture_and_page "$BIN" "${args[@]}"
}

menu_status() {
    title "Status"
    if gum confirm "JSON output?"; then
        capture_and_page "$BIN" --status --json
    else
        capture_and_page "$BIN" --status
    fi
}

menu_info() {
    title "Encoder + CPU info"
    if gum confirm "JSON output?"; then
        capture_and_page "$BIN" --info --json
    else
        capture_and_page "$BIN" --info
    fi
}

menu_about() {
    title "About"
    capture_and_page "$BIN" --about
}

menu_change_camera() {
    title "Change a camera's resolution/fps"
    local names device res fps
    names="$(list_camera_names)"
    if [ -z "$names" ]; then
        error "No cameras detected (is rtsp-generator running, and is a webcam attached?)"
        pause
        return
    fi

    device="$(printf '%s\n' "$names" | gum choose --header "Select a camera")"
    if [ -z "$device" ]; then
        return
    fi

    res="$(gum input --header "New resolution for $device (e.g. 1280x720; blank = don't change)" --placeholder "WIDTHxHEIGHT")"
    fps="$(gum input --header "New fps for $device (blank = don't change)" --placeholder "e.g. 30")"

    if [ -z "$res" ] && [ -z "$fps" ]; then
        warn "Nothing entered; not changing anything."
        pause
        return
    fi

    local args=(--device "$device")
    [ -n "$res" ] && args+=(--res "$res")
    [ -n "$fps" ] && args+=(--fps "$fps")

    warn "This applies immediately if rtsp-gen isn't running as a service yet, or is saved to"
    warn "config.yaml (and needs 'rtsp-gen --restart') if the service is already running."
    if gum confirm "Run: rtsp-gen ${args[*]} ?"; then
        capture_and_page sudo "$BIN" "${args[@]}"
    fi
}

menu_install_service() {
    title "Install & enable service"
    warn "Writes /etc/systemd/system/rtsp-generator.service and enables it (idempotent)."
    if gum confirm "Proceed?"; then
        capture_and_page sudo "$BIN" --install-service
        if gum confirm "Start it now (systemctl start)?"; then
            if sudo systemctl start rtsp-generator; then
                gum style --foreground 2 "Started."
            else
                error "Failed to start — check 'rtsp-gen --status' for details."
            fi
        fi
    fi
}

menu_uninstall_service() {
    title "Uninstall service"
    warn "Disables and stops the service, then removes the systemd unit file."
    if gum confirm "Are you sure?" --default=false; then
        capture_and_page sudo "$BIN" --uninstall-service
    fi
}

menu_restart_service() {
    title "Restart service"
    warn "Briefly interrupts every camera's stream while MediaMTX restarts."
    if gum confirm "Restart now?"; then
        capture_and_page sudo "$BIN" --restart
    fi
}

menu_stop_service() {
    title "Stop service"
    if gum confirm "Stop rtsp-generator now?" --default=false; then
        capture_and_page sudo "$BIN" --stop
    fi
}

menu_run_foreground() {
    title "Run in foreground (debug)"
    warn "Blocks this terminal until you press Ctrl+C. For debugging — for normal use, install"
    warn "and start it as a service instead (see the other menu items)."
    local args=(-vv)
    gum confirm "Use --dry-run (show what would happen, no side effects)?" && args+=(--dry-run)
    if gum confirm "Start now?"; then
        sudo "$BIN" "${args[@]}"
        gum style --faint "(foreground run ended, exit code: $?)"
        pause
    fi
}

menu_edit_config() {
    title "Edit $CONFIG_PATH"
    if [ ! -f "$CONFIG_PATH" ]; then
        warn "No config file yet — creating an empty one (every field is optional and falls"
        warn "back to a built-in default; see the README's config.yaml section for the schema)."
        sudo mkdir -p "$(dirname "$CONFIG_PATH")"
        sudo tee "$CONFIG_PATH" >/dev/null <<'YAML'
# rtsp-generator config.yaml
# Every field is optional; anything left out uses the built-in default.
# See the project README's "config.yaml" section for the full schema.
YAML
    fi
    sudo "${EDITOR:-nano}" "$CONFIG_PATH"
    if gum confirm "Restart the service now to apply changes?"; then
        capture_and_page sudo "$BIN" --restart
    fi
}

main_menu() {
    while true; do
        clear
        title "rtsp-generator control panel"
        choice=$(gum choose --header "Choose an action" \
            "List cameras" \
            "Show status" \
            "Show encoder + CPU info" \
            "Change a camera's resolution/fps" \
            "Install & enable service" \
            "Restart service" \
            "Stop service" \
            "Uninstall service" \
            "Run in foreground (debug)" \
            "Edit config.yaml" \
            "About" \
            "Quit")

        case "$choice" in
            "List cameras") menu_list_cameras ;;
            "Show status") menu_status ;;
            "Show encoder + CPU info") menu_info ;;
            "Change a camera's resolution/fps") menu_change_camera ;;
            "Install & enable service") menu_install_service ;;
            "Restart service") menu_restart_service ;;
            "Stop service") menu_stop_service ;;
            "Uninstall service") menu_uninstall_service ;;
            "Run in foreground (debug)") menu_run_foreground ;;
            "Edit config.yaml") menu_edit_config ;;
            "About") menu_about ;;
            "Quit" | "") break ;;
            *)
                # Shouldn't happen from a real menu selection; a defensive stop rather than an
                # infinite loop if gum ever returns something unexpected (e.g. it lost its TTY
                # mid-session and printed an error string to stdout instead of a choice).
                error "Unexpected menu response ('$choice'); exiting."
                break
                ;;
        esac
    done
}

main_menu
