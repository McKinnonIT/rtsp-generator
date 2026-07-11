use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tracing::{info, warn};

use crate::config::HardwarePreference;

/// The default DRI render node VAAPI encoders use on Linux.
pub const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwAccel {
    Vaapi,
    Qsv,
    V4l2m2m,
    Software,
}

impl HwAccel {
    pub fn label(&self) -> &'static str {
        match self {
            HwAccel::Vaapi => "VAAPI (h264_vaapi)",
            HwAccel::Qsv => "Intel Quick Sync (h264_qsv)",
            HwAccel::V4l2m2m => "V4L2 M2M (h264_v4l2m2m)",
            HwAccel::Software => "software (libx264)",
        }
    }
}

/// The hardware backends to try, in priority order, for a given preference. Pure and
/// unit-testable; the actual verification (`probe`) needs a real ffmpeg + device and isn't.
fn candidates_for(preference: HardwarePreference) -> &'static [HwAccel] {
    match preference {
        HardwarePreference::Auto => &[HwAccel::Vaapi, HwAccel::Qsv, HwAccel::V4l2m2m],
        HardwarePreference::Vaapi => &[HwAccel::Vaapi],
        HardwarePreference::Qsv => &[HwAccel::Qsv],
        HardwarePreference::V4l2m2m => &[HwAccel::V4l2m2m],
        HardwarePreference::Software => &[],
    }
}

async fn probe_ok(args: &[&str]) -> bool {
    let run = Command::new("ffmpeg").args(args).output();
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, run).await,
        Ok(Ok(output)) if output.status.success()
    )
}

/// Test-encodes a couple of synthetic frames with each candidate encoder to confirm it actually
/// works, rather than just checking that ffmpeg lists the codec — `ffmpeg -encoders` will
/// happily list `h264_vaapi` even when the driver, permissions, or hardware don't support it.
async fn probe(candidate: HwAccel) -> bool {
    match candidate {
        HwAccel::Vaapi => {
            if !Path::new(VAAPI_DEVICE).exists() {
                return false;
            }
            probe_ok(&[
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1280x720:rate=30",
                "-vaapi_device",
                VAAPI_DEVICE,
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-frames:v",
                "2",
                "-f",
                "null",
                "-",
            ])
            .await
        }
        HwAccel::Qsv => {
            probe_ok(&[
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1280x720:rate=30",
                "-c:v",
                "h264_qsv",
                "-frames:v",
                "2",
                "-f",
                "null",
                "-",
            ])
            .await
        }
        HwAccel::V4l2m2m => {
            probe_ok(&[
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1280x720:rate=30",
                "-c:v",
                "h264_v4l2m2m",
                "-frames:v",
                "2",
                "-f",
                "null",
                "-",
            ])
            .await
        }
        HwAccel::Software => true,
    }
}

/// Probes for a working hardware H.264 encoder per `preference`, verifying each candidate with a
/// real trial encode (see `probe`), and falls back to software if none pan out. This is a
/// synthetic-input check only — it can still miss failures that only show up against a real
/// capture pipeline, which is why callers should also verify against live camera data at runtime
/// (see `daemon::verify_and_maybe_downgrade`) and downgrade to software if that fails too.
pub async fn detect(preference: HardwarePreference) -> HwAccel {
    let candidates = candidates_for(preference);
    for &candidate in candidates {
        if probe(candidate).await {
            info!(backend = candidate.label(), "hardware H.264 encoder detected and verified");
            return candidate;
        }
        warn!(
            backend = candidate.label(),
            "hardware encoder not usable (absent, misconfigured, or failed a trial encode)"
        );
    }
    if !candidates.is_empty() {
        info!("no working hardware H.264 encoder found; using software (libx264)");
    }
    HwAccel::Software
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_tries_all_three_in_priority_order() {
        assert_eq!(
            candidates_for(HardwarePreference::Auto),
            &[HwAccel::Vaapi, HwAccel::Qsv, HwAccel::V4l2m2m]
        );
    }

    #[test]
    fn forced_preference_only_tries_that_one() {
        assert_eq!(candidates_for(HardwarePreference::Vaapi), &[HwAccel::Vaapi]);
        assert_eq!(candidates_for(HardwarePreference::Qsv), &[HwAccel::Qsv]);
        assert_eq!(
            candidates_for(HardwarePreference::V4l2m2m),
            &[HwAccel::V4l2m2m]
        );
    }

    #[test]
    fn software_preference_tries_nothing() {
        assert_eq!(candidates_for(HardwarePreference::Software), &[]);
    }

    #[tokio::test]
    async fn software_preference_detects_as_software_without_probing() {
        assert_eq!(detect(HardwarePreference::Software).await, HwAccel::Software);
    }
}
