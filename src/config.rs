use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{Cli, Resolution};
use crate::mediamtx::{DEFAULT_HLS_PORT, DEFAULT_WEBRTC_PORT};

/// Per-device resolution/fps override, keyed by stable camera name in `devices:`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceOverride {
    pub resolution: Option<String>,
    pub fps: Option<u32>,
}

impl DeviceOverride {
    fn parsed_resolution(&self) -> Option<Resolution> {
        self.resolution.as_deref().and_then(|s| s.parse().ok())
    }
}

/// Which hardware H.264 encoder (if any) to prefer. See `hwaccel::detect`.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HardwarePreference {
    /// Probe VAAPI -> QSV -> V4L2M2M in order, verify each with a real trial encode, use the
    /// first that actually works; fall back to software (`libx264`) if none do.
    #[default]
    Auto,
    /// Only try VAAPI (`h264_vaapi`); fall back to software if it doesn't verify.
    Vaapi,
    /// Only try Intel Quick Sync (`h264_qsv`); fall back to software if it doesn't verify.
    Qsv,
    /// Only try V4L2 M2M (`h264_v4l2m2m`, e.g. Raspberry Pi's hardware encoder); fall back to
    /// software if it doesn't verify.
    V4l2m2m,
    /// Always use software `libx264`, skipping hardware probing entirely.
    Software,
}

/// Tunable H.264 transcode parameters (see `mediamtx::ffmpeg_command`). Only applies to cameras
/// that need transcoding; cameras that natively capture H.264 are always passed through as-is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EncodingConfig {
    /// x264 `-preset` value, used only for the software fallback. `ultrafast` is already the
    /// fastest/lowest-CPU preset x264 offers; changing this away from the default trades CPU for
    /// compression efficiency, not the reverse.
    pub preset: String,
    /// If set, caps output with `-b:v/-maxrate <n>k -bufsize <2n>k` for predictable bandwidth,
    /// on whichever encoder (hardware or software) ends up in use. If unset, software encoding
    /// uses libx264's default constant-quality rate control (CRF 23) instead; hardware encoders
    /// use their own defaults.
    pub bitrate_kbps: Option<u32>,
    /// Which hardware encoder to prefer; see `HardwarePreference`.
    pub hardware: HardwarePreference,
}

impl Default for EncodingConfig {
    fn default() -> Self {
        EncodingConfig {
            preset: "ultrafast".to_string(),
            bitrate_kbps: None,
            hardware: HardwarePreference::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub rtsp_port: u16,
    /// HLS port MediaMTX listens on. Every published camera is automatically viewable over HLS
    /// (browser: `http://<host>:<port>/<name>`, players: `.../index.m3u8`) — MediaMTX serves
    /// every path over every enabled protocol, no separate ffmpeg command needed.
    pub hls_port: u16,
    /// WebRTC port MediaMTX listens on (browser: `http://<host>:<port>/<name>`, WHEP:
    /// `.../whep`). See `hls_port` doc comment — same automatic per-path behavior applies.
    pub webrtc_port: u16,
    pub mediamtx_binary: Option<PathBuf>,
    pub advertise_ip: Option<IpAddr>,
    pub exclude_interfaces: Vec<String>,
    pub devices: HashMap<String, DeviceOverride>,
    /// Where to write the reference streams.yaml. `None` until either `--output` is passed
    /// explicitly or the first interactive run resolves and persists a location (see
    /// `daemon::resolve_streams_path`).
    pub streams_path: Option<PathBuf>,
    pub encoding: EncodingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rtsp_port: 8554,
            hls_port: DEFAULT_HLS_PORT,
            webrtc_port: DEFAULT_WEBRTC_PORT,
            mediamtx_binary: None,
            advertise_ip: None,
            exclude_interfaces: vec![
                "docker0".to_string(),
                "br-".to_string(),
                "veth".to_string(),
                "tailscale0".to_string(),
                "zt".to_string(),
            ],
            devices: HashMap::new(),
            streams_path: None,
            encoding: EncodingConfig::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path} as YAML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("failed to write config file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Config {
    /// Loads config from `path`. If the file does not exist, returns the built-in defaults
    /// without touching disk (the file is only ever created explicitly, e.g. via
    /// `--install-service`, so a fresh `rtsp-gen` run never surprises the user with a new file).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                serde_yaml::from_str(&contents).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Writes this config to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let yaml = serde_yaml::to_string(self).expect("Config always serializes");
        std::fs::write(path, yaml).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Resolves the effective RTSP port: `--port` CLI flag wins, else `rtsp_port` from config.
    pub fn effective_rtsp_port(&self, cli: &Cli) -> u16 {
        cli.port.unwrap_or(self.rtsp_port)
    }

    /// Resolves the effective HLS port: `--hls-port` CLI flag wins, else `hls_port` from config.
    pub fn effective_hls_port(&self, cli: &Cli) -> u16 {
        cli.hls_port.unwrap_or(self.hls_port)
    }

    /// Resolves the effective WebRTC port: `--webrtc-port` CLI flag wins, else `webrtc_port` from
    /// config.
    pub fn effective_webrtc_port(&self, cli: &Cli) -> u16 {
        cli.webrtc_port.unwrap_or(self.webrtc_port)
    }

    /// Resolves the effective resolution/fps for a given camera, applying the precedence
    /// described in the spec:
    ///   1. `--device <name> --res/--fps` (CLI, scoped to that device) — highest priority
    ///   2. `devices.<name>.resolution/fps` in config.yaml
    ///   3. `--res/--fps` (CLI, global, no `--device`)
    ///   4. `None` (caller falls back to auto-selection)
    pub fn effective_override(
        &self,
        cli: &Cli,
        camera_name: &str,
    ) -> Result<(Option<Resolution>, Option<u32>), crate::cli::CliError> {
        let cli_res = cli.parsed_res()?;
        let device_scoped = cli.device.as_deref() == Some(camera_name);

        if device_scoped {
            let res = cli_res.or_else(|| {
                self.devices
                    .get(camera_name)
                    .and_then(DeviceOverride::parsed_resolution)
            });
            let fps = cli.fps.or_else(|| {
                self.devices.get(camera_name).and_then(|d| d.fps)
            });
            return Ok((res, fps));
        }

        if let Some(dev) = self.devices.get(camera_name) {
            let res = dev.parsed_resolution();
            let fps = dev.fps;
            if res.is_some() || fps.is_some() {
                return Ok((res, fps));
            }
        }

        // No device-specific config entry: a global (device-less) CLI override applies.
        if cli.device.is_none() {
            return Ok((cli_res, cli.fps));
        }

        Ok((None, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cli_with(res: Option<&str>, fps: Option<u32>, device: Option<&str>) -> Cli {
        Cli {
            list: false,
            status: false,
            info: false,
            restart: false,
            stop: false,
            install_service: false,
            uninstall_service: false,
            about: false,
            config: PathBuf::from("/dev/null"),
            output: None,
            res: res.map(String::from),
            device: device.map(String::from),
            fps,
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
    fn default_when_file_missing() {
        let cfg = Config::load(Path::new("/nonexistent/path/config.yaml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn device_scoped_cli_overrides_config() {
        let mut cfg = Config::default();
        cfg.devices.insert(
            "cam1".to_string(),
            DeviceOverride {
                resolution: Some("640x480".to_string()),
                fps: Some(10),
            },
        );
        let cli = cli_with(Some("1920x1080"), Some(60), Some("cam1"));
        let (res, fps) = cfg.effective_override(&cli, "cam1").unwrap();
        assert_eq!(res, Some(Resolution { width: 1920, height: 1080 }));
        assert_eq!(fps, Some(60));
    }

    #[test]
    fn config_device_entry_beats_global_cli_flag() {
        let mut cfg = Config::default();
        cfg.devices.insert(
            "cam1".to_string(),
            DeviceOverride {
                resolution: Some("640x480".to_string()),
                fps: Some(10),
            },
        );
        // Global CLI override (no --device) should NOT beat the config.yaml per-device entry.
        let cli = cli_with(Some("1920x1080"), Some(60), None);
        let (res, fps) = cfg.effective_override(&cli, "cam1").unwrap();
        assert_eq!(res, Some(Resolution { width: 640, height: 480 }));
        assert_eq!(fps, Some(10));
    }

    #[test]
    fn global_cli_flag_applies_without_config_entry() {
        let cfg = Config::default();
        let cli = cli_with(Some("1920x1080"), Some(60), None);
        let (res, fps) = cfg.effective_override(&cli, "cam1").unwrap();
        assert_eq!(res, Some(Resolution { width: 1920, height: 1080 }));
        assert_eq!(fps, Some(60));
    }

    #[test]
    fn device_scoped_cli_for_other_device_does_not_apply() {
        let cfg = Config::default();
        let cli = cli_with(Some("1920x1080"), Some(60), Some("cam2"));
        let (res, fps) = cfg.effective_override(&cli, "cam1").unwrap();
        assert_eq!(res, None);
        assert_eq!(fps, None);
    }
}
