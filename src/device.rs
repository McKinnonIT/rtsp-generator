use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use v4l::capability::Flags;
use v4l::format::FourCC;
use v4l::video::Capture;
use v4l::{Capabilities, Device as V4lDevice};

const MIN_FPS: u32 = 15;
/// Default target resolution: caps auto-selection at 720p to keep the H.264 transcode CPU cost
/// reasonable (see `mediamtx::ffmpeg_command`). Overridable via `--res`/config.yaml as usual.
const DEFAULT_TARGET_WIDTH: u32 = 1280;
const DEFAULT_TARGET_HEIGHT: u32 = 720;
/// Pixel formats we know how to feed to ffmpeg's `-input_format`, in preference order.
const PREFERRED_FORMATS: &[&str] = &["MJPG", "H264", "YUYV"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Camera {
    /// Stable name, used as the RTSP path segment.
    pub name: String,
    /// `/dev/videoN` at detection time.
    pub device_path: PathBuf,
    /// `/dev/v4l/by-id/...` symlink, if present.
    pub stable_path: Option<PathBuf>,
    pub resolutions: Vec<(u32, u32)>,
    pub chosen_resolution: (u32, u32),
    pub fps: u32,
    /// e.g. "MJPG", "YUYV".
    pub pixel_format: String,
}

impl Camera {
    /// The path ffmpeg should open: the stable by-id symlink if available, else the raw devnode.
    pub fn capture_path(&self) -> &Path {
        self.stable_path.as_deref().unwrap_or(&self.device_path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("failed to enumerate /dev for video devices: {0}")]
    Enumerate(#[source] std::io::Error),
    #[error("failed to open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to query capabilities of {path}: {source}")]
    QueryCaps {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Returns true if the device's capabilities advertise `V4L2_CAP_VIDEO_CAPTURE`.
///
/// This excludes metadata-only nodes many webcams also expose (e.g. `/dev/video1` alongside
/// `/dev/video0` for the same physical camera).
pub fn is_capture_capable(caps: &Capabilities) -> bool {
    caps.capabilities.contains(Flags::VIDEO_CAPTURE)
}

/// Lists `/dev/videoN` nodes in ascending numeric order.
fn enumerate_video_nodes() -> Result<Vec<PathBuf>, DeviceError> {
    let mut nodes = Vec::new();
    for entry in fs::read_dir("/dev").map_err(DeviceError::Enumerate)? {
        let entry = entry.map_err(DeviceError::Enumerate)?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if let Some(suffix) = name.strip_prefix("video") {
            if suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty() {
                nodes.push(entry.path());
            }
        }
    }
    nodes.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_string_lossy().strip_prefix("video").map(str::to_string))
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    Ok(nodes)
}

/// Picks the best-supported pixel format from the device's advertised formats.
fn choose_pixel_format(dev: &V4lDevice) -> Option<FourCC> {
    let formats = dev.enum_formats().ok()?;
    if formats.is_empty() {
        return None;
    }
    for preferred in PREFERRED_FORMATS {
        if let Some(f) = formats.iter().find(|f| f.fourcc.str() == Ok(*preferred)) {
            return Some(f.fourcc);
        }
    }
    formats.first().map(|f| f.fourcc)
}

/// Returns the discrete resolutions supported for `fourcc`, largest first.
fn discrete_resolutions(dev: &V4lDevice, fourcc: FourCC) -> Vec<(u32, u32)> {
    let Ok(sizes) = dev.enum_framesizes(fourcc) else {
        return Vec::new();
    };
    let mut resolutions: Vec<(u32, u32)> = sizes
        .into_iter()
        .flat_map(|fs| fs.size.to_discrete())
        .map(|d| (d.width, d.height))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    resolutions.sort_by_key(|(w, h)| std::cmp::Reverse((*w as u64) * (*h as u64)));
    resolutions
}

/// Returns the maximum supported fps for `fourcc` at `width`x`height`.
fn max_fps(dev: &V4lDevice, fourcc: FourCC, width: u32, height: u32) -> Option<u32> {
    let intervals = dev.enum_frameintervals(fourcc, width, height).ok()?;
    intervals
        .into_iter()
        .filter_map(|fi| match fi.interval {
            v4l::frameinterval::FrameIntervalEnum::Discrete(frac) if frac.numerator > 0 => {
                Some(frac.denominator / frac.numerator)
            }
            v4l::frameinterval::FrameIntervalEnum::Stepwise(sw) if sw.max.numerator > 0 => {
                Some(sw.max.denominator / sw.max.numerator)
            }
            _ => None,
        })
        .max()
}

/// Picks the default (resolution, fps) from `resolutions` (must be sorted largest-first by
/// area), given a way to look up the max fps for any candidate. Pure and hardware-independent
/// so it's unit-testable without a real device.
///
/// Priority order:
///   1. The largest resolution at or below the 720p target that supports >= 15fps — this caps
///      the default at 720p to keep H.264 transcode CPU cost reasonable.
///   2. If the device's smallest mode is still above 720p, the smallest resolution that
///      supports >= 15fps (best available approximation of "as low-CPU as this device allows").
///   3. If nothing meets 15fps anywhere, the largest resolution available at its own best fps.
fn pick_default_resolution(
    resolutions: &[(u32, u32)],
    fps_lookup: impl Fn(u32, u32) -> Option<u32>,
) -> ((u32, u32), u32) {
    let target_area = u64::from(DEFAULT_TARGET_WIDTH) * u64::from(DEFAULT_TARGET_HEIGHT);

    let mut best_at_or_below_target: Option<((u32, u32), u32)> = None;
    let mut smallest_above_target_qualifying: Option<((u32, u32), u32)> = None;
    let mut best_any: Option<((u32, u32), u32)> = None;

    for &(w, h) in resolutions {
        let fps = fps_lookup(w, h).unwrap_or(0);
        if best_any.is_none() {
            best_any = Some(((w, h), fps));
        }

        if fps >= MIN_FPS {
            let area = u64::from(w) * u64::from(h);
            if area <= target_area {
                if best_at_or_below_target.is_none() {
                    best_at_or_below_target = Some(((w, h), fps));
                }
            } else {
                // `resolutions` is sorted largest-first, so later above-target entries are
                // smaller; keep overwriting so we end up with the smallest qualifying one.
                smallest_above_target_qualifying = Some(((w, h), fps));
            }
        }
    }

    best_at_or_below_target
        .or(smallest_above_target_qualifying)
        .or(best_any)
        .unwrap_or(((0, 0), 0))
}

/// Hardware-facing wrapper around `pick_default_resolution`.
fn choose_default_resolution_fps(
    dev: &V4lDevice,
    fourcc: FourCC,
    resolutions: &[(u32, u32)],
) -> ((u32, u32), u32) {
    pick_default_resolution(resolutions, |w, h| max_fps(dev, fourcc, w, h))
}

/// Detects all V4L2 capture-capable webcams currently attached to the system.
pub fn detect_cameras() -> Result<Vec<Camera>, DeviceError> {
    let nodes = enumerate_video_nodes()?;
    let mut used_names: HashSet<String> = HashSet::new();
    let mut cameras = Vec::new();

    for path in nodes {
        let dev = match V4lDevice::with_path(&path) {
            Ok(d) => d,
            Err(source) => {
                // Node disappeared or is inaccessible; skip rather than fail the whole run.
                tracing::debug!(
                    error = %DeviceError::Open { path: path.clone(), source },
                    "skipping video node"
                );
                continue;
            }
        };

        let caps = match dev.query_caps() {
            Ok(c) => c,
            Err(source) => {
                tracing::debug!(
                    error = %DeviceError::QueryCaps { path: path.clone(), source },
                    "skipping video node"
                );
                continue;
            }
        };

        if !is_capture_capable(&caps) {
            continue;
        }

        let Some(fourcc) = choose_pixel_format(&dev) else {
            continue;
        };

        let resolutions = discrete_resolutions(&dev, fourcc);
        if resolutions.is_empty() {
            continue;
        }

        let (chosen_resolution, fps) = choose_default_resolution_fps(&dev, fourcc, &resolutions);

        let stable_path = find_by_id_symlink(&path);
        let base_name = derive_name(&path, stable_path.as_deref());
        let name = disambiguate(base_name, &path, &mut used_names);

        cameras.push(Camera {
            name,
            device_path: path,
            stable_path,
            resolutions,
            chosen_resolution,
            fps,
            pixel_format: fourcc.str().unwrap_or("????").to_string(),
        });
    }

    Ok(cameras)
}

/// Finds the `/dev/v4l/by-id/...` symlink that resolves to `devnode`, if any.
fn find_by_id_symlink(devnode: &Path) -> Option<PathBuf> {
    let canonical_target = fs::canonicalize(devnode).ok()?;
    let entries = fs::read_dir("/dev/v4l/by-id").ok()?;
    for entry in entries.flatten() {
        let link_path = entry.path();
        if let Ok(resolved) = fs::canonicalize(&link_path) {
            if resolved == canonical_target {
                return Some(link_path);
            }
        }
    }
    None
}

/// Reads the `ID_V4L_PRODUCT` udev property for this device node, if the udev database has it.
fn udev_product_name(devnode: &Path) -> Option<String> {
    let sysname = devnode.file_name()?.to_string_lossy().to_string();
    let dev = udev::Device::from_subsystem_sysname("video4linux".to_string(), sysname).ok()?;
    dev.property_value("ID_V4L_PRODUCT")
        .map(|v| v.to_string_lossy().to_string())
}

/// Strips the `usb-` prefix and `-video-indexN` suffix conventionally used in
/// `/dev/v4l/by-id/` symlink names, e.g.
/// `usb-Logitech_Webcam_C920_ABC123-video-index0` -> `Logitech_Webcam_C920_ABC123`.
fn strip_by_id_decoration(name: &str) -> &str {
    let name = name.strip_prefix("usb-").unwrap_or(name);
    match name.rfind("-video-index") {
        Some(idx) => &name[..idx],
        None => name,
    }
}

/// Slugifies a name for use as an RTSP path segment: lowercase, alphanumerics and
/// hyphens only, no repeated or leading/trailing hyphens.
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_was_hyphen = true; // suppresses a leading hyphen
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen {
            slug.push('-');
            last_was_hyphen = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "camera".to_string()
    } else {
        slug
    }
}

/// A fixed set of short, recognizable words used to give each camera a memorable name suffix
/// (see `animal_suffix`). Order matters for stability: never reorder or remove entries, since
/// that would change which animal an existing camera's serial hashes to across upgrades.
const ANIMAL_NAMES: &[&str] = &[
    "badger", "otter", "falcon", "lynx", "heron", "marten", "wolverine", "kestrel", "puffin",
    "gecko", "mongoose", "capybara", "quokka", "narwhal", "pangolin", "ocelot", "serval", "tapir",
    "coyote", "jackal", "ibis", "stoat", "civet", "raccoon", "wombat", "dingo", "gazelle",
    "meerkat", "toucan", "egret", "ferret", "mink", "weasel", "possum", "armadillo", "alpaca",
    "llama", "bison", "moose", "elk", "caribou", "antelope", "impala", "warthog", "hyena",
    "cheetah", "leopard", "panther", "cougar", "bobcat", "crane", "stork", "pelican", "albatross",
    "osprey", "harrier", "buzzard", "condor", "vulture", "magpie", "raven", "sparrow", "finch",
    "swallow", "kingfisher", "woodpecker", "chameleon", "iguana", "skink", "salamander", "newt",
    "axolotl", "platypus", "echidna", "dugong", "manatee", "walrus", "seal",
];

/// Deterministically maps `seed` (some stable per-device identifier, e.g. a USB serial) to one
/// of `ANIMAL_NAMES`. Same seed always yields the same animal, so a given physical camera keeps
/// its name across reboots and replugs — this is a stable identifier, not cosmetic randomness.
fn animal_suffix(seed: &str) -> &'static str {
    let hash = seed
        .bytes()
        .fold(0xcbf29ce484222325u64, |acc, b| {
            (acc ^ b as u64).wrapping_mul(0x100000001b3)
        });
    ANIMAL_NAMES[(hash as usize) % ANIMAL_NAMES.len()]
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Splits an underscore-separated device identifier like `046d_HD_Pro_Webcam_C920_7438C0DF` into
/// a human-readable model portion (`HD_Pro_Webcam_C920`) and a seed for `animal_suffix` (the full
/// original string, which is the most specific stable identifier available). Strips a leading
/// 4-hex-digit USB vendor ID and a trailing hex-looking serial number when present — neither is
/// meaningful to a human reading RTSP path names. If nothing but hex-looking tokens would be left
/// (e.g. `046d_7438C0DF`, vendor + serial with no real model text), keeps the original string
/// whole instead: there's no model name to extract, so stripping would just lose information.
fn split_vendor_model_serial(name: &str) -> (String, String) {
    let parts: Vec<&str> = name.split('_').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return (name.to_string(), name.to_string());
    }

    let mut start = 0;
    let mut end = parts.len();

    if parts[0].len() == 4 && is_hex(parts[0]) {
        start += 1;
    }
    if end > start && parts[end - 1].len() >= 4 && is_hex(parts[end - 1]) {
        end -= 1;
    }

    let model = if start < end && !parts[start..end].iter().all(|p| is_hex(p)) {
        parts[start..end].join("_")
    } else {
        parts.join("_")
    };

    (model, name.to_string())
}

/// Derives the stable, human-friendly name for a camera: by-id symlink, else `ID_V4L_PRODUCT`,
/// else `videoN`, with the USB vendor ID and serial number stripped from the visible part and
/// replaced with a deterministic animal name (e.g. `hd-pro-webcam-c920-badger`) — memorable, and
/// still unique per physical device since it's derived from that device's actual serial.
fn derive_name(devnode: &Path, stable_path: Option<&Path>) -> String {
    let raw_name = stable_path
        .and_then(|stable| stable.file_name())
        .map(|n| strip_by_id_decoration(&n.to_string_lossy()).to_string())
        .or_else(|| udev_product_name(devnode))
        .unwrap_or_else(|| {
            devnode
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

    let (model, seed) = split_vendor_model_serial(&raw_name);
    format!("{}-{}", slugify(&model), animal_suffix(&seed))
}

/// Returns a short suffix derived from the device's USB port path (e.g. "1-1.2"),
/// for disambiguating name collisions.
fn usb_port_suffix(devnode: &Path) -> Option<String> {
    let sysname = devnode.file_name()?.to_string_lossy().to_string();
    let dev = udev::Device::from_subsystem_sysname("video4linux".to_string(), sysname).ok()?;
    let usb_parent = dev.parent_with_subsystem("usb").ok()??;
    Some(slugify(&usb_parent.sysname().to_string_lossy()))
}

/// Ensures `base_name` is unique among `used`, appending a USB-port-derived (or numeric)
/// suffix on collision.
fn disambiguate(base_name: String, devnode: &Path, used: &mut HashSet<String>) -> String {
    if used.insert(base_name.clone()) {
        return base_name;
    }

    if let Some(suffix) = usb_port_suffix(devnode) {
        let candidate = format!("{base_name}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }

    let mut n = 2;
    loop {
        let candidate = format!("{base_name}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use v4l::capability::Flags as CapFlags;

    /// Builds a `fps_lookup` closure from a fixed table, for testing `pick_default_resolution`
    /// without real hardware.
    fn fps_table(entries: &[((u32, u32), u32)]) -> impl Fn(u32, u32) -> Option<u32> {
        let map: HashMap<(u32, u32), u32> = entries.iter().copied().collect();
        move |w, h| map.get(&(w, h)).copied()
    }

    #[test]
    fn picks_720p_over_1080p_when_both_qualify() {
        // Sorted largest-first, as `discrete_resolutions` produces.
        let resolutions = vec![(1920, 1080), (1280, 720), (640, 480)];
        let fps = fps_table(&[((1920, 1080), 30), ((1280, 720), 30), ((640, 480), 30)]);
        assert_eq!(
            pick_default_resolution(&resolutions, fps),
            ((1280, 720), 30)
        );
    }

    #[test]
    fn falls_back_below_720p_when_720p_missing() {
        let resolutions = vec![(1920, 1080), (640, 480)];
        let fps = fps_table(&[((1920, 1080), 30), ((640, 480), 30)]);
        assert_eq!(pick_default_resolution(&resolutions, fps), ((640, 480), 30));
    }

    #[test]
    fn falls_back_below_720p_when_720p_too_slow() {
        let resolutions = vec![(1920, 1080), (1280, 720), (640, 480)];
        // 720p only manages 10fps here, below the 15fps threshold.
        let fps = fps_table(&[((1920, 1080), 30), ((1280, 720), 10), ((640, 480), 30)]);
        assert_eq!(pick_default_resolution(&resolutions, fps), ((640, 480), 30));
    }

    #[test]
    fn uses_smallest_qualifying_mode_when_device_never_goes_below_720p() {
        // e.g. a camera whose only modes are 4K and 1080p.
        let resolutions = vec![(3840, 2160), (1920, 1080)];
        let fps = fps_table(&[((3840, 2160), 30), ((1920, 1080), 30)]);
        assert_eq!(
            pick_default_resolution(&resolutions, fps),
            ((1920, 1080), 30)
        );
    }

    #[test]
    fn falls_back_to_largest_available_when_nothing_hits_min_fps() {
        let resolutions = vec![(1920, 1080), (1280, 720)];
        let fps = fps_table(&[((1920, 1080), 5), ((1280, 720), 5)]);
        assert_eq!(
            pick_default_resolution(&resolutions, fps),
            ((1920, 1080), 5)
        );
    }

    #[test]
    fn exact_720p_not_required_area_based_cap_applies() {
        // No exact 720p mode, but a slightly smaller-area 16:10 mode exists alongside 1080p.
        let resolutions = vec![(1920, 1080), (1280, 700), (640, 480)];
        let fps = fps_table(&[((1920, 1080), 30), ((1280, 700), 30), ((640, 480), 30)]);
        assert_eq!(
            pick_default_resolution(&resolutions, fps),
            ((1280, 700), 30)
        );
    }

    fn caps_with(flags: CapFlags) -> Capabilities {
        Capabilities {
            driver: "uvcvideo".to_string(),
            card: "Test Camera".to_string(),
            bus: "usb-0000:00:14.0-1".to_string(),
            version: (5, 15, 0),
            capabilities: flags,
        }
    }

    #[test]
    fn capture_capable_devices_pass() {
        let caps = caps_with(CapFlags::VIDEO_CAPTURE | CapFlags::STREAMING);
        assert!(is_capture_capable(&caps));
    }

    #[test]
    fn metadata_only_devices_are_excluded() {
        let caps = caps_with(CapFlags::META_CAPTURE | CapFlags::STREAMING);
        assert!(!is_capture_capable(&caps));
    }

    #[test]
    fn output_only_devices_are_excluded() {
        let caps = caps_with(CapFlags::VIDEO_OUTPUT);
        assert!(!is_capture_capable(&caps));
    }

    #[test]
    fn strips_usb_and_video_index_decoration() {
        let stripped = strip_by_id_decoration("usb-Logitech_Webcam_C920_ABC123-video-index0");
        assert_eq!(stripped, "Logitech_Webcam_C920_ABC123");
    }

    #[test]
    fn slugify_lowercases_and_hyphenates() {
        assert_eq!(slugify("Logitech_Webcam C920!!"), "logitech-webcam-c920");
        assert_eq!(slugify("  leading/trailing  "), "leading-trailing");
        assert_eq!(slugify(""), "camera");
    }

    #[test]
    fn disambiguate_appends_numeric_suffix_when_no_usb_info() {
        let mut used = HashSet::new();
        // A devnode with no backing udev entry (real or otherwise) so `usb_port_suffix`
        // reliably returns `None` regardless of what hardware the test happens to run on.
        let path = PathBuf::from("/dev/video_does_not_exist");
        let first = disambiguate("cam".to_string(), &path, &mut used);
        assert_eq!(first, "cam");
        let second = disambiguate("cam".to_string(), &path, &mut used);
        assert_eq!(second, "cam-2");
    }

    #[test]
    fn splits_vendor_id_and_serial_from_typical_by_id_name() {
        let (model, seed) = split_vendor_model_serial("046d_HD_Pro_Webcam_C920_7438C0DF");
        assert_eq!(model, "HD_Pro_Webcam_C920");
        assert_eq!(seed, "046d_HD_Pro_Webcam_C920_7438C0DF");
    }

    #[test]
    fn split_never_strips_down_to_nothing() {
        // Only a vendor id and a serial, no actual model text: keep everything rather than
        // stripping both and ending up with an empty model.
        let (model, _) = split_vendor_model_serial("046d_7438C0DF");
        assert_eq!(model, "046d_7438C0DF");

        // A single token: nothing to strip either side.
        let (model, _) = split_vendor_model_serial("Webcam");
        assert_eq!(model, "Webcam");
    }

    #[test]
    fn split_does_not_strip_short_non_serial_trailing_tokens() {
        // A trailing token under 4 chars (e.g. a model revision like "v2") isn't serial-shaped,
        // so it should be kept as part of the model.
        let (model, _) = split_vendor_model_serial("046d_Webcam_v2");
        assert_eq!(model, "Webcam_v2");
    }

    #[test]
    fn animal_suffix_is_deterministic_and_from_the_fixed_list() {
        let a = animal_suffix("some-serial-1234");
        let b = animal_suffix("some-serial-1234");
        assert_eq!(a, b);
        assert!(ANIMAL_NAMES.contains(&a));
    }

    #[test]
    fn different_seeds_usually_get_different_animals() {
        // Not a strict guarantee (it's a hash into a fixed-size list), but with two very
        // different serials it should hold in practice and catches a suffix function that
        // accidentally ignores its input.
        assert_ne!(animal_suffix("7438C0DF"), animal_suffix("1A2B3C4D"));
    }

    #[test]
    fn derive_name_strips_vendor_and_serial_and_appends_animal() {
        let stable_path = PathBuf::from(
            "/dev/v4l/by-id/usb-046d_HD_Pro_Webcam_C920_7438C0DF-video-index0",
        );
        let name = derive_name(Path::new("/dev/video0"), Some(&stable_path));
        let expected_animal = animal_suffix("046d_HD_Pro_Webcam_C920_7438C0DF");
        assert_eq!(name, format!("hd-pro-webcam-c920-{expected_animal}"));
    }

    #[test]
    fn derive_name_is_stable_across_repeated_calls() {
        // Same physical device info in -> same name out, every time (this is what makes the
        // name usable as a persistent RTSP path / config.yaml key across reboots and replugs).
        let stable_path = PathBuf::from(
            "/dev/v4l/by-id/usb-046d_HD_Pro_Webcam_C920_7438C0DF-video-index0",
        );
        let first = derive_name(Path::new("/dev/video0"), Some(&stable_path));
        let second = derive_name(Path::new("/dev/video3"), Some(&stable_path));
        assert_eq!(first, second);
    }
}
