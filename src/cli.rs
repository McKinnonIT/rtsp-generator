use std::path::PathBuf;

use clap::Parser;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/rtsp-generator/config.yaml";
pub const DEFAULT_OUTPUT_PATH: &str = "/etc/rtsp-generator/streams.yaml";

/// Detects V4L2 webcams and publishes them as RTSP streams via MediaMTX.
#[derive(Parser, Debug)]
#[command(name = "rtsp-gen", version, about, long_about = None)]
pub struct Cli {
    /// Detect webcams and print a table (name, current resolution/fps, RTSP URL). Pair with
    /// --all to see full capabilities (device path, all supported resolutions). No side effects.
    #[arg(short = 'l', long)]
    pub list: bool,

    /// Query systemd (and optionally MediaMTX) for current status.
    #[arg(short = 's', long)]
    pub status: bool,

    /// Report the running instance's active encoding backend per camera (hardware or software)
    /// and current CPU usage of the rtsp-gen/MediaMTX/ffmpeg process tree.
    #[arg(long)]
    pub info: bool,

    /// Restart the rtsp-generator systemd service.
    #[arg(long)]
    pub restart: bool,

    /// Stop the rtsp-generator systemd service.
    #[arg(long)]
    pub stop: bool,

    /// Install the systemd unit and enable the service.
    #[arg(long = "install-service")]
    pub install_service: bool,

    /// Disable and remove the systemd unit.
    #[arg(long = "uninstall-service")]
    pub uninstall_service: bool,

    /// Print version, build target, license, and repo URL.
    #[arg(long)]
    pub about: bool,

    /// Path to config.yaml.
    #[arg(short = 'c', long, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// Path to write the reference streams.yaml. If not given, the first interactive run of
    /// `rtsp-gen` (no action flag) prompts for a location (default suggestion: ~/.rtsp-gen) and
    /// remembers the answer in config.yaml; non-interactive runs (e.g. under systemd) fall back
    /// to /etc/rtsp-generator/streams.yaml.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Override resolution, e.g. 1280x720. Global unless paired with --device.
    #[arg(long, value_name = "WxH")]
    pub res: Option<String>,

    /// Target one device by stable id for scoped --res/--fps overrides.
    #[arg(short = 'd', long)]
    pub device: Option<String>,

    /// Override framerate.
    #[arg(long)]
    pub fps: Option<u32>,

    /// RTSP port for MediaMTX.
    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    /// HLS port for MediaMTX (browser: http://host:PORT/<name>, players: .../index.m3u8).
    #[arg(long)]
    pub hls_port: Option<u16>,

    /// WebRTC port for MediaMTX (browser: http://host:PORT/<name>, WHEP: .../whep).
    #[arg(long)]
    pub webrtc_port: Option<u16>,

    /// Machine-readable output for --list / --status.
    #[arg(long)]
    pub json: bool,

    /// With --list, show full device capabilities (device path, all supported resolutions)
    /// instead of just the current setting.
    #[arg(long)]
    pub all: bool,

    /// Show what would be generated/changed without writing/applying anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Increase log verbosity, repeatable (-vv, -vvv).
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// The single action requested for this invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No action flag given: full run (systemd ExecStart command).
    Run,
    List,
    Status,
    Info,
    Restart,
    Stop,
    InstallService,
    UninstallService,
    About,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(
        "conflicting action flags given: {0} (only one of --list/--status/--info/--restart/--stop/--install-service/--uninstall-service/--about may be set)"
    )]
    ConflictingActions(String),

    #[error("invalid --res value '{0}': expected format WIDTHxHEIGHT, e.g. 1280x720")]
    InvalidResolution(String),
}

/// A parsed WIDTHxHEIGHT resolution override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl std::str::FromStr for Resolution {
    type Err = CliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (w, h) = s
            .split_once(['x', 'X'])
            .ok_or_else(|| CliError::InvalidResolution(s.to_string()))?;
        let width: u32 = w
            .trim()
            .parse()
            .map_err(|_| CliError::InvalidResolution(s.to_string()))?;
        let height: u32 = h
            .trim()
            .parse()
            .map_err(|_| CliError::InvalidResolution(s.to_string()))?;
        if width == 0 || height == 0 {
            return Err(CliError::InvalidResolution(s.to_string()));
        }
        Ok(Resolution { width, height })
    }
}

impl Cli {
    /// Parses `argv` and validates that at most one action flag is set and that `--res`
    /// (if given) parses as a valid resolution.
    pub fn parse_validated() -> Result<(Self, Action), CliError> {
        let cli = Self::parse();
        let action = cli.validated_action()?;
        cli.parsed_res()?;
        Ok((cli, action))
    }

    fn validated_action(&self) -> Result<Action, CliError> {
        let flags: [(bool, &str, Action); 8] = [
            (self.list, "--list", Action::List),
            (self.status, "--status", Action::Status),
            (self.info, "--info", Action::Info),
            (self.restart, "--restart", Action::Restart),
            (self.stop, "--stop", Action::Stop),
            (
                self.install_service,
                "--install-service",
                Action::InstallService,
            ),
            (
                self.uninstall_service,
                "--uninstall-service",
                Action::UninstallService,
            ),
            (self.about, "--about", Action::About),
        ];

        let set: Vec<&str> = flags
            .iter()
            .filter(|(enabled, _, _)| *enabled)
            .map(|(_, name, _)| *name)
            .collect();

        match set.len() {
            0 => Ok(Action::Run),
            1 => Ok(flags.into_iter().find(|(e, _, _)| *e).unwrap().2),
            _ => Err(CliError::ConflictingActions(set.join(", "))),
        }
    }

    /// Parses the `--res` flag, if present.
    pub fn parsed_res(&self) -> Result<Option<Resolution>, CliError> {
        self.res.as_deref().map(str::parse).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cli() -> Cli {
        Cli {
            list: false,
            status: false,
            info: false,
            restart: false,
            stop: false,
            install_service: false,
            uninstall_service: false,
            about: false,
            config: DEFAULT_CONFIG_PATH.into(),
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
    fn no_flags_means_run() {
        let cli = base_cli();
        assert_eq!(cli.validated_action().unwrap(), Action::Run);
    }

    #[test]
    fn single_flag_ok() {
        let mut cli = base_cli();
        cli.list = true;
        assert_eq!(cli.validated_action().unwrap(), Action::List);
    }

    #[test]
    fn conflicting_flags_rejected() {
        let mut cli = base_cli();
        cli.list = true;
        cli.status = true;
        assert!(matches!(
            cli.validated_action(),
            Err(CliError::ConflictingActions(_))
        ));
    }

    #[test]
    fn parses_resolution() {
        let r: Resolution = "1920x1080".parse().unwrap();
        assert_eq!(r, Resolution { width: 1920, height: 1080 });
    }

    #[test]
    fn rejects_bad_resolution() {
        assert!("1920".parse::<Resolution>().is_err());
        assert!("0x0".parse::<Resolution>().is_err());
        assert!("abcxdef".parse::<Resolution>().is_err());
    }
}
