//! `/sys/class/backlight` enumeration and brightness math.

use std::cmp::Ordering;
use std::path::Path;

/// One backlight device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklightDevice {
    pub name: String,
    pub subsystem: String,
    /// The requested value (`brightness`), i.e. what the user/system set.
    /// This is the base for relative changes.
    pub requested: u64,
    /// The effective value (`actual_brightness`), shown to the user.
    pub actual: u64,
    pub max: u64,
    pub powered: bool,
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Read a single device `name` under `root` (usually `/sys/class/backlight`).
///
/// `subsystem` is carried into the result so a cached device can be re-read
/// without a directory scan. Returns `None` when the device is gone or no
/// longer exposes `max_brightness`.
pub fn read_device(root: &Path, name: &str, subsystem: &str) -> Option<BacklightDevice> {
    let path = root.join(name);
    let max = read_u64(&path.join("max_brightness"))?;
    // Some drivers only expose `brightness` (the requested value); the rest
    // expose `actual_brightness` (the effective value).
    let attr = if name.starts_with("amdgpu_bl") || name == "apple-panel-bl" {
        "brightness"
    } else {
        "actual_brightness"
    };
    let actual = read_u64(&path.join(attr))
        .or_else(|| read_u64(&path.join("brightness")))
        .unwrap_or(0);
    let requested = read_u64(&path.join("brightness")).unwrap_or(actual);
    let powered = read_u64(&path.join("bl_power"))
        .map(|v| v == 0)
        .unwrap_or(true);
    Some(BacklightDevice {
        name: name.to_string(),
        subsystem: subsystem.to_string(),
        requested,
        actual,
        max,
        powered,
    })
}

/// Enumerate backlight devices under `root` (usually `/sys/class/backlight`).
pub fn enumerate(root: &Path) -> Vec<BacklightDevice> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(device) = read_device(root, &name, "backlight") {
            devices.push(device);
        }
    }
    devices
}

/// Brightness as a percentage.
pub fn percent(actual: u64, max: u64) -> u16 {
    if max == 0 {
        100
    } else {
        (actual as f64 * 100.0 / max as f64).round() as u16
    }
}

/// Perceptually-scaled brightness percentage, matching waybar's `percent_exp`.
pub fn percent_exp(actual: u64, max: u64) -> u16 {
    if max == 0 {
        100
    } else {
        let ratio = actual as f64 / max as f64;
        (ratio.powf(1.0 / std::f64::consts::E) * 100.0).round() as u16
    }
}

/// The absolute brightness to move to when the user scrolls by one step.
///
/// `step` is a percentage of the full range; it is converted to absolute device
/// units so that a step never rounds away to nothing on devices with a small
/// `max_brightness` (always at least one unit). `min_pct` is the lower clamp in
/// percent. Returns `None` when the value would not change or `max` is zero.
pub fn step_value(current: u64, max: u64, step: f64, min_pct: f64, increase: bool) -> Option<u64> {
    if max == 0 {
        return None;
    }
    let current = current.min(max);

    let unit = (step * max as f64 / 100.0).round();
    let delta = if unit.is_finite() && unit >= 1.0 {
        unit as u64
    } else {
        1
    };

    let min_abs = (min_pct.clamp(0.0, 100.0) * max as f64 / 100.0).round() as u64;
    let min_abs = min_abs.min(max);

    let target = if increase {
        current.saturating_add(delta).min(max)
    } else {
        if current <= min_abs {
            return None;
        }
        current.saturating_sub(delta).max(min_abs)
    };

    (target != current).then_some(target)
}

/// Choose the device to use: `preferred` by name, else the `backlight` device
/// with the largest `max_brightness`.
pub fn best_device<'a>(
    devices: &'a [BacklightDevice],
    preferred: &str,
) -> Option<&'a BacklightDevice> {
    if !preferred.is_empty()
        && let Some(device) = devices.iter().find(|d| d.name == preferred)
    {
        return Some(device);
    }
    devices.iter().max_by(
        |l, r| match (l.subsystem == "backlight", r.subsystem == "backlight") {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => l.max.cmp(&r.max),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_bounds() {
        assert_eq!(percent(0, 100), 0);
        assert_eq!(percent(50, 100), 50);
        assert_eq!(percent(100, 100), 100);
        assert_eq!(percent(0, 0), 100);
    }

    #[test]
    fn percent_exp_is_non_linear() {
        assert_eq!(percent_exp(0, 100), 0);
        assert_eq!(percent_exp(100, 100), 100);
        // 25% linear maps to a higher perceptual value
        assert!(percent_exp(25, 100) > 25);
    }

    #[test]
    fn prefers_named_then_largest_backlight() {
        let devices = vec![
            BacklightDevice {
                name: "intel_backlight".into(),
                subsystem: "backlight".into(),
                requested: 10,
                actual: 10,
                max: 100,
                powered: true,
            },
            BacklightDevice {
                name: "kbd_backlight".into(),
                subsystem: "leds".into(),
                requested: 1,
                actual: 1,
                max: 3,
                powered: true,
            },
        ];
        assert_eq!(
            best_device(&devices, "intel_backlight").unwrap().name,
            "intel_backlight"
        );
        assert_eq!(best_device(&devices, "").unwrap().name, "intel_backlight");
        assert_eq!(
            best_device(&devices, "kbd_backlight").unwrap().name,
            "kbd_backlight"
        );
    }

    #[test]
    fn step_never_rounds_away_on_small_ranges() {
        // max = 15: a 1% step is 0.15 units, but the value must still move.
        assert_eq!(step_value(5, 15, 1.0, 0.0, true), Some(6));
        assert_eq!(step_value(6, 15, 1.0, 0.0, true), Some(7));
        assert_eq!(step_value(6, 15, 1.0, 0.0, false), Some(5));
        // A sub-unit step still moves by one absolute unit.
        assert_eq!(step_value(0, 15, 0.5, 0.0, true), Some(1));
    }

    #[test]
    fn step_clamps_to_range() {
        assert_eq!(step_value(100, 100, 1.0, 0.0, true), None);
        assert_eq!(step_value(0, 100, 1.0, 0.0, false), None);
        assert_eq!(step_value(0, 0, 1.0, 0.0, true), None);
        // min-brightness stops the descent.
        assert_eq!(step_value(11, 100, 1.0, 10.0, false), Some(10));
        assert_eq!(step_value(10, 100, 1.0, 10.0, false), None);
        // A large step clamps at the top rather than overflowing.
        assert_eq!(step_value(0, 100, 1000.0, 0.0, true), Some(100));
    }

    #[test]
    fn reads_and_enumerates_devices_from_a_directory() {
        let root = std::env::temp_dir().join(format!("waybar-backlight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let intel = root.join("intel_backlight");
        let kbd = root.join("kbd_backlight");
        let broken = root.join("broken");
        std::fs::create_dir_all(&intel).unwrap();
        std::fs::create_dir_all(&kbd).unwrap();
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(intel.join("max_brightness"), "100\n").unwrap();
        std::fs::write(intel.join("brightness"), "40\n").unwrap();
        std::fs::write(intel.join("actual_brightness"), "42\n").unwrap();
        std::fs::write(intel.join("bl_power"), "0\n").unwrap();
        std::fs::write(kbd.join("max_brightness"), "3\n").unwrap();
        std::fs::write(kbd.join("brightness"), "1\n").unwrap();
        std::fs::write(kbd.join("actual_brightness"), "1\n").unwrap();

        let device = read_device(&root, "intel_backlight", "backlight").unwrap();
        assert_eq!(device.requested, 40);
        assert_eq!(device.actual, 42);
        assert_eq!(device.max, 100);
        assert!(device.powered);

        let mut names: Vec<_> = enumerate(&root).into_iter().map(|d| d.name).collect();
        names.sort();
        assert_eq!(names, ["intel_backlight", "kbd_backlight"]);

        // A directory without `max_brightness` is skipped, both by the scanner
        // and by a direct read.
        assert!(read_device(&root, "broken", "backlight").is_none());
        assert!(read_device(&root, "gone", "backlight").is_none());
        assert_eq!(enumerate(&root).len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }
}
