# rtsp-generator

Detects V4L2 webcams attached to a Linux host and publishes each one as an RTSP stream via
[MediaMTX](https://github.com/bluenviron/mediamtx), supervising MediaMTX as a child process and
reacting live to USB hotplug/unplug. Also writes a `streams.yaml` reference file so other systems
on the LAN can discover each camera's RTSP URL.

Binary name: `rtsp-gen`. LAN-only trust model — no auth/TLS on the RTSP streams.

**Encoding**: cameras that natively capture H.264 are passed through untouched (`-c copy`).
Everything else — MJPEG (the common case for most UVC webcams), raw YUYV/NV12, etc. — is
transcoded to H.264 with `libx264 -preset ultrafast -tune zerolatency`. This is a deliberate
deviation from straight passthrough: ffmpeg's RTP-JPEG payloader (RFC 2435) is unreliable for
real-world MJPEG streams (see "Why not MJPEG passthrough?" below), so H.264 is the only encoding
that reliably produces a stream RTSP clients can actually play.

## Build (Debian)

These steps take a fresh Debian install (tested on Debian 13 "trixie", should work on any
reasonably current Debian-family release) to a built `rtsp-gen` binary.

### 1. Install build tools and dependencies

```
sudo apt update
sudo apt install build-essential pkg-config libssl-dev libudev-dev curl git
```

What each package is for:

- `build-essential` — C compiler and linker, needed to build the native (non-Rust) parts of a
  few dependencies below.
- `pkg-config` — lets those dependencies' build scripts locate the libraries below.
- `libssl-dev` — OpenSSL headers, needed by `reqwest` (used to talk to the MediaMTX control API)
  via its default TLS backend.
- `libudev-dev` — needed by the `udev` crate, used for hotplug watching.
- `curl` — only needed to fetch the Rust installer in the next step; skip it if you already have
  one.
- `git` — only needed if you're cloning this repository rather than copying the source another
  way.

### 2. Install Rust

This project needs a fairly recent Rust — it uses `std::fs::File::try_lock`, stabilized in Rust
**1.89**. Debian's own packaged `rustc` lags well behind that (Debian 13 ships 1.85), so
`apt install rustc cargo` is **not** enough on its own and the build will fail with a missing-method
error on an older toolchain. Install the current stable toolchain via
[rustup](https://rustup.rs) instead:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Accept the default installation options when prompted. Verify with `rustc --version` — anything
1.89 or newer works.

### 3. Get the source and build

```
git clone <this-repository-url>
cd rtsp-generator
cargo build --release
```

The resulting binary is at `target/release/rtsp-gen`. A clean release build takes a couple of
minutes on typical hardware (longer on something Pi-class).

### 4. Install it

```
sudo install -m 755 target/release/rtsp-gen /usr/local/bin/rtsp-gen
```

MediaMTX and ffmpeg are separate runtime dependencies, not needed to *build* this project — see
"Runtime dependencies" below for installing those.

## Runtime dependencies

- **MediaMTX** must be installed separately and discoverable — either on `$PATH` as `mediamtx`,
  or via `mediamtx_binary: /path/to/mediamtx` in `config.yaml`. It is not vendored. Download a
  release for your architecture from
  [github.com/bluenviron/mediamtx/releases](https://github.com/bluenviron/mediamtx/releases) and
  place the binary somewhere on `$PATH` (e.g. `/usr/local/bin/mediamtx`).
- **ffmpeg** must be on `$PATH` — MediaMTX shells out to it (via the generated `runOnInit`
  commands) to pull frames from each V4L2 device and push them into MediaMTX over RTSP.

If the MediaMTX binary can't be found, `rtsp-gen` exits with code `3` and an actionable error
message rather than failing silently.

### Why not MJPEG passthrough?

The obvious design for most webcams (which capture MJPEG) is to push the JPEG frames straight
into RTP with `-c copy` and no re-encoding. In testing against a real UVC webcam (Logitech C920),
this reliably failed: MediaMTX's inbound-frame-error counter climbed by thousands per second with
zero bytes ever accepted, and ffmpeg logged `RFC 2435 suggests two quantization tables, 3
provided` — this camera's JPEG frames use 3 quantization tables, but ffmpeg's RTP-JPEG payloader
only supports 2. Re-encoding to force exactly 2 tables traded that error for a second one, `Only
1x1 chroma blocks are supported`, which persisted even when the re-encode was forced to 4:4:4
chroma (`-pix_fmt yuvj444p`) — the exact format the error message asks for. This is a known
fragility in ffmpeg's built-in RTP-JPEG muxer, not a misconfiguration, and it isn't specific to
this one camera model. Transcoding to H.264 avoids ffmpeg's RTP-JPEG code path entirely and is
what most webcam→RTSP tools do for this reason.

## Camera names

Each camera's stable name (used as its RTSP path segment, e.g.
`rtsp://<host>:8554/hd-pro-webcam-c920-antelope`) is derived from its `/dev/v4l/by-id/` symlink
with the USB vendor ID and serial number stripped — neither means anything to a human — and
replaced with a short animal name, e.g. `usb-046d_HD_Pro_Webcam_C920_7438C0DF-video-index0` ->
`hd-pro-webcam-c920-antelope`.

The animal isn't random per run: it's a deterministic hash of the camera's actual serial number,
so the same physical camera always gets the same name across reboots and replugs (important,
since it's also usable as a `devices:` key in `config.yaml` and shows up in `streams.yaml`).
Two identical camera models get different animals because they have different serials; in the
rare case two cameras somehow hash to the same animal, the existing USB-port-based (or numeric)
disambiguation still applies on top.

## CLI

```
rtsp-gen                      # full run: detect, generate config, start MediaMTX, write
                               # streams.yaml, then block watching for hotplug events
                               # (this is what systemd runs)
rtsp-gen --list [--all] [--json]  # detect and print webcams; no side effects. Default: name,
                                   # current resolution/fps, RTSP URL. --all additionally shows
                                   # device path and every supported resolution.
rtsp-gen --status [--json]    # systemctl status; --json additionally queries the MediaMTX
                               # control API for per-path health
rtsp-gen --info [--json]      # per-camera active encoder (hardware backend or software) and
                               # current CPU usage of the rtsp-gen/MediaMTX/ffmpeg process tree
rtsp-gen --restart            # systemctl restart rtsp-generator
rtsp-gen --stop               # systemctl stop rtsp-generator
rtsp-gen --install-service    # write the systemd unit, daemon-reload, enable (idempotent)
rtsp-gen --uninstall-service  # disable, remove the unit, daemon-reload (idempotent)
rtsp-gen --about              # version / target / license / repo

# modifiers, usable with the above:
  -c, --config <path>     default: /etc/rtsp-generator/config.yaml
  -o, --output <path>     where to write streams.yaml; see "Where streams.yaml is stored" below
                           if omitted
      --res <WxH>         override resolution (global unless paired with --device)
  -d, --device <name>     scope --res/--fps to one camera by its stable name
      --fps <n>           override framerate
  -p, --port <n>          RTSP port (default 8554)
      --json              machine-readable output for --list / --status / --info
      --all               with --list, show full device capabilities instead of just
                           the current setting
      --dry-run           show what would happen; never writes files, spawns processes,
                           or calls the MediaMTX API
  -v, -vv, -vvv           increase log verbosity
```

Exit codes: `0` success, `1` general error, `2` invalid arguments, `3` MediaMTX binary not found,
`4` no capture-capable devices found (`--list` / default run only).

## Single instance

The default (no-flag) run takes an exclusive lock on `/var/lib/rtsp-generator/rtsp-gen.lock`
before doing anything else. If another instance already holds it (typically the systemd service),
it refuses to start a second one — which would otherwise fight over RTSP/API/RTP ports and
crash-loop — and instead prints a readout of what's already running (how it's managed, RTSP port,
each camera's live RTSP URL, whether the MediaMTX API is reachable) and exits with code `1`. Use
`rtsp-gen --status`/`--restart`/`--stop` to interact with the already-running instance instead.

`--info` uses this same lock to find the running instance (whether started by systemd or by
hand) and report its live state — active encoder per camera, and CPU usage of the
rtsp-gen/MediaMTX/ffmpeg process tree via `ps`. It checks the lock is actually held, not just that
the lock file has *some* pid in it — the file's last-recorded pid persists after a clean exit, so
a naive read would misreport a stopped instance as running. If nothing holds the lock, `--info`
reports that and exits with code `1`.

### Changing settings while it's already running

`--res`/`--fps`/`--device`/`--port` only ever apply to the foreground run they're attached to —
but that run refuses to start while another instance holds the lock, so those flags would
otherwise be silently ignored with no indication anything was wrong. Instead:

- `--device <name>` with `--res`/`--fps`: the override is saved into `config.yaml`'s `devices:`
  section for you, and the report tells you to run `rtsp-gen --restart` to apply it.
  ```
  $ sudo rtsp-gen --device hd-pro-webcam-c920-antelope --fps 10
  rtsp-generator is already running — refusing to start a second instance.
  ...
  ---
  rtsp-generator is already running, so this override was saved to /etc/rtsp-generator/config.yaml
  instead of applying immediately:
    hd-pro-webcam-c920-antelope:
      fps: 10
  Run `rtsp-gen --restart` to apply it.
  ```
- `--res`/`--fps`/`--port` **without** `--device` (a global override): there's no well-defined
  place to persist a global override automatically, so it just explains that and tells you to
  edit `config.yaml` directly and restart.
- `--dry-run` previews what would be saved without writing anything, same as everywhere else.

## Where streams.yaml is stored

Resolved in this order, on every default (no-flag) run:

1. `--output <path>` on the command line — always wins, never remembered for next time.
2. Whatever was resolved and saved to `config.yaml` (`streams_path:`) on a previous run.
3. First run, nothing configured yet:
   - **Interactive** (you're running `rtsp-gen` at a terminal): prompts —
     `Where would you like to store the streams.yaml reference file? [~/.rtsp-gen]:` — press enter
     to accept the default (`~/.rtsp-gen/streams.yaml`) or type a path/directory. The answer is
     saved to `config.yaml` so you're only asked once.
   - **Non-interactive** (no controlling terminal — this is how systemd runs it): silently uses
     `/etc/rtsp-generator/streams.yaml` and saves that choice to `config.yaml`, so the installed
     service's behavior doesn't change.

`--dry-run` resolves the same way but never prompts and never writes `config.yaml`.

## config.yaml

```yaml
rtsp_port: 8554
mediamtx_binary: /usr/local/bin/mediamtx
advertise_ip: null            # null = auto-detect
exclude_interfaces: ["docker0", "br-", "veth", "tailscale0", "zt"]
streams_path: /etc/rtsp-generator/streams.yaml  # set automatically on first run; see above
encoding:
  hardware: auto                # auto | vaapi | qsv | v4l2m2m | software — see "Hardware
                                 # encoding" below
  preset: ultrafast              # x264 preset (software fallback only); already the
                                 # fastest/lowest-CPU option x264 has
  bitrate_kbps: null             # null = default rate control; set a number (e.g. 1500) to cap
                                 # bandwidth with -b:v/-maxrate/-bufsize, on any backend
devices:
  logitech-c920:
    resolution: "1280x720"
    fps: 30
```

If this file doesn't exist, built-in defaults are used (nothing is written implicitly).
`--install-service` writes out a default copy if one isn't already present, so there's something
to edit.

Override precedence per camera (highest wins): `--device <name> --res/--fps` on the command line,
then that camera's entry under `devices:` in config.yaml, then a global (no `--device`) `--res`/
`--fps` flag, then the built-in auto-selection: the largest resolution at or below **720p** that
supports >= 15fps (capped there deliberately to bound H.264 transcode CPU cost — see "Encoding"
above), falling back to progressively looser tiers if a device can't meet that (e.g. a
1080p/4K-only camera falls back to its smallest mode; a camera with no 720p mode falls back to
its largest sub-720p mode).

### Hardware encoding

`encoding.hardware` defaults to `auto`: on every run, before generating the MediaMTX config,
`rtsp-gen` probes for a working hardware H.264 encoder and prefers it over software whenever one
verifies as actually usable, in this order:

1. **VAAPI** (`h264_vaapi`) — the generic Linux hardware video path (Intel and AMD GPUs), via
   `/dev/dri/renderD128`.
2. **Intel Quick Sync** (`h264_qsv`) — often more efficient than VAAPI specifically on Intel, but
   needs an ffmpeg build with `libmfx`/`oneVPL` support, which isn't always present.
3. **V4L2 M2M** (`h264_v4l2m2m`) — embedded SoC encoders exposed via Linux's V4L2 memory-to-memory
   API (e.g. Raspberry Pi 4 and earlier's hardware encoder; notably **absent on Raspberry Pi 5**,
   which dropped the hardware H.264 encode block).
4. **Software** (`libx264`) — the universal fallback.

Each candidate is verified with a real trial encode, not just checked for presence — `ffmpeg
-encoders` will happily list `h264_vaapi` even when the driver, permissions, or hardware don't
actually support it. Even that isn't fully conclusive: a hardware encoder can pass the synthetic
trial and still fail against the real capture pipeline (this is exactly the kind of gap that
caused the MJPEG-over-RTP bug documented above), so `rtsp-gen` also checks, a few seconds after
starting, that each transcoded camera's MediaMTX path is actually receiving bytes — and if not,
automatically regenerates the config for software encoding and restarts, logging why. The same
check runs again for any camera added later via hotplug.

Set `encoding.hardware` to `vaapi`, `qsv`, or `v4l2m2m` to force a specific backend (falls back to
software with a warning if it doesn't verify), or `software` to skip probing entirely.

### Startup latency (why a viewer takes a few seconds to connect)

For transcoded (non-passthrough) cameras, ffmpeg is told to force a keyframe every 2 seconds
(`-g`, and `-keyint_min`/`-sc_threshold 0` on the software backend). This isn't a buffer size
setting — it's the encoder's keyframe interval (GOP length), and it matters because an RTSP
client (VLC, ffplay, etc.) can't render *any* video until it receives one full keyframe; every
frame in between is a delta encoded against previous frames and is useless on its own. Without an
explicit `-g`, libx264 defaults to a **250-frame** GOP — at 10fps that's 25 seconds between
keyframes, and a new viewer joining at a random point waits 12.5 seconds on average, worst case
25. Critically, that default is a frame *count*, not a duration, so lowering `--fps` to reduce CPU
load makes the wall-clock wait even longer, not shorter. Forcing a 2-second interval bounds join
latency to about 2 seconds regardless of the configured framerate, at the cost of a modest bitrate
increase (keyframes are larger than delta frames). Passthrough (`-c copy`) cameras aren't affected
— there's no encoder here to configure, so join latency there depends entirely on the camera's own
native keyframe interval.

### Lowering CPU further

Beyond getting a hardware encoder into the picture, `preset` is already at x264's floor
(`ultrafast`) for the software fallback, so the remaining levers, in order of impact, are:

- **Resolution/fps**: the biggest lever for software encoding. Use `--res`/`--fps` or a
  `devices:` entry to drop a specific camera below the 720p default (e.g. `480p@15fps` for a
  low-priority camera).
- **Number of concurrent software-encoded cameras**: each software transcode is a separate ffmpeg
  process pinning close to one CPU core; on a Pi-class device without a working hardware
  encoder, 3-4 simultaneous 720p30 transcodes will likely saturate it.
- **`bitrate_kbps`**: bounds bandwidth/output size, but has little effect on CPU for software
  encoding — a single-pass CRF encode's cost is governed by preset and resolution/fps, not the
  bitrate target. Set it for predictable network usage, not as a CPU lever.

## systemd

`--install-service` generates `/etc/systemd/system/rtsp-generator.service` running as
`User=root`. This is a v1 simplification, not the end state: the daemon needs udev netlink access
and read/write access to `/dev/video*`, `/etc/rtsp-generator`, and `/var/lib/rtsp-generator`. A
dedicated non-root user in the `video` group (plus a udev rule granting netlink permissions) would
be preferable from a least-privilege standpoint — flagged here as a deliberate follow-up rather
than solved in v1.

## Extras

`scripts/rtsp-gen-tui.sh` is an optional [gum](https://github.com/charmbracelet/gum)-based
terminal menu covering the CLI end to end (list/status/info, changing a camera's resolution/fps,
installing/restarting/stopping/uninstalling the service, running in the foreground for
debugging, and editing `config.yaml`) — for people who'd rather navigate a menu than remember
flags. It's a thin wrapper: every action just shells out to the real `rtsp-gen` binary (with
`sudo` for the actions that need it), so it stays correct by construction rather than duplicating
any logic. Not part of the build — needs `gum` installed and a real terminal (it won't run
piped/non-interactively). Run it with:

```
gum --version || see https://github.com/charmbracelet/gum#installation
./scripts/rtsp-gen-tui.sh
```

## Manual test steps

Integration testing against real webcams is out of scope for CI. To test by hand:

1. **Device enumeration**: plug in N webcams, run `rtsp-gen --list` (and `--list --all --json`),
   and confirm exactly N entries appear, each with a sensible stable name, correct current
   resolution/fps, and a plausible RTSP URL. Confirm any metadata-only nodes (e.g. a second
   `/dev/videoN` per physical camera) are excluded, and that `--all` shows every resolution the
   device actually supports.
2. **Full run**: run `rtsp-gen -vv` in the foreground (or `sudo rtsp-gen -vv` if `/etc` and
   `/var/lib` aren't writable by your user). Confirm:
   - `mediamtx.yml` is generated at `/var/lib/rtsp-generator/mediamtx.yml` with one `paths:` entry
     per camera.
   - `streams.yaml` is written at `/etc/rtsp-generator/streams.yaml` (or `--output` path) with the
     correct LAN IP and one `rtsp_url` per camera.
   - Each stream is playable from another host on the LAN: `ffplay rtsp://<host-ip>:8554/<name>`.
3. **Hotplug**: with the daemon running, unplug a webcam and confirm its entry disappears from
   `streams.yaml` and its MediaMTX path stops within a few seconds; replug it and confirm it
   reappears with a working stream, without interrupting other cameras' streams.
4. **Overrides**: run with `--res 640x480 --fps 15 --device <name>` and confirm only that camera
   is affected; add a `devices:` entry in `config.yaml` for the same camera and confirm the config
   entry takes precedence when `--device` isn't passed on that run.
5. **Service management**: `rtsp-gen --install-service`, confirm the unit is enabled and the
   daemon starts on boot; `rtsp-gen --uninstall-service`, confirm the unit is gone and
   `systemctl status rtsp-generator` reports "could not be found".
6. **`--dry-run`**: confirm no files are written and no processes are spawned (compare directory
   contents/mtimes before and after).
7. **`--status --json`**: confirm the output is valid JSON reflecting the live systemd state and
   MediaMTX's actual per-path status.
