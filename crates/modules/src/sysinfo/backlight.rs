//! `/sys/class/backlight` enumeration and brightness math.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// One backlight device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklightDevice {
    pub name: String,
    pub subsystem: String,
    pub actual: u64,
    pub max: u64,
    pub powered: bool,
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Enumerate backlight devices under `root` (usually `/sys/class/backlight`).
pub fn enumerate(root: &Path) -> Vec<BacklightDevice> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let Some(max) = read_u64(&path.join("max_brightness")) else {
            continue;
        };
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
        let powered = read_u64(&path.join("bl_power")).map(|v| v == 0).unwrap_or(true);
        devices.push(BacklightDevice {
            name,
            subsystem: "backlight".to_string(),
            actual,
            max,
            powered,
        });
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

/// The path to write brightness to.
pub fn brightness_path(root: &Path, name: &str) -> PathBuf {
    root.join(name).join("brightness")
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
    devices.iter().max_by(|l, r| {
        match (
            l.subsystem == "backlight",
            r.subsystem == "backlight",
        ) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => l.max.cmp(&r.max),
        }
    })
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
                actual: 10,
                max: 100,
                powered: true,
            },
            BacklightDevice {
                name: "kbd_backlight".into(),
                subsystem: "leds".into(),
                actual: 1,
                max: 3,
                powered: true,
            },
        ];
        assert_eq!(best_device(&devices, "intel_backlight").unwrap().name, "intel_backlight");
        assert_eq!(best_device(&devices, "").unwrap().name, "intel_backlight");
        assert_eq!(best_device(&devices, "kbd_backlight").unwrap().name, "kbd_backlight");
    }
}
