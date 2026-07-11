use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::config::EncodingConfig;
use crate::device::Camera;
use crate::hwaccel::{HwAccel, VAAPI_DEVICE};

pub const DEFAULT_API_PORT: u16 = 9997;
/// Working config path, separate from any user-authored MediaMTX config so regeneration never
/// clobbers a hand-edited file.
pub const CONFIG_PATH: &str = "/var/lib/rtsp-generator/mediamtx.yml";
const BACKOFF_CAP_SECS: u64 = 30;
/// A run has to stay up this long before we consider the crash loop "recovered" and reset backoff.
const STABLE_RUN_THRESHOLD: Duration = Duration::from_secs(60);
/// How often to force a keyframe, for any transcoded (non-`-c copy`) camera. Without this,
/// libx264 defaults to a 250-*frame* GOP — at low fps that's tens of seconds between keyframes,
/// and a new RTSP viewer can't render anything until the next one arrives, however good their
/// buffer settings are. Forcing a short, fps-relative interval keeps join latency low regardless
/// of the configured framerate, at the cost of a modest bitrate increase (I-frames are larger).
const KEYFRAME_INTERVAL_SECS: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum MediaMtxError {
    #[error(
        "MediaMTX binary not found at '{0}'. Install it or set `mediamtx_binary` in config.yaml"
    )]
    ConfiguredBinaryNotFound(PathBuf),
    #[error(
        "MediaMTX binary not found on $PATH. Install it (see README) or set `mediamtx_binary` \
         in config.yaml"
    )]
    BinaryNotFoundOnPath,
    #[error("failed to write generated MediaMTX config to {path}: {source}")]
    WriteConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to spawn MediaMTX process ({binary}): {source}")]
    Spawn {
        binary: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("MediaMTX API request failed: {0}")]
    Api(#[source] reqwest::Error),
    #[error("MediaMTX API returned status {status} for {url}: {body}")]
    ApiStatus {
        status: reqwest::StatusCode,
        url: String,
        body: String,
    },
}

// ---------------------------------------------------------------------------
// Config generation (pure, unit-testable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathConfig {
    #[serde(rename = "runOnInit")]
    pub run_on_init: String,
    #[serde(rename = "runOnInitRestart")]
    pub run_on_init_restart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeneratedConfig {
    #[serde(rename = "rtspAddress")]
    pub rtsp_address: String,
    pub api: bool,
    #[serde(rename = "apiAddress")]
    pub api_address: String,
    pub paths: BTreeMap<String, PathConfig>,
}

/// Maps a V4L2 fourcc pixel format to the value ffmpeg's `-input_format` expects.
fn ffmpeg_input_format(pixel_format: &str) -> String {
    match pixel_format {
        "MJPG" => "mjpeg".to_string(),
        "YUYV" => "yuyv422".to_string(),
        "H264" => "h264".to_string(),
        "NV12" => "nv12".to_string(),
        other => other.to_lowercase(),
    }
}

/// Only true RTP-native formats can be pushed with `-c copy`. Notably, MJPEG can't: ffmpeg's
/// RTP-JPEG payloader (RFC 2435) is fragile in practice — it rejects the >2-quantization-table
/// and non-4:4:4-chroma JPEG streams that most UVC webcams actually produce, even when ffmpeg
/// itself re-encodes to force those constraints. Anything that isn't already H.264 is transcoded.
pub fn needs_transcode(pixel_format: &str) -> bool {
    pixel_format != "H264"
}

/// Best-effort description of which encoder a generated `runOnInit` command uses, for
/// diagnostics (`--info`). Matches on the flags `ffmpeg_command` actually emits, so it stays in
/// sync with that function by construction rather than by convention.
pub fn describe_encoder(run_on_init: &str) -> &'static str {
    if run_on_init.contains("h264_vaapi") {
        HwAccel::Vaapi.label()
    } else if run_on_init.contains("h264_qsv") {
        HwAccel::Qsv.label()
    } else if run_on_init.contains("h264_v4l2m2m") {
        HwAccel::V4l2m2m.label()
    } else if run_on_init.contains("libx264") {
        HwAccel::Software.label()
    } else if run_on_init.contains("-c copy") {
        "passthrough (-c copy)"
    } else {
        "unknown"
    }
}

/// Appends a bitrate cap, if configured, in the `-b:v/-maxrate/-bufsize` form every ffmpeg H.264
/// encoder (hardware or software) accepts.
fn bitrate_args(bitrate_kbps: Option<u32>) -> String {
    match bitrate_kbps {
        Some(kbps) => format!(" -b:v {kbps}k -maxrate {kbps}k -bufsize {}k", kbps * 2),
        None => String::new(),
    }
}

/// Global ffmpeg args that must precede `-i` (hardware device setup). Only VAAPI needs this.
fn pre_input_args(hw: HwAccel) -> String {
    match hw {
        HwAccel::Vaapi => format!("-vaapi_device {VAAPI_DEVICE} "),
        HwAccel::Qsv | HwAccel::V4l2m2m | HwAccel::Software => String::new(),
    }
}

/// Builds the codec/filter arguments (after `-i`) for a transcoded camera, on the given
/// hardware/software backend. Forces a keyframe at least every `KEYFRAME_INTERVAL_SECS` (see
/// that constant's doc comment for why) — `-g` is a generic AVCodecContext option every ffmpeg
/// video encoder honors, hardware or software. `-keyint_min`/`-sc_threshold` are libx264-private
/// options (not necessarily recognized by hardware encoders), so those are software-only.
fn codec_args(encoding: &EncodingConfig, hw: HwAccel, fps: u32) -> String {
    let gop = (fps * KEYFRAME_INTERVAL_SECS).max(1);
    let base = match hw {
        HwAccel::Vaapi => format!("-vf format=nv12,hwupload -c:v h264_vaapi -g {gop}"),
        HwAccel::Qsv => format!("-c:v h264_qsv -g {gop}"),
        HwAccel::V4l2m2m => format!("-c:v h264_v4l2m2m -g {gop}"),
        HwAccel::Software => format!(
            "-c:v libx264 -preset {preset} -tune zerolatency -g {gop} -keyint_min {gop} \
             -sc_threshold 0",
            preset = encoding.preset
        ),
    };
    format!("{base}{}", bitrate_args(encoding.bitrate_kbps))
}

/// Builds the `runOnInit` ffmpeg command for a single camera, pushing into an RTSP publish at
/// `rtsp://localhost:<port>/<name>`. Cameras that already capture H.264 are passed through with
/// `-c copy`; everything else (MJPEG, raw YUYV/NV12, ...) is transcoded to H.264 on `hw` (or
/// software `libx264` if `hw` is `HwAccel::Software` or no hardware encoder verified as working —
/// see `hwaccel::detect`), since H.264 is the only codec ffmpeg's RTP layer handles reliably for
/// real-world capture formats.
pub fn ffmpeg_command(camera: &Camera, rtsp_port: u16, encoding: &EncodingConfig, hw: HwAccel) -> String {
    let transcoding = needs_transcode(&camera.pixel_format);
    let pre_input = if transcoding { pre_input_args(hw) } else { String::new() };
    let codec = if transcoding {
        codec_args(encoding, hw, camera.fps)
    } else {
        "-c copy".to_string()
    };
    format!(
        "ffmpeg {pre_input}-f v4l2 -input_format {input_format} -video_size {width}x{height} \
         -framerate {fps} -i {path} {codec} -f rtsp rtsp://localhost:{rtsp_port}/{name}",
        input_format = ffmpeg_input_format(&camera.pixel_format),
        width = camera.chosen_resolution.0,
        height = camera.chosen_resolution.1,
        fps = camera.fps,
        path = camera.capture_path().display(),
        rtsp_port = rtsp_port,
        name = camera.name,
    )
}

fn path_config(camera: &Camera, rtsp_port: u16, encoding: &EncodingConfig, hw: HwAccel) -> PathConfig {
    PathConfig {
        run_on_init: ffmpeg_command(camera, rtsp_port, encoding, hw),
        run_on_init_restart: true,
    }
}

/// Generates the full MediaMTX config for the given camera set. Pure function: given a
/// `Vec<Camera>`, produces a struct that serializes deterministically to YAML.
pub fn generate_config(
    cameras: &[Camera],
    rtsp_port: u16,
    api_port: u16,
    encoding: &EncodingConfig,
    hw: HwAccel,
) -> GeneratedConfig {
    let paths = cameras
        .iter()
        .map(|cam| (cam.name.clone(), path_config(cam, rtsp_port, encoding, hw)))
        .collect();

    GeneratedConfig {
        rtsp_address: format!(":{rtsp_port}"),
        api: true,
        api_address: format!(":{api_port}"),
        paths,
    }
}

pub fn to_yaml(config: &GeneratedConfig) -> String {
    serde_yaml::to_string(config).expect("GeneratedConfig always serializes")
}

/// Writes the generated config to its working path (e.g. `/var/lib/rtsp-generator/mediamtx.yml`),
/// separate from any user-authored MediaMTX config so regeneration never clobbers a hand-edited file.
pub fn write_config(path: &Path, config: &GeneratedConfig) -> Result<(), MediaMtxError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| MediaMtxError::WriteConfig {
            path: path.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, to_yaml(config)).map_err(|source| MediaMtxError::WriteConfig {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------
// Binary resolution
// ---------------------------------------------------------------------------

/// Resolves the MediaMTX binary path: the configured path if set, else a `$PATH` lookup.
pub fn find_binary(configured: Option<&Path>) -> Result<PathBuf, MediaMtxError> {
    if let Some(p) = configured {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(MediaMtxError::ConfiguredBinaryNotFound(p.to_path_buf()))
        };
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("mediamtx");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(MediaMtxError::BinaryNotFoundOnPath)
}

// ---------------------------------------------------------------------------
// Process supervision
// ---------------------------------------------------------------------------

/// Capped exponential backoff: 1s, 2s, 4s, ... up to a 30s cap.
pub fn backoff_delay(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    Duration::from_secs(secs.min(BACKOFF_CAP_SECS))
}

async fn spawn_mediamtx(binary: &Path, config_path: &Path) -> Result<Child, MediaMtxError> {
    Command::new(binary)
        .arg(config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| MediaMtxError::Spawn {
            binary: binary.to_path_buf(),
            source,
        })
}

const TAIL_LINES: usize = 20;

/// Spawns a task that collects the last `TAIL_LINES` lines from `reader`. MediaMTX logs
/// everything — including fatal startup errors — to stdout by default, not stderr, so callers
/// must capture both streams to get a useful crash diagnostic.
fn spawn_tail_reader<R>(reader: R) -> tokio::task::JoinHandle<Vec<String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        let mut tail: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if tail.len() >= TAIL_LINES {
                tail.remove(0);
            }
            tail.push(line);
        }
        tail
    })
}

/// Runs MediaMTX as a supervised child process, restarting it with capped exponential backoff
/// if it exits unexpectedly. Returns when `stop_rx` reports a shutdown request.
pub async fn supervise(
    binary: PathBuf,
    config_path: PathBuf,
    mut stop_rx: watch::Receiver<bool>,
    mut restart_rx: mpsc::Receiver<()>,
) -> Result<(), MediaMtxError> {
    let mut attempt: u32 = 0;

    loop {
        if *stop_rx.borrow() {
            return Ok(());
        }

        info!(binary = %binary.display(), config = %config_path.display(), "starting MediaMTX");
        let started_at = std::time::Instant::now();
        let mut child = spawn_mediamtx(&binary, &config_path).await?;

        let stdout_task = child.stdout.take().map(spawn_tail_reader);
        let stderr_task = child.stderr.take().map(spawn_tail_reader);

        tokio::select! {
            status = child.wait() => {
                let mut tail = Vec::new();
                if let Some(task) = stdout_task {
                    tail.extend(task.await.unwrap_or_default());
                }
                if let Some(task) = stderr_task {
                    tail.extend(task.await.unwrap_or_default());
                }

                if *stop_rx.borrow() {
                    return Ok(());
                }

                match status {
                    Ok(status) if status.success() => {
                        warn!("MediaMTX exited cleanly but unexpectedly; restarting");
                    }
                    Ok(status) => {
                        error!(code = ?status.code(), output_tail = %tail.join("\n"), "MediaMTX exited with an error");
                    }
                    Err(e) => {
                        error!(error = %e, "failed to wait on MediaMTX process");
                    }
                }

                if started_at.elapsed() >= STABLE_RUN_THRESHOLD {
                    attempt = 0;
                } else {
                    attempt = attempt.saturating_add(1);
                }

                let delay = backoff_delay(attempt);
                warn!(delay_secs = delay.as_secs(), "restarting MediaMTX after backoff");

                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            return Ok(());
                        }
                    }
                }
            }
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    let _ = child.kill().await;
                    return Ok(());
                }
            }
            _ = restart_rx.recv() => {
                info!("forced restart requested (config regenerated after hotplug); reloading MediaMTX");
                let _ = child.kill().await;
                if let Some(task) = stdout_task {
                    task.abort();
                }
                if let Some(task) = stderr_task {
                    task.abort();
                }
                // Forced restarts are a deliberate reload, not a crash: don't penalize backoff.
                attempt = 0;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime control API
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AddPathBody {
    #[serde(rename = "runOnInit")]
    run_on_init: String,
    #[serde(rename = "runOnInitRestart")]
    run_on_init_restart: bool,
}

pub struct MediaMtxApi {
    base_url: String,
    client: reqwest::Client,
}

impl MediaMtxApi {
    pub fn new(api_port: u16) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{api_port}"),
            client: reqwest::Client::new(),
        }
    }

    /// `POST /v3/config/paths/add/<name>` — adds a single path without restarting MediaMTX.
    pub async fn add_path(
        &self,
        camera: &Camera,
        rtsp_port: u16,
        encoding: &EncodingConfig,
        hw: HwAccel,
    ) -> Result<(), MediaMtxError> {
        let url = format!("{}/v3/config/paths/add/{}", self.base_url, camera.name);
        let body = AddPathBody {
            run_on_init: ffmpeg_command(camera, rtsp_port, encoding, hw),
            run_on_init_restart: true,
        };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(MediaMtxError::Api)?;
        Self::check_status(resp, url).await
    }

    /// `DELETE /v3/config/paths/delete/<name>` — removes a single path without restarting MediaMTX.
    pub async fn delete_path(&self, name: &str) -> Result<(), MediaMtxError> {
        let url = format!("{}/v3/config/paths/delete/{}", self.base_url, name);
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(MediaMtxError::Api)?;
        Self::check_status(resp, url).await
    }

    /// `GET /v3/paths/list` — used by `--status --json`.
    pub async fn list_paths(&self) -> Result<serde_json::Value, MediaMtxError> {
        let url = format!("{}/v3/paths/list", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(MediaMtxError::Api)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MediaMtxError::ApiStatus { status, url, body });
        }
        resp.json().await.map_err(MediaMtxError::Api)
    }

    async fn check_status(resp: reqwest::Response, url: String) -> Result<(), MediaMtxError> {
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(MediaMtxError::ApiStatus { status, url, body })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn camera(name: &str) -> Camera {
        Camera {
            name: name.to_string(),
            device_path: PathBuf::from("/dev/video0"),
            stable_path: Some(PathBuf::from(format!(
                "/dev/v4l/by-id/usb-{name}-video-index0"
            ))),
            resolutions: vec![(1920, 1080)],
            chosen_resolution: (1920, 1080),
            fps: 30,
            pixel_format: "MJPG".to_string(),
        }
    }

    #[test]
    fn generates_expected_path_entry() {
        let cams = vec![camera("logitech-c920")];
        let config = generate_config(&cams, 8554, 9997, &EncodingConfig::default(), HwAccel::Software);
        assert_eq!(config.rtsp_address, ":8554");
        assert_eq!(config.api_address, ":9997");
        assert!(config.api);

        let path = config.paths.get("logitech-c920").unwrap();
        assert!(path.run_on_init_restart);
        assert!(path.run_on_init.contains("-input_format mjpeg"));
        assert!(path.run_on_init.contains("-video_size 1920x1080"));
        assert!(path.run_on_init.contains("-framerate 30"));
        assert!(path
            .run_on_init
            .contains("rtsp://localhost:8554/logitech-c920"));
        assert!(path
            .run_on_init
            .contains("/dev/v4l/by-id/usb-logitech-c920-video-index0"));
    }

    #[test]
    fn mjpeg_cameras_are_transcoded_to_h264() {
        // ffmpeg's RTP-JPEG payloader (RFC 2435) rejects the quantization-table counts and
        // chroma subsampling most real UVC MJPEG streams actually use, even after re-encoding
        // to force compliance — so MJPEG must go through libx264, not `-c copy`.
        let cam = camera("logitech-c920");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(cmd.contains("-c:v libx264"));
        assert!(!cmd.contains("-c copy"));
    }

    #[test]
    fn h264_native_cameras_use_passthrough() {
        let mut cam = camera("native-h264-cam");
        cam.pixel_format = "H264".to_string();
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(cmd.contains("-c copy"));
        assert!(!cmd.contains("libx264"));
    }

    #[test]
    fn raw_pixel_formats_are_also_transcoded() {
        let mut cam = camera("raw-yuyv-cam");
        cam.pixel_format = "YUYV".to_string();
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(cmd.contains("-c:v libx264"));
    }

    #[test]
    fn default_preset_is_ultrafast() {
        let cam = camera("cam1");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(cmd.contains("-preset ultrafast"));
    }

    #[test]
    fn custom_preset_is_honored() {
        let cam = camera("cam1");
        let encoding = EncodingConfig {
            preset: "veryfast".to_string(),
            ..EncodingConfig::default()
        };
        let cmd = ffmpeg_command(&cam, 8554, &encoding, HwAccel::Software);
        assert!(cmd.contains("-preset veryfast"));
    }

    #[test]
    fn no_bitrate_flags_when_unset() {
        let cam = camera("cam1");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(!cmd.contains("-b:v"));
        assert!(!cmd.contains("-maxrate"));
        assert!(!cmd.contains("-bufsize"));
    }

    #[test]
    fn keyframe_interval_is_fps_relative_not_a_fixed_frame_count() {
        // Regression test for a real bug: without an explicit -g, libx264 defaults to a
        // 250-*frame* GOP, so lowering fps makes the wall-clock wait between keyframes (and
        // therefore RTSP join latency) worse, not better. -g must scale with fps.
        let mut cam = camera("cam1");
        cam.fps = 30;
        let cmd_30fps = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(cmd_30fps.contains(&format!("-g {}", 30 * KEYFRAME_INTERVAL_SECS)));

        cam.fps = 10;
        let cmd_10fps = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(cmd_10fps.contains(&format!("-g {}", 10 * KEYFRAME_INTERVAL_SECS)));
    }

    #[test]
    fn software_keyframe_args_are_fully_pinned() {
        let cam = camera("cam1");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        let gop = 30 * KEYFRAME_INTERVAL_SECS;
        assert!(cmd.contains(&format!("-g {gop}")));
        assert!(cmd.contains(&format!("-keyint_min {gop}")));
        assert!(cmd.contains("-sc_threshold 0"));
    }

    #[test]
    fn hardware_backends_get_g_but_not_libx264_private_options() {
        // -keyint_min/-sc_threshold are libx264-private AVOptions; hardware encoders may not
        // recognize them, so only the universal -g should be sent to those backends.
        let cam = camera("cam1");
        for hw in [HwAccel::Vaapi, HwAccel::Qsv, HwAccel::V4l2m2m] {
            let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), hw);
            assert!(cmd.contains(&format!("-g {}", 30 * KEYFRAME_INTERVAL_SECS)), "{hw:?}");
            assert!(!cmd.contains("-keyint_min"), "{hw:?}");
            assert!(!cmd.contains("-sc_threshold"), "{hw:?}");
        }
    }

    #[test]
    fn passthrough_cameras_get_no_keyframe_args() {
        // -c copy cameras aren't transcoded, so there's no encoder to configure a GOP on.
        let mut cam = camera("native-h264-cam");
        cam.pixel_format = "H264".to_string();
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Software);
        assert!(!cmd.contains("-g "));
        assert!(!cmd.contains("-keyint_min"));
    }

    #[test]
    fn bitrate_cap_produces_maxrate_and_bufsize() {
        let cam = camera("cam1");
        let encoding = EncodingConfig {
            bitrate_kbps: Some(1500),
            ..EncodingConfig::default()
        };
        let cmd = ffmpeg_command(&cam, 8554, &encoding, HwAccel::Software);
        assert!(cmd.contains("-b:v 1500k"));
        assert!(cmd.contains("-maxrate 1500k"));
        assert!(cmd.contains("-bufsize 3000k"));
    }

    #[test]
    fn h264_passthrough_ignores_encoding_config() {
        // -c copy cameras shouldn't get libx264 flags even if a custom preset/bitrate is set.
        let mut cam = camera("native-h264-cam");
        cam.pixel_format = "H264".to_string();
        let encoding = EncodingConfig {
            preset: "veryslow".to_string(),
            bitrate_kbps: Some(4000),
            ..EncodingConfig::default()
        };
        let cmd = ffmpeg_command(&cam, 8554, &encoding, HwAccel::Software);
        assert!(cmd.contains("-c copy"));
        assert!(!cmd.contains("veryslow"));
        assert!(!cmd.contains("4000"));
    }

    #[test]
    fn vaapi_backend_includes_device_and_filter() {
        let cam = camera("cam1");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Vaapi);
        assert!(cmd.contains(&format!("-vaapi_device {VAAPI_DEVICE}")));
        assert!(cmd.contains("-vf format=nv12,hwupload"));
        assert!(cmd.contains("-c:v h264_vaapi"));
        assert!(!cmd.contains("libx264"));
        // The device arg must precede -i.
        let device_pos = cmd.find("-vaapi_device").unwrap();
        let input_pos = cmd.find(" -i ").unwrap();
        assert!(device_pos < input_pos);
    }

    #[test]
    fn qsv_backend_uses_h264_qsv_with_no_vaapi_device() {
        let cam = camera("cam1");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Qsv);
        assert!(cmd.contains("-c:v h264_qsv"));
        assert!(!cmd.contains("-vaapi_device"));
        assert!(!cmd.contains("libx264"));
    }

    #[test]
    fn v4l2m2m_backend_uses_h264_v4l2m2m() {
        let cam = camera("cam1");
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::V4l2m2m);
        assert!(cmd.contains("-c:v h264_v4l2m2m"));
        assert!(!cmd.contains("-vaapi_device"));
        assert!(!cmd.contains("libx264"));
    }

    #[test]
    fn hardware_backends_still_respect_bitrate_cap() {
        let cam = camera("cam1");
        let encoding = EncodingConfig {
            bitrate_kbps: Some(2000),
            ..EncodingConfig::default()
        };
        let cmd = ffmpeg_command(&cam, 8554, &encoding, HwAccel::Qsv);
        assert!(cmd.contains("-b:v 2000k"));
        assert!(cmd.contains("-maxrate 2000k"));
    }

    #[test]
    fn h264_passthrough_skips_hardware_setup_entirely() {
        // A native-H.264 camera should stay pure `-c copy`, even on a hardware backend —
        // there's nothing to transcode, so no device/filter setup should appear.
        let mut cam = camera("native-h264-cam");
        cam.pixel_format = "H264".to_string();
        let cmd = ffmpeg_command(&cam, 8554, &EncodingConfig::default(), HwAccel::Vaapi);
        assert!(cmd.contains("-c copy"));
        assert!(!cmd.contains("-vaapi_device"));
        assert!(!cmd.contains("hwupload"));
    }

    #[test]
    fn describe_encoder_matches_ffmpeg_command_output() {
        let cam = camera("cam1");
        let encoding = EncodingConfig::default();
        for hw in [HwAccel::Vaapi, HwAccel::Qsv, HwAccel::V4l2m2m, HwAccel::Software] {
            let cmd = ffmpeg_command(&cam, 8554, &encoding, hw);
            assert_eq!(describe_encoder(&cmd), hw.label());
        }

        let mut passthrough_cam = camera("native-h264-cam");
        passthrough_cam.pixel_format = "H264".to_string();
        let cmd = ffmpeg_command(&passthrough_cam, 8554, &encoding, HwAccel::Software);
        assert_eq!(describe_encoder(&cmd), "passthrough (-c copy)");
    }

    #[test]
    fn multiple_cameras_get_distinct_paths() {
        let cams = vec![camera("cam1"), camera("cam2")];
        let config = generate_config(&cams, 8554, 9997, &EncodingConfig::default(), HwAccel::Software);
        assert_eq!(config.paths.len(), 2);
        assert!(config.paths.contains_key("cam1"));
        assert!(config.paths.contains_key("cam2"));
    }

    #[test]
    fn yaml_round_trips() {
        let cams = vec![camera("cam1")];
        let config = generate_config(&cams, 8554, 9997, &EncodingConfig::default(), HwAccel::Software);
        let yaml = to_yaml(&config);
        let parsed: GeneratedConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn backoff_caps_at_30s() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(63), Duration::from_secs(30));
    }

    #[test]
    fn binary_not_found_on_path_when_missing() {
        // SAFETY: test-local mutation of PATH, restored immediately after; no other test
        // in this process depends on PATH concurrently touching this exact value.
        let original = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "/nonexistent-bin-dir") };
        let result = find_binary(None);
        if let Some(p) = original {
            unsafe { std::env::set_var("PATH", p) };
        }
        assert!(matches!(result, Err(MediaMtxError::BinaryNotFoundOnPath)));
    }

    #[test]
    fn configured_binary_must_exist() {
        let result = find_binary(Some(Path::new("/nonexistent/mediamtx")));
        assert!(matches!(
            result,
            Err(MediaMtxError::ConfiguredBinaryNotFound(_))
        ));
    }
}
