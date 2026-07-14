mod cli;
mod config;
mod daemon;
mod device;
mod hotplug;
mod hwaccel;
mod mediamtx;
mod netinfo;
mod output;
mod service;

use std::process::Command;

use cli::{Action, Cli};
use config::Config;
use tracing_subscriber::EnvFilter;

const EXIT_OK: i32 = 0;
const EXIT_GENERAL_ERROR: i32 = 1;
const EXIT_INVALID_ARGS: i32 = 2;
const EXIT_MEDIAMTX_NOT_FOUND: i32 = 3;
const EXIT_NO_DEVICES: i32 = 4;

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() {
    let (cli, action) = match Cli::parse_validated() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(EXIT_INVALID_ARGS);
        }
    };

    init_tracing(cli.verbose);

    let code = run(cli, action).await;
    std::process::exit(code);
}

async fn run(cli: Cli, action: Action) -> i32 {
    match action {
        Action::About => {
            print_about();
            EXIT_OK
        }
        Action::List => run_list(&cli),
        Action::Status => run_status(&cli).await,
        Action::Info => run_info(&cli),
        Action::Restart => report(service::restart()),
        Action::Stop => report(service::stop()),
        Action::InstallService => run_install_service(&cli),
        Action::UninstallService => report(service::uninstall(cli.dry_run)),
        Action::Run => run_daemon(&cli).await,
    }
}

fn report<E: std::fmt::Display>(result: Result<(), E>) -> i32 {
    match result {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {e}");
            EXIT_GENERAL_ERROR
        }
    }
}

fn print_about() {
    println!("rtsp-gen {}", env!("CARGO_PKG_VERSION"));
    println!("target: {}-{}", std::env::consts::ARCH, std::env::consts::OS);
    println!("license: {}", env!("CARGO_PKG_LICENSE"));
    let repo = env!("CARGO_PKG_REPOSITORY");
    if !repo.is_empty() {
        println!("repository: {repo}");
    }
}

/// Compact per-camera summary: the shape shown by plain `--list` (and its `--json` form).
#[derive(serde::Serialize)]
struct CameraSummary {
    name: String,
    resolution: String,
    fps: u32,
    pixel_format: String,
    rtsp_url: String,
    hls_url: String,
    webrtc_url: String,
}

fn run_list(cli: &Cli) -> i32 {
    let cameras = match device::detect_cameras() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_GENERAL_ERROR;
        }
    };

    if cameras.is_empty() {
        if cli.json {
            println!("[]");
        } else {
            println!("No capture-capable V4L2 devices found.");
        }
        return EXIT_NO_DEVICES;
    }

    let config = match Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_GENERAL_ERROR;
        }
    };

    let cameras = match daemon::apply_overrides(cameras, cli, &config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_GENERAL_ERROR;
        }
    };

    let rtsp_port = config.effective_rtsp_port(cli);
    let hls_port = config.effective_hls_port(cli);
    let webrtc_port = config.effective_webrtc_port(cli);
    let host_ip = match netinfo::detect_lan_ip(config.advertise_ip, &config.exclude_interfaces) {
        Ok(ip) => Some(ip),
        Err(e) => {
            eprintln!("warning: {e}");
            None
        }
    };
    let stream_urls = |name: &str| {
        let host = match host_ip {
            Some(ip) => ip.to_string(),
            None => "<unknown-host>".to_string(),
        };
        StreamUrls {
            rtsp: format!("rtsp://{host}:{rtsp_port}/{name}"),
            hls: format!("http://{host}:{hls_port}/{name}"),
            webrtc: format!("http://{host}:{webrtc_port}/{name}"),
        }
    };

    if cli.json {
        let result = if cli.all {
            serde_json::to_string_pretty(&camera_all_json(&cameras, stream_urls))
        } else {
            let summaries: Vec<CameraSummary> = cameras
                .iter()
                .map(|cam| {
                    let urls = stream_urls(&cam.name);
                    CameraSummary {
                        name: cam.name.clone(),
                        resolution: format!(
                            "{}x{}",
                            cam.chosen_resolution.0, cam.chosen_resolution.1
                        ),
                        fps: cam.fps,
                        pixel_format: cam.pixel_format.clone(),
                        rtsp_url: urls.rtsp,
                        hls_url: urls.hls,
                        webrtc_url: urls.webrtc,
                    }
                })
                .collect();
            serde_json::to_string_pretty(&summaries)
        };
        match result {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("error: failed to serialize camera list: {e}");
                return EXIT_GENERAL_ERROR;
            }
        }
    } else if cli.all {
        print_camera_table_all(&cameras, stream_urls);
    } else {
        print_camera_table(&cameras, stream_urls);
    }

    EXIT_OK
}

/// RTSP/HLS/WebRTC URLs for one camera, all rooted at the same host.
struct StreamUrls {
    rtsp: String,
    hls: String,
    webrtc: String,
}

/// Full `Camera` fields (device path, all supported resolutions, ...) plus stream URLs, for
/// `--list --all --json`.
fn camera_all_json(
    cameras: &[device::Camera],
    stream_urls: impl Fn(&str) -> StreamUrls,
) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = cameras
        .iter()
        .map(|cam| {
            let mut value = serde_json::to_value(cam).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = value.as_object_mut() {
                let urls = stream_urls(&cam.name);
                obj.insert("rtsp_url".to_string(), serde_json::Value::String(urls.rtsp));
                obj.insert("hls_url".to_string(), serde_json::Value::String(urls.hls));
                obj.insert(
                    "webrtc_url".to_string(),
                    serde_json::Value::String(urls.webrtc),
                );
            }
            value
        })
        .collect();
    serde_json::Value::Array(entries)
}

/// Prints the `rtsp:`/`hls:`/`webrtc:` block shared by both table views. Indented lines rather
/// than fixed-width columns, since long camera names and long URLs together blow past any
/// column width worth picking — a table layout just misaligns instead of wrapping.
fn print_url_block(urls: &StreamUrls) {
    println!("    rtsp:   {}", urls.rtsp);
    println!("    hls:    {}", urls.hls);
    println!("    webrtc: {}", urls.webrtc);
}

/// Concise view: name, current setting, then an indented URL block. This is the default
/// `--list` view.
fn print_camera_table(cameras: &[device::Camera], stream_urls: impl Fn(&str) -> StreamUrls) {
    for (i, cam) in cameras.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let setting = format!(
            "{}x{}@{}fps ({})",
            cam.chosen_resolution.0, cam.chosen_resolution.1, cam.fps, cam.pixel_format
        );
        println!("{} - {setting}", cam.name);
        print_url_block(&stream_urls(&cam.name));
    }
}

/// Full view: adds device path and every supported resolution. Shown with `--list --all`.
fn print_camera_table_all(cameras: &[device::Camera], stream_urls: impl Fn(&str) -> StreamUrls) {
    for (i, cam) in cameras.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!(
            "{} - {} - {}x{}@{}fps ({})",
            cam.name,
            cam.device_path.display(),
            cam.chosen_resolution.0,
            cam.chosen_resolution.1,
            cam.fps,
            cam.pixel_format,
        );
        let res_list = cam
            .resolutions
            .iter()
            .map(|(w, h)| format!("{w}x{h}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("    resolutions available: {res_list}");
        print_url_block(&stream_urls(&cam.name));
    }
}

async fn run_status(cli: &Cli) -> i32 {
    if !cli.json {
        return report(service::print_status());
    }

    let is_active = Command::new("systemctl")
        .args(["is-active", service::SERVICE_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let is_enabled = Command::new("systemctl")
        .args(["is-enabled", service::SERVICE_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let api = mediamtx::MediaMtxApi::new(mediamtx::DEFAULT_API_PORT);
    let paths = match api.list_paths().await {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("warning: failed to query MediaMTX API: {e}");
            None
        }
    };

    let report = serde_json::json!({
        "service": {
            "active": is_active,
            "enabled": is_enabled,
        },
        "mediamtx_paths": paths,
    });

    match serde_json::to_string_pretty(&report) {
        Ok(s) => {
            println!("{s}");
            EXIT_OK
        }
        Err(e) => {
            eprintln!("error: failed to serialize status report: {e}");
            EXIT_GENERAL_ERROR
        }
    }
}

/// One process in the rtsp-gen -> MediaMTX -> ffmpeg tree, with its instantaneous `ps` %CPU.
struct ProcEntry {
    pid: u32,
    comm: String,
    pcpu: f64,
}

fn round_to_1dp(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn ps_query(args: &[&str]) -> Vec<ProcEntry> {
    let Ok(output) = Command::new("ps").args(args).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let comm = parts.next()?.to_string();
            let pcpu: f64 = parts.next()?.parse().ok()?;
            Some(ProcEntry { pid, comm, pcpu })
        })
        .collect()
}

/// Walks the process tree rooted at `pid` (rtsp-gen -> MediaMTX -> ffmpeg, two levels deep) and
/// returns each process's instantaneous `ps` %CPU.
fn collect_process_tree_cpu(pid: u32) -> Vec<ProcEntry> {
    let mut entries = ps_query(&["-p", &pid.to_string(), "-o", "pid=,comm=,pcpu="]);
    let children = ps_query(&["--ppid", &pid.to_string(), "-o", "pid=,comm=,pcpu="]);
    for child in &children {
        entries.extend(ps_query(&[
            "--ppid",
            &child.pid.to_string(),
            "-o",
            "pid=,comm=,pcpu=",
        ]));
    }
    entries.extend(children);
    entries
}

/// Reads the generated MediaMTX config and describes which encoder each camera's path is using.
fn read_active_encoders() -> Vec<(String, String)> {
    let Ok(contents) = std::fs::read_to_string(mediamtx::CONFIG_PATH) else {
        return Vec::new();
    };
    let Ok(config) = serde_yaml::from_str::<mediamtx::GeneratedConfig>(&contents) else {
        return Vec::new();
    };
    config
        .paths
        .into_iter()
        .map(|(name, path)| {
            let encoder = mediamtx::describe_encoder(&path.run_on_init).to_string();
            (name, encoder)
        })
        .collect()
}

fn run_info(cli: &Cli) -> i32 {
    let Some(pid) = daemon::running_pid() else {
        if cli.json {
            println!("{}", serde_json::json!({"running": false}));
        } else {
            println!(
                "rtsp-generator does not appear to be running \
                 (no lock file at /var/lib/rtsp-generator/rtsp-gen.lock)."
            );
        }
        return EXIT_GENERAL_ERROR;
    };

    let procs = collect_process_tree_cpu(pid);
    let total_cpu = round_to_1dp(procs.iter().map(|p| p.pcpu).sum());
    let encoders = read_active_encoders();

    if cli.json {
        let report = serde_json::json!({
            "running": true,
            "pid": pid,
            "encoders": encoders.iter().map(|(name, enc)| (name.clone(), enc.clone())).collect::<std::collections::HashMap<_, _>>(),
            "cpu": {
                "total_pcpu": total_cpu,
                "processes": procs.iter().map(|p| serde_json::json!({
                    "pid": p.pid,
                    "comm": p.comm,
                    "pcpu": round_to_1dp(p.pcpu),
                })).collect::<Vec<_>>(),
            },
        });
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("error: failed to serialize info report: {e}");
                return EXIT_GENERAL_ERROR;
            }
        }
    } else {
        println!("rtsp-generator is running (pid {pid})");
        println!();
        if encoders.is_empty() {
            println!("Encoding: (no generated MediaMTX config found yet)");
        } else {
            println!("Encoding:");
            for (name, encoder) in &encoders {
                println!("  {name:<35} {encoder}");
            }
        }
        println!();
        if procs.is_empty() {
            println!("CPU usage: (could not read process tree; is `ps` installed?)");
        } else {
            println!("CPU usage:");
            for p in &procs {
                println!("  {:<12} (pid {}) {:>5.1}%", p.comm, p.pid, p.pcpu);
            }
            println!("  {:<12} {:>5.1}%", "total", total_cpu);
        }
    }

    EXIT_OK
}

fn run_install_service(cli: &Cli) -> i32 {
    let exec_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to determine the path of the running executable: {e}");
            return EXIT_GENERAL_ERROR;
        }
    };

    if !cli.dry_run && !cli.config.exists() {
        if let Err(e) = Config::default().save(&cli.config) {
            eprintln!(
                "warning: failed to write default config to {}: {e}",
                cli.config.display()
            );
        } else {
            println!("wrote default config to {}", cli.config.display());
        }
    }

    report(service::install(&exec_path, cli.dry_run))
}

async fn run_daemon(cli: &Cli) -> i32 {
    let config = match Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_GENERAL_ERROR;
        }
    };

    match daemon::run(cli, config).await {
        Ok(daemon::RunOutcome::Success) => EXIT_OK,
        Ok(daemon::RunOutcome::NoCamerasFound) => {
            eprintln!(
                "error: no capture-capable V4L2 devices found. Plug in a webcam and try again."
            );
            EXIT_NO_DEVICES
        }
        Ok(daemon::RunOutcome::AlreadyRunning(report)) => {
            println!("{report}");
            EXIT_GENERAL_ERROR
        }
        Err(e) => {
            if e.downcast_ref::<mediamtx::MediaMtxError>().is_some() {
                eprintln!("error: {e}");
                return EXIT_MEDIAMTX_NOT_FOUND;
            }
            eprintln!("error: {e:#}");
            EXIT_GENERAL_ERROR
        }
    }
}
