use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

pub const SERVICE_NAME: &str = "rtsp-generator";
pub const UNIT_PATH: &str = "/etc/systemd/system/rtsp-generator.service";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("failed to write systemd unit file {path}: {source}")]
    WriteUnit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove systemd unit file {path}: {source}")]
    RemoveUnit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to run `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{command}` exited with a non-zero status")]
    CommandFailed { command: String },
}

/// Renders the systemd unit for the given binary path.
///
/// Note: runs as `User=root` for v1, since the daemon writes to `/etc/rtsp-generator`,
/// `/var/lib/rtsp-generator`, and needs udev netlink + `/dev/video*` access. A dedicated
/// non-root user in the `video` group (plus a udev rule granting netlink access) would be
/// preferable from a least-privilege standpoint; flagged here as a follow-up rather than done
/// in v1 to match the reference unit in the spec.
fn render_unit(exec_path: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=rtsp-generator - webcam RTSP stream generator\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         User=root\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exec = exec_path.display(),
    )
}

fn run(command: &mut Command, description: &str) -> Result<(), ServiceError> {
    let status = command
        .status()
        .map_err(|source| ServiceError::Spawn {
            command: description.to_string(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(ServiceError::CommandFailed {
            command: description.to_string(),
        })
    }
}

/// Writes the unit file, runs `systemctl daemon-reload`, and enables the service. Idempotent:
/// re-running produces the same end state (the unit content is deterministic and always
/// overwritten).
pub fn install(exec_path: &Path, dry_run: bool) -> Result<(), ServiceError> {
    let unit = render_unit(exec_path);

    if dry_run {
        info!(path = UNIT_PATH, "[dry-run] would write systemd unit:\n{unit}");
        info!("[dry-run] would run: systemctl daemon-reload");
        info!("[dry-run] would run: systemctl enable {SERVICE_NAME}");
        return Ok(());
    }

    std::fs::write(UNIT_PATH, unit).map_err(|source| ServiceError::WriteUnit {
        path: PathBuf::from(UNIT_PATH),
        source,
    })?;
    info!(path = UNIT_PATH, "wrote systemd unit");

    run(
        Command::new("systemctl").arg("daemon-reload"),
        "systemctl daemon-reload",
    )?;
    run(
        Command::new("systemctl").args(["enable", SERVICE_NAME]),
        &format!("systemctl enable {SERVICE_NAME}"),
    )?;
    Ok(())
}

/// Disables and stops the service, removes the unit file, and reloads systemd. Idempotent:
/// safe to run when the service was never installed.
pub fn uninstall(dry_run: bool) -> Result<(), ServiceError> {
    if dry_run {
        info!("[dry-run] would run: systemctl disable --now {SERVICE_NAME}");
        info!(path = UNIT_PATH, "[dry-run] would remove unit file if present");
        info!("[dry-run] would run: systemctl daemon-reload");
        return Ok(());
    }

    // `systemctl disable --now` on a never-installed unit exits non-zero; that's fine, we still
    // want to clean up any leftover unit file and reload.
    let _ = Command::new("systemctl")
        .args(["disable", "--now", SERVICE_NAME])
        .status();

    let unit_path = Path::new(UNIT_PATH);
    if unit_path.exists() {
        std::fs::remove_file(unit_path).map_err(|source| ServiceError::RemoveUnit {
            path: unit_path.to_path_buf(),
            source,
        })?;
        info!(path = UNIT_PATH, "removed systemd unit");
    }

    run(
        Command::new("systemctl").arg("daemon-reload"),
        "systemctl daemon-reload",
    )?;
    Ok(())
}

pub fn restart() -> Result<(), ServiceError> {
    run(
        Command::new("systemctl").args(["restart", SERVICE_NAME]),
        &format!("systemctl restart {SERVICE_NAME}"),
    )
}

pub fn stop() -> Result<(), ServiceError> {
    run(
        Command::new("systemctl").args(["stop", SERVICE_NAME]),
        &format!("systemctl stop {SERVICE_NAME}"),
    )
}

/// Prints `systemctl status rtsp-generator` to stdout/stderr as-is (human-readable mode).
pub fn print_status() -> Result<(), ServiceError> {
    let status = Command::new("systemctl")
        .args(["status", SERVICE_NAME])
        .status()
        .map_err(|source| ServiceError::Spawn {
            command: format!("systemctl status {SERVICE_NAME}"),
            source,
        })?;
    // `systemctl status` returns non-zero for inactive-but-valid units; that's a normal
    // outcome for this command, not an error worth propagating.
    let _ = status;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_contains_expected_fields() {
        let unit = render_unit(Path::new("/usr/local/bin/rtsp-gen"));
        assert!(unit.contains("ExecStart=/usr/local/bin/rtsp-gen"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("After=network-online.target"));
    }
}
