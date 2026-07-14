use std::io::Write;
use std::net::IpAddr;
use std::path::Path;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::device::Camera;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CameraEntry {
    pub name: String,
    pub device: String,
    pub resolution: String,
    pub fps: u32,
    pub rtsp_url: String,
    /// Browser: this URL directly. Players (ffmpeg/VLC): append `/index.m3u8`.
    pub hls_url: String,
    /// Browser: this URL directly. WHEP clients: append `/whep`.
    pub webrtc_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamsFile {
    pub generated_at: DateTime<Local>,
    pub host_ip: IpAddr,
    pub rtsp_port: u16,
    pub hls_port: u16,
    pub webrtc_port: u16,
    pub cameras: Vec<CameraEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("failed to create parent directory for {path}: {source}")]
    CreateDir {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write temp file in {dir}: {source}")]
    WriteTemp {
        dir: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to atomically replace {path}: {source}")]
    Rename {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Builds the in-memory reference document for the current camera set. Pure function: no I/O.
pub fn build(
    cameras: &[Camera],
    host_ip: IpAddr,
    rtsp_port: u16,
    hls_port: u16,
    webrtc_port: u16,
) -> StreamsFile {
    let camera_entries = cameras
        .iter()
        .map(|cam| CameraEntry {
            name: cam.name.clone(),
            device: cam.capture_path().to_string_lossy().to_string(),
            resolution: format!("{}x{}", cam.chosen_resolution.0, cam.chosen_resolution.1),
            fps: cam.fps,
            rtsp_url: format!("rtsp://{host_ip}:{rtsp_port}/{}", cam.name),
            hls_url: format!("http://{host_ip}:{hls_port}/{}", cam.name),
            webrtc_url: format!("http://{host_ip}:{webrtc_port}/{}", cam.name),
        })
        .collect();

    StreamsFile {
        generated_at: Local::now(),
        host_ip,
        rtsp_port,
        hls_port,
        webrtc_port,
        cameras: camera_entries,
    }
}

/// Writes `streams` to `path` atomically: write to a temp file in the same directory, then
/// rename over the destination, so other systems on the LAN watching this file never observe
/// a partial read.
pub fn write_atomic(path: &Path, streams: &StreamsFile) -> Result<(), OutputError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| OutputError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;

    let yaml = serde_yaml::to_string(streams).expect("StreamsFile always serializes");

    let mut tmp = tempfile::Builder::new()
        .prefix(".streams.yaml.")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|source| OutputError::WriteTemp {
            dir: dir.to_path_buf(),
            source,
        })?;
    tmp.write_all(yaml.as_bytes())
        .and_then(|_| tmp.flush())
        .map_err(|source| OutputError::WriteTemp {
            dir: dir.to_path_buf(),
            source,
        })?;

    // `tempfile` creates files 0600 by default; this file is a world-readable reference for
    // other local processes/systems on the LAN, and contains no secrets, so open it up.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(|source| OutputError::WriteTemp {
                dir: dir.to_path_buf(),
                source,
            })?;
    }

    tmp.persist(path)
        .map_err(|e| OutputError::Rename {
            path: path.to_path_buf(),
            source: e.error,
        })?;

    Ok(())
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
    fn builds_expected_entries() {
        let cams = vec![camera("logitech-c920")];
        let doc = build(&cams, "192.168.1.50".parse().unwrap(), 8554, 8888, 8889);
        assert_eq!(doc.host_ip, "192.168.1.50".parse::<IpAddr>().unwrap());
        assert_eq!(doc.rtsp_port, 8554);
        assert_eq!(doc.hls_port, 8888);
        assert_eq!(doc.webrtc_port, 8889);
        assert_eq!(doc.cameras.len(), 1);
        assert_eq!(doc.cameras[0].name, "logitech-c920");
        assert_eq!(doc.cameras[0].resolution, "1920x1080");
        assert_eq!(
            doc.cameras[0].rtsp_url,
            "rtsp://192.168.1.50:8554/logitech-c920"
        );
        assert_eq!(
            doc.cameras[0].hls_url,
            "http://192.168.1.50:8888/logitech-c920"
        );
        assert_eq!(
            doc.cameras[0].webrtc_url,
            "http://192.168.1.50:8889/logitech-c920"
        );
        assert_eq!(
            doc.cameras[0].device,
            "/dev/v4l/by-id/usb-logitech-c920-video-index0"
        );
    }

    #[test]
    fn write_then_read_roundtrips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streams.yaml");
        let cams = vec![camera("cam1")];
        let doc = build(&cams, "10.0.0.5".parse().unwrap(), 8554, 8888, 8889);

        write_atomic(&path, &doc).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let read_back: StreamsFile = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(read_back.cameras.len(), 1);
        assert_eq!(read_back.host_ip, doc.host_ip);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "streams.yaml must be world-readable");
        }

        // No leftover temp files in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }
}
