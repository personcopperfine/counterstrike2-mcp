# =============================================================================
# client-mcp: the preamble shared by the Linux CS2 sessions.
#
# Sourced (not executed) by docker/linux-client.sh and docker/linux-dedicated.sh, which mount it
# read-only at /cs2mcp/lib-preamble.sh. Sourcing has no side effects: it only defines functions.
#
# The caller must set, before calling anything here:
#   G        game dir inside the cs2-home volume (CS2_GAME_DIR)
#   B        G/bin/linuxsteamrt64
#   GSE_SAVE_DIR, OUT  (see the scripts)
#
# Functions, in the order the scripts call them:
#   cs2mcp_lock             take the home-volume flock (fd 9); the caller releases it
#   cs2mcp_unlock           release it
#   cs2mcp_preamble_fix     restore-then-fix + gbe + video identity + ICD (run-cs2.sh:15-65 ...)
#   cs2mcp_preamble_env     env exports + video config overrides (run-cs2.sh:113-145)
#
# Why the lock. A client and a dedicated server can share one cs2-home volume, so both run
# cs2mcp_preamble_fix against the same game dir. The fix steps move files (`mv`) and make links
# and directories. Each run restores the bundled files first and then moves libfreetype aside again
# (RC-28, restore-then-fix), so a second run logs RESTORED / MOVED-ASIDE again; the end state is the
# same whichever run went last. Without serialising, two runs can interleave those moves: one run's
# `mv` finds the file already gone and fails. The flock makes the runs take turns; each finds the
# same end state. ln -sf, mkdir -p and the gbe config writes are repeatable as well.
#
# Output markers (grep-able in podman logs): PREAMBLE-LOCK-HELD, PREAMBLE-LOCK-TIMEOUT,
# PREAMBLE-LOCK-UNAVAILABLE (no flock binary: the steps run unserialised, as before).
# =============================================================================

# The lock file lives in the home volume so that every container mounting it sees the same lock.
CS2MCP_LOCK="${CS2MCP_LOCK:-/home/steam/.cs2mcp-preamble.lock}"
# How long a session waits for the lock before it goes on unlocked (a stuck peer must not hang us).
CS2MCP_LOCK_WAIT="${CS2MCP_LOCK_WAIT:-120}"
# The shader cache volume the game's shadercache link points at (harness: /cache).
CS2MCP_SHADER_DIR="${CS2MCP_SHADER_DIR:-/cache}"

cs2mcp_lock() {
    mkdir -p "$(dirname "$CS2MCP_LOCK")" 2>/dev/null
    if ! command -v flock >/dev/null 2>&1; then
        echo "PREAMBLE-LOCK-UNAVAILABLE (no flock binary): continuing WITHOUT lock"
        return 0
    fi
    # probe in a subshell first: a failing `exec` redirection in the main shell would end the script
    if ! ( exec 9>"$CS2MCP_LOCK" ) 2>/dev/null; then
        echo "PREAMBLE-LOCK-UNAVAILABLE (cannot open $CS2MCP_LOCK): continuing WITHOUT lock"
        return 0
    fi
    exec 9>"$CS2MCP_LOCK"
    if flock -w "$CS2MCP_LOCK_WAIT" 9; then
        echo "PREAMBLE-LOCK-HELD $CS2MCP_LOCK"
    else
        echo "PREAMBLE-LOCK-TIMEOUT $CS2MCP_LOCK after ${CS2MCP_LOCK_WAIT}s: continuing WITHOUT lock"
    fi
}

# Create or repoint a symlink only when it does not already point at $1. A live peer may be using
# the link, so an unchanged link is left alone (no rewrite, no output).
cs2mcp_link() {
    local target="$1" path="$2"
    if [ "$(readlink "$path" 2>/dev/null)" != "$target" ]; then
        ln -sfn "$target" "$path"
        echo "LINKED $path -> $target"
    fi
}

cs2mcp_unlock() {
    # closing fd 9 releases the flock; it must be closed before any child (the game) starts
    exec 9>&-
}

cs2mcp_preamble_fix() {
echo "=== [0] restore-then-fix (RC-28) === (run-cs2.sh:15-26)"
# run-cs2.sh:16-19: put back any file a previous run moved aside
for f in "$B"/*.game-bundled; do
  [ -e "$f" ] || continue
  # a peer game that is running may have libfreetype.so.6 moved aside; putting the bundled copy
  # back under it would change a live process's library set, so this one stays aside
  [ "$(basename "$f")" = "libfreetype.so.6.game-bundled" ] && continue
  mv "$f" "${f%.game-bundled}"; echo "RESTORED $(basename "$f")"
done
# run-cs2.sh:20-22: the bundled libfreetype is replaced by the system one (LD_PRELOAD below)
if [ -f "$B/libfreetype.so.6" ]; then
  mv "$B/libfreetype.so.6" "$B/libfreetype.so.6.game-bundled"; echo "MOVED-ASIDE libfreetype.so.6"
fi
# run-cs2.sh:23-25: pango/fontconfig dev symlinks (repointed only when they differ)
cs2mcp_link libpangoft2-1.0.so.0 "$B/libpangoft2-1.0.so"
cs2mcp_link libpango-1.0.so.0    "$B/libpango-1.0.so"
cs2mcp_link libfontconfig.so.1   "$B/libfontconfig.so"
echo DEV-SYMLINKS-CREATED

echo "=== [0b] shadercache === (run-cs2.sh:28-29)"
# run-cs2.sh:29: the game's shadercache dir points at the cs2-shadercache volume (/cache). Only
# recreated when it does not already point there (a peer may be using it)
if [ "$(readlink "$G/csgo/shadercache" 2>/dev/null)" != "$CS2MCP_SHADER_DIR" ]; then
  rm -rf "$G/csgo/shadercache"; ln -sfn "$CS2MCP_SHADER_DIR" "$G/csgo/shadercache"
  echo "LINKED $G/csgo/shadercache -> $CS2MCP_SHADER_DIR"
fi
mkdir -p "$CS2MCP_SHADER_DIR/vulkan"

echo "=== [0c] gbe portable saves === (run-cs2.sh:31-44)"
UCFG="$HOME/.steam/sdk64/steam_settings/configs.user.ini"
mkdir -p "$(dirname "$UCFG")"; touch "$UCFG"
if grep -qE '^[[:space:]]*local_save_path[[:space:]]*=' "$UCFG"; then
    sed -i -E "s|^[[:space:]]*local_save_path[[:space:]]*=.*|local_save_path=$GSE_SAVE_DIR|" "$UCFG"
else
    printf '\n[user::saves]\nlocal_save_path=%s\n' "$GSE_SAVE_DIR" >> "$UCFG"
fi
echo "PORTABLE-SAVES local_save_path=$GSE_SAVE_DIR ($UCFG)"
# run-cs2.sh:41-44: the emu looks for steam_settings next to the loaded .so
BN_ST="$B/steam_settings"
if [ -d "$HOME/.steam/sdk64/steam_settings" ] && [ "$(readlink "$BN_ST" 2>/dev/null)" != "$HOME/.steam/sdk64/steam_settings" ]; then
  ln -sfn "$HOME/.steam/sdk64/steam_settings" "$BN_ST"; echo "LINKED $BN_ST -> $HOME/.steam/sdk64/steam_settings"
fi
# run-cs2.sh:45-51: seed the save-dir video config from the game's default when missing
SVCFG="$GSE_SAVE_DIR/730/local/cfg/cs2_video.txt"
if [ ! -f "$SVCFG" ]; then
  mkdir -p "$(dirname "$SVCFG")"
  for src in "/home/steam/.local/share/GSE Saves/730/local/cfg/cs2_video.txt" /app/steam_settings/cs2_video.default.txt; do
    if [ -f "$src" ]; then cp "$src" "$SVCFG"; echo "SEEDED $SVCFG from $src"; break; fi
  done
fi

echo "=== [0d] gbe setup === (client/entrypoint.sh:112)"
# client/entrypoint.sh:112 always runs the image's gbe setup before launching; run-cs2.sh does not.
[ -x /app/setup_gbe.sh ] && /app/setup_gbe.sh || echo "SETUP-GBE-SKIPPED (/app/setup_gbe.sh missing)"

echo "=== [0e] video identity + low preset === (client/entrypoint.sh:126-196)"
# client/entrypoint.sh:131-137: llvmpipe = PCI vendor 0x10005 (65541), device 0; a detected
# NVIDIA card (--gpus) wins. A stale identity makes CS2 re-run autoconfig (HDR -> black screen).
vendor_id=65541; device_id=0
dev_info="$(nvidia-smi --query-gpu=pci.device_id --format=csv,noheader 2>/dev/null || true)"
dev_info="${dev_info%%$'\n'*}"
dev_info="${dev_info//[[:space:]]/}"
if [ -n "$dev_info" ]; then
    vendor_id=$(( dev_info & 0xFFFF ))
    device_id=$(( dev_info >> 16 ))
fi
# client/entrypoint.sh:141-164: install-dir cfg seeded at the low preset (0) when missing
CFG_DIR="$G/csgo/cfg"; VIDEO_CFG="$CFG_DIR/cs2_video.txt"
if [ ! -f "$VIDEO_CFG" ] && [ -f "/app/steam_settings/video_defaults/video_defaults_0.txt" ]; then
    mkdir -p "$CFG_DIR"
    {
        echo '"config"'
        echo '{'
        printf '\t"Version"\t\t\t"16"\n'
        printf '\t"VendorID"\t\t\t"%d"\n' "$vendor_id"
        printf '\t"DeviceID"\t\t\t"%d"\n' "$device_id"
        tr -d '\r' < "/app/steam_settings/video_defaults/video_defaults_0.txt" \
            | sed -e '/^[[:space:]]*{/d' -e '/^[[:space:]]*}/d' -e '/^"config"/d'
    } > "$VIDEO_CFG"
    echo "seeded low preset into $VIDEO_CFG (vendor=$vendor_id device=$device_id)"
fi
# client/entrypoint.sh:173-196: the save-dir cfg gets the same identity stamp
if [ -f "$SVCFG" ]; then
    sed -i -E "s/(\"VendorID\"[[:space:]]+\")[0-9]+(\")/\1${vendor_id}\2/;
               s/(\"DeviceID\"[[:space:]]+\")[0-9]+(\")/\1${device_id}\2/" "$SVCFG" \
        && echo "device identity stamped into $SVCFG (vendor=$vendor_id device=$device_id)"
fi

echo "=== [1a] ICD install (LVP_ICD: none|image|build) === (run-cs2.sh:53-65)"
# run-cs2.sh:54-58; default none: nothing to install unless the caller sets LVP_ICD
case "${LVP_ICD:-none}" in
  image) SRC=/artifacts/lavapipe ;;
  build) SRC=/build/src/gallium/targets/lavapipe ;;
  *)     SRC= ;;
esac
if [ -n "$SRC" ]; then
  mkdir -p /usr/local/lib/x86_64-linux-gnu /usr/share/vulkan/icd.d
  cp "$SRC/libvulkan_lvp.so" /usr/local/lib/x86_64-linux-gnu/libvulkan_lvp.so               # run-cs2.sh:61
  cp "$SRC/lvp_icd.x86_64.json" /usr/share/vulkan/icd.d/lvp_cs2_fork.x86_64.json            # run-cs2.sh:62
  export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_cs2_fork.x86_64.json                   # run-cs2.sh:63
  echo "ICD-INSTALLED $(sha256sum /usr/local/lib/x86_64-linux-gnu/libvulkan_lvp.so | cut -c1-16)"
fi
}

cs2mcp_preamble_env() {
echo "=== [3] env === (run-cs2.sh:113-124)"
# run-cs2.sh:115: shipped cs2.sh aborts unless VERSION_CODENAME=sniper (T17)
sed -i 's/^VERSION_CODENAME=.*/VERSION_CODENAME=sniper/' /etc/os-release
export SDL_VIDEO_DRIVER=x11 ENABLE_PATHMATCH=1                                              # run-cs2.sh:116
export LP_NUM_THREADS=${LP_NUM_THREADS:-12}                                                 # run-cs2.sh:117
export SHADER_CACHE=/cache DXVK_STATE_CACHE_PATH=/cache                                     # run-cs2.sh:118
export MESA_SHADER_CACHE_DIR=/cache MESA_SHADER_CACHE_MAX_SIZE=8G                           # run-cs2.sh:119
export XDG_CACHE_HOME=/cache                                                                # run-cs2.sh:120
ulimit -n 65535                                                                             # run-cs2.sh:121
export LD_PRELOAD="libasound.so.2 libpulse.so.0"                                            # run-cs2.sh:122
# the s2sdk probe (docker.rs: CS2MCP_PRELOAD env, probe dir mounted at /cs2mcp/s2sdk) is prepended
# when the session was launched with one, so it loads before the audio libs. Both Linux scripts
# get it from here (this block runs before cs2.sh in each) and nothing below re-exports
# LD_PRELOAD. The export above is never empty, so no trailing space is produced.
# The Metamod loader (csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so) is NOT exported
# here on purpose: each script prepends it to LD_PRELOAD on the cs2.sh command only, so the rest
# of the session (the preamble's helpers, tee, the tail) does not carry it.
export LD_PRELOAD="${CS2MCP_PRELOAD:+$CS2MCP_PRELOAD }${LD_PRELOAD}"

echo "=== [3b] video config overrides === (run-cs2.sh:126-145)"
# only acts when a CS2_VIDEO_* knob is set; same keys and sed as the harness
VCFGS=("/home/steam/.local/share/GSE Saves/730/local/cfg/cs2_video.txt" "$GSE_SAVE_DIR/730/local/cfg/cs2_video.txt")
CS2_VIDEO_HDR=${CS2_VIDEO_HDR:-0}
for kv in "CS2_VIDEO_HDR:setting.videocfg_hdr_detail" "CS2_VIDEO_MSAA:setting.msaa_samples" "CS2_VIDEO_FSR:setting.videocfg_fsr_detail"; do
  knob=${kv%%:*}; key=${kv#*:}
  val=$(eval echo "\${$knob:-}")
  [ -n "$val" ] || continue
  for VCFG in "${VCFGS[@]}"; do
    if [ -f "$VCFG" ]; then
      sed -i -E "s/(\"$key\"[[:space:]]+\")[0-9]+(\")/\\1$val\\2/" "$VCFG" \
        && echo "$knob=$val set in cs2_video.txt ($key, $VCFG)" \
        || echo "$knob=$val FAILED ($VCFG)"
    fi
  done
done
}
