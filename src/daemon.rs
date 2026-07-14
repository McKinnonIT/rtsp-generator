use std::collections::HashSet;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::cli::{Cli, DEFAULT_OUTPUT_PATH};
use crate::config::{Config, EncodingConfig};
use crate::device::{self, Camera};
use crate::hotplug;
use crate::hwaccel::{self, HwAccel};
use crate::mediamtx::{self, MediaMtxApi};
use crate::netinfo;
use crate::output;
use crate::service;

/// How long to let a newly (re)started MediaMTX/ffmpeg settle before checking whether a
/// hardware encoder is actually producing data, against a real camera rather than the
/// synthetic test source `hwaccel::detect` used.
const HW_HEALTH_CHECK_DELAY: Duration = Duration::from_secs(6);

pub enum RunOutcome {
    Success,
    NoCamerasFound,
    /// Another instance already holds the single-instance lock. Carries a human-readable
    /// readout of what's currently running, for `main.rs` to print as-is.
    AlreadyRunning(String),
}

const LOCK_PATH: &str = "/var/lib/rtsp-generator/rtsp-gen.lock";

/// Returns the pid of the currently-running default-run instance, if any. Verified via the
/// actual flock, not just by reading the lock file's contents: the file's recorded pid persists
/// after a clean exit (nothing deletes the file), but the flock itself is released once the
/// process exits, so a stale pid string alone doesn't mean an instance is still running. Used by
/// `--info` to find the running instance regardless of whether it was started by systemd or by
/// hand.
///
/// Deliberately opens the file read-only (unlike `acquire_single_instance_lock`, which needs
/// read+write to record its own pid): the daemon runs as root and the lock file ends up
/// root-owned at `0644`, so a non-root `--info` invocation can only ever open it for reading.
/// flock() doesn't require a writable fd to take an exclusive lock, so a read-only open is
/// sufficient to probe it.
pub fn running_pid() -> Option<u32> {
    running_pid_at(Path::new(LOCK_PATH))
}

fn running_pid_at(path: &Path) -> Option<u32> {
    let mut file = std::fs::OpenOptions::new().read(true).open(path).ok()?;
    match file.try_lock() {
        Ok(()) => {
            // Nobody else was holding it; release immediately since we were only checking.
            let _ = file.unlock();
            None
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut contents = String::new();
            file.read_to_string(&mut contents).ok()?;
            contents.trim().parse().ok()
        }
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

enum LockOutcome {
    Acquired(std::fs::File),
    AlreadyRunning { pid: Option<u32> },
}

/// Acquires an exclusive, non-blocking lock on `path`, so that a second `rtsp-gen` invocation
/// (started by hand while the systemd-managed instance, or another manual one, is already
/// running) doesn't spawn a competing MediaMTX that immediately dies on port conflicts and
/// crash-loops. The lock is released automatically when the returned `File` is dropped.
fn acquire_single_instance_lock(path: &Path) -> std::io::Result<LockOutcome> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false) // preserve existing contents so a contending process can read our pid
        .read(true)
        .write(true)
        .open(path)?;

    match file.try_lock() {
        Ok(()) => {
            // Record our pid so a contending process can report who's holding the lock, even
            // though flock() itself doesn't expose that.
            file.set_len(0)?;
            file.write_all(std::process::id().to_string().as_bytes())?;
            file.flush()?;
            Ok(LockOutcome::Acquired(file))
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut contents = String::new();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_string(&mut contents)?;
            Ok(LockOutcome::AlreadyRunning {
                pid: contents.trim().parse().ok(),
            })
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

fn try_read_streams_yaml(path: &Path) -> Option<output::StreamsFile> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&contents).ok()
}

/// Builds a human-readable readout of the currently-running instance's config and live camera
/// set, for display when a second `rtsp-gen` invocation is refused.
async fn build_already_running_report(cli: &Cli, config: &Config, pid: Option<u32>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("rtsp-generator is already running — refusing to start a second instance.\n\n");

    let systemd_active = std::process::Command::new("systemctl")
        .args(["is-active", service::SERVICE_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false);

    if systemd_active {
        out.push_str("Managed by: systemd (rtsp-generator.service)\n");
    } else if let Some(pid) = pid {
        let _ = writeln!(out, "Running as: pid {pid} (not managed by systemd)");
    } else {
        out.push_str("Running as: another rtsp-gen process (pid unknown)\n");
    }

    let _ = writeln!(out, "RTSP port:  {}", config.effective_rtsp_port(cli));
    let _ = writeln!(out, "HLS port:   {}", config.effective_hls_port(cli));
    let _ = writeln!(out, "WebRTC port: {}", config.effective_webrtc_port(cli));
    if let Some(binary) = &config.mediamtx_binary {
        let _ = writeln!(out, "MediaMTX:   {}", binary.display());
    }

    let streams_path = config
        .streams_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_PATH));
    let _ = writeln!(out, "Reference:  {}", streams_path.display());

    match try_read_streams_yaml(&streams_path) {
        Some(streams) => {
            let _ = writeln!(out, "\nCameras ({}):", streams.cameras.len());
            for cam in &streams.cameras {
                let _ = writeln!(out, "  - {:<30} {}", cam.name, cam.rtsp_url);
            }
        }
        None => {
            out.push_str("\n(no reference file found yet at that path)\n");
        }
    }

    let api = MediaMtxApi::new(mediamtx::DEFAULT_API_PORT);
    if api.list_paths().await.is_ok() {
        out.push_str("\nMediaMTX API: reachable on :9997\n");
    }

    out.push_str("\nTo check live status:  rtsp-gen --status --json\n");
    out.push_str("To restart it:         rtsp-gen --restart\n");
    out.push_str("To stop it:            rtsp-gen --stop\n");

    out
}

fn write_device_override_lines(
    note: &mut String,
    device_name: &str,
    resolution: &Option<String>,
    fps: Option<u32>,
) {
    use std::fmt::Write as _;
    let _ = writeln!(note, "  {device_name}:");
    if let Some(r) = resolution {
        let _ = writeln!(note, "    resolution: \"{r}\"");
    }
    if let Some(f) = fps {
        let _ = writeln!(note, "    fps: {f}");
    }
}

/// `--res`/`--fps`/`--device`/`--port` only ever apply to *this* invocation's foreground run —
/// but that run never happens when another instance already holds the lock, so those flags would
/// otherwise be silently swallowed with no indication anything was wrong. This persists a
/// `--device <name>` override into `config.yaml` instead (so it takes effect on the next
/// restart), or explains why a global override can't be applied this way, and returns a concise,
/// standalone message describing what happened — deliberately *not* the full "already running"
/// status dump (`build_already_running_report`), since passing an override flag means the user
/// already knows it's running and just wants confirmation the change took effect. Returns `None`
/// if no override flags were passed at all, so callers fall back to the full report. Never writes
/// anything under `--dry-run`.
fn pending_override_note(cli: &Cli, config: &mut Config) -> Option<String> {
    use std::fmt::Write as _;

    let device_override_requested = cli.res.is_some() || cli.fps.is_some();
    let has_any_override = cli.device.is_some()
        || device_override_requested
        || cli.port.is_some()
        || cli.hls_port.is_some()
        || cli.webrtc_port.is_some();
    if !has_any_override {
        return None;
    }

    let mut note = String::new();

    let Some(device_name) = cli.device.clone() else {
        let _ = writeln!(
            note,
            "rtsp-generator is already running, so the --res/--fps/--port/--hls-port/\
             --webrtc-port flags on this invocation have no effect. To change settings for a \
             running instance, edit {} directly (rtsp_port/hls_port/webrtc_port for those \
             ports, a devices: entry for per-camera --res/--fps) and run `rtsp-gen --restart`.",
            cli.config.display(),
        );
        return Some(note);
    };

    if !device_override_requested {
        let _ = writeln!(
            note,
            "rtsp-generator is already running. --device {device_name} was passed without \
             --res or --fps, so there's nothing to apply."
        );
        return Some(note);
    }

    // Already validated during CLI parsing (`Cli::parse_validated`), so this can't fail here.
    let parsed_res = cli.parsed_res().ok().flatten();
    let entry = config.devices.entry(device_name.clone()).or_default();
    if let Some(r) = parsed_res {
        entry.resolution = Some(format!("{}x{}", r.width, r.height));
    }
    if let Some(fps) = cli.fps {
        entry.fps = Some(fps);
    }
    let resolution = entry.resolution.clone();
    let fps = entry.fps;

    if cli.dry_run {
        let _ = writeln!(
            note,
            "[dry-run] rtsp-generator is already running; would save this override to {} \
             (not writing anything):",
            cli.config.display(),
        );
        write_device_override_lines(&mut note, &device_name, &resolution, fps);
        return Some(note);
    }

    match config.save(&cli.config) {
        Ok(()) => {
            let _ = writeln!(
                note,
                "rtsp-generator is already running, so this override was saved to {} instead of \
                 applying immediately:",
                cli.config.display(),
            );
            write_device_override_lines(&mut note, &device_name, &resolution, fps);
            note.push_str("Run `rtsp-gen --restart` to apply it.\n");
        }
        Err(e) => {
            let _ = writeln!(
                note,
                "tried to save this override to {} but failed: {e}",
                cli.config.display(),
            );
        }
    }

    Some(note)
}

/// The suggested default shown in the first-run prompt: `~/.rtsp-gen/streams.yaml`.
fn suggested_streams_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"));
    home.join(".rtsp-gen").join("streams.yaml")
}

fn expand_tilde(input: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/root"))
    };
    if let Some(rest) = input.strip_prefix("~/") {
        home().join(rest)
    } else if input == "~" {
        home()
    } else {
        PathBuf::from(input)
    }
}

/// If `path` doesn't already look like a `.yaml`/`.yml` file, treats it as a directory and
/// appends `streams.yaml`.
fn as_streams_file_path(path: PathBuf) -> PathBuf {
    let looks_like_file = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    );
    if looks_like_file {
        path
    } else {
        path.join("streams.yaml")
    }
}

/// Resolves where to write the reference `streams.yaml`, in order of precedence:
///   1. `--output` on the command line — always wins, never persisted.
///   2. A location already persisted in `config.yaml` (`streams_path`) from a prior run.
///   3. First run: if interactive (a human at a terminal), prompt with a default suggestion of
///      `~/.rtsp-gen/streams.yaml`, and remember the answer in `config.yaml`. If not interactive
///      (e.g. running under systemd with no controlling TTY), silently fall back to
///      `/etc/rtsp-generator/streams.yaml` (preserving the pre-existing systemd-service default)
///      and persist that instead.
///
/// Under `--dry-run`, resolves the same way but never prompts or writes `config.yaml`.
fn resolve_streams_path(cli: &Cli, config: &mut Config) -> PathBuf {
    if let Some(explicit) = &cli.output {
        return explicit.clone();
    }
    if let Some(sticky) = &config.streams_path {
        return sticky.clone();
    }

    if cli.dry_run {
        let fallback = PathBuf::from(DEFAULT_OUTPUT_PATH);
        info!(path = %fallback.display(), "[dry-run] would resolve a streams.yaml location on a real run (not prompting or persisting)");
        return fallback;
    }

    let chosen = if std::io::stdin().is_terminal() {
        let suggestion = suggested_streams_path();
        print!(
            "Where would you like to store the streams.yaml reference file? [~/.rtsp-gen]: "
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    suggestion
                } else {
                    as_streams_file_path(expand_tilde(trimmed))
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to read answer; using default");
                suggestion
            }
        }
    } else {
        let fallback = PathBuf::from(DEFAULT_OUTPUT_PATH);
        info!(
            path = %fallback.display(),
            "no streams.yaml location configured yet; using the default (run interactively to be prompted)"
        );
        fallback
    };

    config.streams_path = Some(chosen.clone());
    if let Err(e) = config.save(&cli.config) {
        warn!(error = %e, "failed to persist chosen streams.yaml location to config.yaml");
    }

    chosen
}

/// Applies `--res`/`--fps`/`--device` and `config.yaml` `devices:` overrides to each detected
/// camera, per the precedence rules in `Config::effective_override`.
pub fn apply_overrides(
    mut cameras: Vec<Camera>,
    cli: &Cli,
    config: &Config,
) -> anyhow::Result<Vec<Camera>> {
    for cam in &mut cameras {
        let (res, fps) = config.effective_override(cli, &cam.name)?;
        if let Some(r) = res {
            if !cam.resolutions.contains(&(r.width, r.height)) {
                warn!(
                    camera = %cam.name,
                    requested = format!("{}x{}", r.width, r.height),
                    "requested resolution is not in the device's advertised list; applying anyway"
                );
            }
            cam.chosen_resolution = (r.width, r.height);
        }
        if let Some(f) = fps {
            cam.fps = f;
        }
    }
    Ok(cameras)
}

/// Diffs `old` against `new` camera sets and reconciles MediaMTX paths via the runtime control
/// API. Returns `true` if any API call failed and a full config regenerate + restart is needed
/// as a fallback.
async fn reconcile_via_api(
    api: &MediaMtxApi,
    old: &[Camera],
    new: &[Camera],
    rtsp_port: u16,
    encoding: &EncodingConfig,
    hw: HwAccel,
) -> bool {
    let old_names: HashSet<&str> = old.iter().map(|c| c.name.as_str()).collect();
    let new_names: HashSet<&str> = new.iter().map(|c| c.name.as_str()).collect();
    let mut needs_full_restart = false;

    for cam in new.iter().filter(|c| !old_names.contains(c.name.as_str())) {
        info!(camera = %cam.name, "camera added; registering MediaMTX path");
        if let Err(e) = api.add_path(cam, rtsp_port, encoding, hw).await {
            warn!(camera = %cam.name, error = %e, "failed to add path via API; falling back to full regenerate+restart");
            needs_full_restart = true;
        }
    }

    for name in old_names.difference(&new_names) {
        info!(camera = %name, "camera removed; deleting MediaMTX path");
        if let Err(e) = api.delete_path(name).await {
            warn!(camera = %name, error = %e, "failed to delete path via API; falling back to full regenerate+restart");
            needs_full_restart = true;
        }
    }

    needs_full_restart
}

/// Checks whether every transcoded camera in `cameras` is actually producing data, per the
/// MediaMTX API. This is the real-world counterpart to `hwaccel::detect`'s synthetic probe: a
/// hardware encoder can pass a synthetic trial encode and still fail against a real capture
/// pipeline (wrong pixel format assumptions, resource limits only hit under real load, etc.),
/// exactly the kind of gap that caused the MJPEG-over-RTP bug this tool already had to work
/// around once. `-c copy` (non-transcoded) cameras aren't affected by the encoder choice, so
/// they're not checked. Returns `true` (assume healthy) if the check itself is inconclusive —
/// e.g. the API is briefly unreachable — rather than triggering a false-positive fallback.
async fn verify_paths_healthy(api: &MediaMtxApi, cameras: &[Camera]) -> bool {
    let transcoded: Vec<&Camera> = cameras
        .iter()
        .filter(|c| mediamtx::needs_transcode(&c.pixel_format))
        .collect();
    if transcoded.is_empty() {
        return true;
    }

    let Ok(value) = api.list_paths().await else {
        return true;
    };
    let Some(items) = value.get("items").and_then(|v| v.as_array()) else {
        return true;
    };

    transcoded.iter().all(|cam| {
        items.iter().any(|item| {
            item.get("name").and_then(|n| n.as_str()) == Some(cam.name.as_str())
                && item.get("ready").and_then(|r| r.as_bool()) == Some(true)
                && item
                    .get("bytesReceived")
                    .and_then(|b| b.as_u64())
                    .unwrap_or(0)
                    > 0
        })
    })
}

/// Waits for MediaMTX/ffmpeg to settle, then verifies `hw` is actually producing data for every
/// transcoded camera in `cameras`. If not, regenerates the MediaMTX config with software
/// encoding, forces a restart via `restart_tx`, and returns `HwAccel::Software`; otherwise
/// returns `hw` unchanged. A no-op (returns `hw` immediately) if `hw` is already `Software`.
async fn verify_and_maybe_downgrade(
    api: &MediaMtxApi,
    cameras: &[Camera],
    ports: mediamtx::Ports,
    encoding: &EncodingConfig,
    hw: HwAccel,
    mediamtx_config_path: &Path,
    restart_tx: &mpsc::Sender<()>,
) -> HwAccel {
    if hw == HwAccel::Software {
        return hw;
    }

    tokio::time::sleep(HW_HEALTH_CHECK_DELAY).await;

    if verify_paths_healthy(api, cameras).await {
        info!(backend = hw.label(), "hardware encoder verified working against live camera data");
        return hw;
    }

    warn!(
        backend = hw.label(),
        "hardware encoder produced no usable output against real camera data; falling back to software encoding"
    );
    let generated = mediamtx::generate_config(cameras, ports, encoding, HwAccel::Software);
    if let Err(e) = mediamtx::write_config(mediamtx_config_path, &generated) {
        warn!(error = %e, "failed to regenerate MediaMTX config for software fallback");
    }
    let _ = restart_tx.send(()).await;
    HwAccel::Software
}

/// Full run: detect, generate config, start MediaMTX, write reference YAML, then block watching
/// for hotplug events. This is the systemd `ExecStart` command.
pub async fn run(cli: &Cli, mut config: Config) -> anyhow::Result<RunOutcome> {
    // Held for the rest of this function; dropping it (on any return path) releases the flock.
    let _instance_lock = match acquire_single_instance_lock(Path::new(LOCK_PATH)) {
        Ok(LockOutcome::Acquired(file)) => Some(file),
        Ok(LockOutcome::AlreadyRunning { pid }) => {
            // An override flag means the user already knows it's running and just wants
            // confirmation their change took effect — skip the full status dump.
            let report = match pending_override_note(cli, &mut config) {
                Some(note) => note,
                None => build_already_running_report(cli, &config, pid).await,
            };
            return Ok(RunOutcome::AlreadyRunning(report));
        }
        Err(e) => {
            warn!(error = %e, path = LOCK_PATH, "failed to set up the single-instance lock file; continuing without it");
            None
        }
    };

    let cameras = device::detect_cameras()?;
    if cameras.is_empty() {
        return Ok(RunOutcome::NoCamerasFound);
    }
    let cameras = apply_overrides(cameras, cli, &config)?;

    let rtsp_port = config.effective_rtsp_port(cli);
    let hls_port = config.effective_hls_port(cli);
    let webrtc_port = config.effective_webrtc_port(cli);
    let binary = mediamtx::find_binary(config.mediamtx_binary.as_deref())?;
    let host_ip = netinfo::detect_lan_ip(config.advertise_ip, &config.exclude_interfaces)?;
    let output_path = resolve_streams_path(cli, &mut config);
    let ports = mediamtx::Ports {
        rtsp: rtsp_port,
        api: mediamtx::DEFAULT_API_PORT,
        hls: hls_port,
        webrtc: webrtc_port,
    };

    if cli.dry_run {
        info!(binary = %binary.display(), "[dry-run] would start MediaMTX");
        info!(
            preference = ?config.encoding.hardware,
            "[dry-run] would probe for a hardware H.264 encoder (VAAPI -> QSV -> V4L2M2M), \
             falling back to software; not probing now since that involves spawning ffmpeg"
        );
        for cam in &cameras {
            info!(
                camera = %cam.name,
                resolution = format!("{}x{}", cam.chosen_resolution.0, cam.chosen_resolution.1),
                fps = cam.fps,
                rtsp_url = format!("rtsp://{host_ip}:{rtsp_port}/{}", cam.name),
                hls_url = format!("http://{host_ip}:{hls_port}/{}", cam.name),
                webrtc_url = format!("http://{host_ip}:{webrtc_port}/{}", cam.name),
                "[dry-run] would publish"
            );
        }
        info!(path = %output_path.display(), "[dry-run] would write reference YAML");
        return Ok(RunOutcome::Success);
    }

    let hw = hwaccel::detect(config.encoding.hardware).await;

    let mediamtx_config_path = PathBuf::from(mediamtx::CONFIG_PATH);
    let generated = mediamtx::generate_config(&cameras, ports, &config.encoding, hw);
    mediamtx::write_config(&mediamtx_config_path, &generated)?;

    let streams = output::build(&cameras, host_ip, rtsp_port, hls_port, webrtc_port);
    output::write_atomic(&output_path, &streams)?;
    info!(path = %output_path.display(), cameras = cameras.len(), "wrote reference YAML");

    let (stop_tx, stop_rx) = watch::channel(false);
    let (restart_tx, restart_rx) = mpsc::channel::<()>(4);
    let supervisor = tokio::spawn(mediamtx::supervise(
        binary.clone(),
        mediamtx_config_path.clone(),
        stop_rx,
        restart_rx,
    ));

    let (hotplug_tx, mut hotplug_rx) = mpsc::channel(4);
    let hotplug_task = tokio::spawn(hotplug::watch(hotplug_tx));

    let api = MediaMtxApi::new(mediamtx::DEFAULT_API_PORT);

    // Real-world safety net: a hardware encoder can pass `hwaccel::detect`'s synthetic probe and
    // still fail against the actual camera pipeline. `hw` may be downgraded to `Software` here.
    let mut hw = verify_and_maybe_downgrade(
        &api,
        &cameras,
        ports,
        &config.encoding,
        hw,
        &mediamtx_config_path,
        &restart_tx,
    )
    .await;

    let mut known_cameras = cameras;

    let mut sigterm = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
            maybe_event = hotplug_rx.recv() => {
                if maybe_event.is_none() {
                    warn!("hotplug watcher terminated unexpectedly");
                    break;
                }
                match device::detect_cameras() {
                    Ok(new_cameras) => {
                        let new_cameras = match apply_overrides(new_cameras, cli, &config) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!(error = %e, "failed to apply overrides after hotplug event");
                                continue;
                            }
                        };

                        let old_names: HashSet<&str> =
                            known_cameras.iter().map(|c| c.name.as_str()).collect();
                        let added_needs_transcode = new_cameras.iter().any(|c| {
                            !old_names.contains(c.name.as_str())
                                && mediamtx::needs_transcode(&c.pixel_format)
                        });

                        let needs_full_restart = reconcile_via_api(
                            &api,
                            &known_cameras,
                            &new_cameras,
                            rtsp_port,
                            &config.encoding,
                            hw,
                        )
                        .await;

                        if needs_full_restart {
                            let generated = mediamtx::generate_config(
                                &new_cameras,
                                ports,
                                &config.encoding,
                                hw,
                            );
                            if let Err(e) = mediamtx::write_config(&mediamtx_config_path, &generated) {
                                warn!(error = %e, "failed to regenerate MediaMTX config");
                            }
                            let _ = restart_tx.send(()).await;
                        } else if added_needs_transcode && hw != HwAccel::Software {
                            // A newly hotplugged camera needs the hardware encoder too — verify
                            // it actually works against this real device before trusting it.
                            hw = verify_and_maybe_downgrade(
                                &api,
                                &new_cameras,
                                ports,
                                &config.encoding,
                                hw,
                                &mediamtx_config_path,
                                &restart_tx,
                            )
                            .await;
                        }

                        known_cameras = new_cameras;
                        let streams = output::build(&known_cameras, host_ip, rtsp_port, hls_port, webrtc_port);
                        if let Err(e) = output::write_atomic(&output_path, &streams) {
                            warn!(error = %e, "failed to update reference YAML after hotplug event");
                        } else {
                            info!(cameras = known_cameras.len(), "updated reference YAML after hotplug event");
                        }
                    }
                    Err(e) => warn!(error = %e, "device re-detection after hotplug event failed"),
                }
            }
        }
    }

    let _ = stop_tx.send(true);
    hotplug_task.abort();
    let _ = supervisor.await;

    Ok(RunOutcome::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeviceOverride;

    #[test]
    fn second_lock_attempt_sees_already_running_with_correct_pid() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        let first = acquire_single_instance_lock(&lock_path).unwrap();
        let held_file = match first {
            LockOutcome::Acquired(f) => f,
            LockOutcome::AlreadyRunning { .. } => panic!("expected to acquire the lock first"),
        };

        match acquire_single_instance_lock(&lock_path).unwrap() {
            LockOutcome::AlreadyRunning { pid } => {
                assert_eq!(pid, Some(std::process::id()));
            }
            LockOutcome::Acquired(_) => panic!("expected the lock to be contended"),
        }

        drop(held_file);

        // Dropping the first handle releases the flock, so re-acquisition should now succeed.
        assert!(matches!(
            acquire_single_instance_lock(&lock_path).unwrap(),
            LockOutcome::Acquired(_)
        ));
    }

    #[test]
    fn running_pid_ignores_stale_lock_file_contents() {
        // Regression test: a lock file's recorded pid persists after a clean process exit
        // (nothing deletes the file), but the flock itself is released. `running_pid` must check
        // the actual lock, not just parse whatever pid string happens to be sitting in the file.
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        // Simulate a stale file: acquire and immediately drop, leaving our pid written but the
        // lock free.
        match acquire_single_instance_lock(&lock_path).unwrap() {
            LockOutcome::Acquired(file) => drop(file),
            LockOutcome::AlreadyRunning { .. } => panic!("lock should have been free"),
        }
        assert_eq!(running_pid_at(&lock_path), None);

        // Now hold it for real: running_pid should report the holder's pid.
        let held = match acquire_single_instance_lock(&lock_path).unwrap() {
            LockOutcome::Acquired(file) => file,
            LockOutcome::AlreadyRunning { .. } => panic!("expected to acquire the lock"),
        };
        assert_eq!(running_pid_at(&lock_path), Some(std::process::id()));
        drop(held);
        assert_eq!(running_pid_at(&lock_path), None);
    }

    fn base_cli(config_path: PathBuf) -> Cli {
        Cli {
            list: false,
            status: false,
            info: false,
            restart: false,
            stop: false,
            install_service: false,
            uninstall_service: false,
            about: false,
            config: config_path,
            output: None,
            res: None,
            device: None,
            fps: None,
            port: None,
            hls_port: None,
            webrtc_port: None,
            json: false,
            all: false,
            dry_run: false,
            verbose: 0,
        }
    }

    #[test]
    fn no_override_flags_means_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let cli = base_cli(dir.path().join("config.yaml"));
        let mut config = Config::default();
        assert_eq!(pending_override_note(&cli, &mut config), None);
    }

    #[test]
    fn global_override_without_device_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path.clone());
        cli.fps = Some(10);
        let mut config = Config::default();

        let note = pending_override_note(&cli, &mut config).unwrap();

        assert!(note.contains("--res/--fps/--port"));
        assert!(note.contains("rtsp-gen --restart"));
        assert!(config.devices.is_empty());
        assert!(!config_path.exists());
    }

    #[test]
    fn device_flag_without_res_or_fps_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path.clone());
        cli.device = Some("cam1".to_string());
        let mut config = Config::default();

        let note = pending_override_note(&cli, &mut config).unwrap();

        assert!(note.contains("nothing to apply"));
        assert!(config.devices.is_empty());
        assert!(!config_path.exists());
    }

    #[test]
    fn device_scoped_fps_override_is_persisted_and_restart_is_suggested() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path.clone());
        cli.device = Some("hd-pro-webcam-c920-antelope".to_string());
        cli.fps = Some(10);
        let mut config = Config::default();

        let note = pending_override_note(&cli, &mut config).unwrap();

        assert_eq!(
            config.devices["hd-pro-webcam-c920-antelope"].fps,
            Some(10)
        );
        assert!(note.contains("hd-pro-webcam-c920-antelope"));
        assert!(note.contains("fps: 10"));
        assert!(note.contains("rtsp-gen --restart"));

        // Actually persisted to disk, not just held in memory.
        let reloaded = Config::load(&config_path).unwrap();
        assert_eq!(
            reloaded.devices["hd-pro-webcam-c920-antelope"].fps,
            Some(10)
        );
    }

    #[test]
    fn device_scoped_resolution_and_fps_both_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path);
        cli.device = Some("cam1".to_string());
        cli.res = Some("640x480".to_string());
        cli.fps = Some(15);
        let mut config = Config::default();

        pending_override_note(&cli, &mut config).unwrap();

        let entry = &config.devices["cam1"];
        assert_eq!(entry.resolution.as_deref(), Some("640x480"));
        assert_eq!(entry.fps, Some(15));
    }

    #[test]
    fn dry_run_previews_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path.clone());
        cli.device = Some("cam1".to_string());
        cli.fps = Some(10);
        cli.dry_run = true;
        let mut config = Config::default();

        let note = pending_override_note(&cli, &mut config).unwrap();

        assert!(note.contains("[dry-run]"));
        assert!(note.contains("fps: 10"));
        assert!(!config_path.exists());
    }

    #[test]
    fn preserves_existing_config_entries_for_other_devices() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path);
        cli.device = Some("cam2".to_string());
        cli.fps = Some(20);
        let mut config = Config::default();
        config.devices.insert(
            "cam1".to_string(),
            DeviceOverride {
                resolution: Some("1920x1080".to_string()),
                fps: Some(30),
            },
        );

        pending_override_note(&cli, &mut config).unwrap();

        assert_eq!(config.devices["cam1"].fps, Some(30));
        assert_eq!(config.devices["cam2"].fps, Some(20));
    }

    /// Runs `body` with `HOME` temporarily set, restoring the previous value afterwards.
    /// Serialized via a process-wide mutex since `std::env::set_var` affects the whole process
    /// and tests run concurrently by default.
    fn with_home<R>(home: &str, body: impl FnOnce() -> R) -> R {
        use std::sync::Mutex;
        static HOME_LOCK: Mutex<()> = Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap();

        let original = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let result = body();
        match original {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result
    }

    #[test]
    fn expand_tilde_handles_prefix_and_bare_tilde() {
        with_home("/home/testuser", || {
            assert_eq!(
                expand_tilde("~/.rtsp-gen"),
                PathBuf::from("/home/testuser/.rtsp-gen")
            );
            assert_eq!(expand_tilde("~"), PathBuf::from("/home/testuser"));
            assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        });
    }

    #[test]
    fn as_streams_file_path_appends_filename_to_directories() {
        assert_eq!(
            as_streams_file_path(PathBuf::from("/home/testuser/.rtsp-gen")),
            PathBuf::from("/home/testuser/.rtsp-gen/streams.yaml")
        );
        assert_eq!(
            as_streams_file_path(PathBuf::from("/some/custom.yaml")),
            PathBuf::from("/some/custom.yaml")
        );
        assert_eq!(
            as_streams_file_path(PathBuf::from("/some/custom.yml")),
            PathBuf::from("/some/custom.yml")
        );
    }

    #[test]
    fn explicit_cli_output_wins_and_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path.clone());
        cli.output = Some(PathBuf::from("/explicit/streams.yaml"));
        let mut config = Config::default();

        let resolved = resolve_streams_path(&cli, &mut config);

        assert_eq!(resolved, PathBuf::from("/explicit/streams.yaml"));
        assert_eq!(config.streams_path, None);
        assert!(!config_path.exists());
    }

    #[test]
    fn persisted_config_path_wins_without_reprompting() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let cli = base_cli(config_path);
        let mut config = Config {
            streams_path: Some(PathBuf::from("/already/chosen/streams.yaml")),
            ..Config::default()
        };

        let resolved = resolve_streams_path(&cli, &mut config);

        assert_eq!(resolved, PathBuf::from("/already/chosen/streams.yaml"));
    }

    #[test]
    fn first_run_non_interactive_falls_back_to_system_default_and_persists() {
        // `cargo test` runs with stdin that isn't a terminal, so this exercises the
        // non-interactive branch without needing to fake a TTY.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let cli = base_cli(config_path.clone());
        let mut config = Config::default();

        let resolved = resolve_streams_path(&cli, &mut config);

        assert_eq!(resolved, PathBuf::from(DEFAULT_OUTPUT_PATH));
        assert_eq!(config.streams_path, Some(PathBuf::from(DEFAULT_OUTPUT_PATH)));
        // The choice was persisted so subsequent runs don't repeat this resolution.
        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.streams_path, Some(PathBuf::from(DEFAULT_OUTPUT_PATH)));
    }

    #[test]
    fn dry_run_does_not_persist_anything() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut cli = base_cli(config_path.clone());
        cli.dry_run = true;
        let mut config = Config::default();

        let resolved = resolve_streams_path(&cli, &mut config);

        assert_eq!(resolved, PathBuf::from(DEFAULT_OUTPUT_PATH));
        assert_eq!(config.streams_path, None);
        assert!(!config_path.exists());
    }
}
