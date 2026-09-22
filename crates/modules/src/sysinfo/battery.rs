//! `/sys/class/power_supply` parsing and battery aggregation.

use std::path::Path;

/// Raw attributes of one power supply, as read from sysfs.
#[derive(Debug, Default, Clone)]
pub struct PowerSupply {
    pub kind: Option<String>,
    pub scope_device: bool,
    pub capacity: Option<u64>,
    pub status: Option<String>,
    pub energy_now: Option<u64>,
    pub energy_full: Option<u64>,
    pub energy_full_design: Option<u64>,
    pub charge_now: Option<u64>,
    pub charge_full: Option<u64>,
    pub charge_full_design: Option<u64>,
    pub voltage_now: Option<u64>,
    pub power_now: Option<u64>,
    pub current_now: Option<u64>,
    pub time_to_empty_now: Option<u64>,
    pub time_to_full_now: Option<u64>,
    pub cycle_count: Option<u64>,
}

fn read_u64(dir: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(dir.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn read_str(dir: &Path, name: &str) -> Option<String> {
    let value = std::fs::read_to_string(dir.join(name)).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Read one power-supply directory.
pub fn read_power_supply(dir: &Path) -> PowerSupply {
    PowerSupply {
        kind: read_str(dir, "type"),
        scope_device: read_str(dir, "scope").as_deref() == Some("device"),
        capacity: read_u64(dir, "capacity"),
        status: read_str(dir, "status"),
        energy_now: read_u64(dir, "energy_now"),
        energy_full: read_u64(dir, "energy_full"),
        energy_full_design: read_u64(dir, "energy_full_design"),
        charge_now: read_u64(dir, "charge_now"),
        charge_full: read_u64(dir, "charge_full"),
        charge_full_design: read_u64(dir, "charge_full_design"),
        voltage_now: read_u64(dir, "voltage_now").or_else(|| read_u64(dir, "voltage_avg")),
        power_now: read_u64(dir, "power_now"),
        current_now: read_u64(dir, "current_now").or_else(|| read_u64(dir, "current_avg")),
        time_to_empty_now: read_u64(dir, "time_to_empty_now"),
        time_to_full_now: read_u64(dir, "time_to_full_now"),
        cycle_count: read_u64(dir, "cycle_count"),
    }
}

impl PowerSupply {
    pub fn is_battery(&self) -> bool {
        self.kind.as_deref() == Some("Battery")
    }

    fn energy_now_uw(&self) -> Option<u64> {
        self.energy_now
            .or_else(|| Some(self.charge_now? * self.voltage_now? / 1_000_000))
    }

    fn energy_full_uw(&self) -> Option<u64> {
        self.energy_full
            .or_else(|| Some(self.charge_full? * self.voltage_now? / 1_000_000))
    }

    fn energy_full_design_uw(&self) -> Option<u64> {
        self.energy_full_design
            .or_else(|| Some(self.charge_full_design? * self.voltage_now? / 1_000_000))
    }

    fn power_now_uw(&self) -> Option<u64> {
        self.power_now
            .or_else(|| Some(self.voltage_now? * self.current_now? / 1_000_000))
    }
}

/// Aggregated battery state shown by the module.
#[derive(Debug, Clone)]
pub struct BatteryInfo {
    pub capacity: u8,
    /// Raw status as reported (`Discharging`, `Charging`, `Full`, ...).
    pub status: String,
    /// Positive when discharging, negative when charging, 0 when unknown.
    pub time_remaining_hours: f32,
    pub power_watts: f32,
    pub cycles: u16,
    pub health: f32,
}

/// True when `a` is a "worse" status than `b` (Unknown > Full > Not charging >
/// Discharging > Charging), mirroring waybar.
pub fn status_gt(a: &str, b: &str) -> bool {
    if a == b {
        false
    } else if a == "Unknown" {
        true
    } else if a == "Full" {
        b != "Unknown"
    } else if a == "Not charging" {
        b != "Unknown" && b != "Full"
    } else if a == "Discharging" {
        b != "Unknown" && b != "Full" && b != "Not charging"
    } else {
        false
    }
}

/// Combine the batteries (and adapter state) into a single reading.
pub fn aggregate(batteries: &[PowerSupply], adapter_online: bool) -> BatteryInfo {
    let mut capacity_sum = 0u64;
    let mut capacity_count = 0u64;
    let mut energy_now = 0u64;
    let mut energy_now_present = false;
    let mut energy_full = 0u64;
    let mut energy_full_present = false;
    let mut power = 0u64;
    let mut power_present = false;
    let mut status = "Unknown".to_string();
    let mut cycles = 0u16;
    let mut health = 0.0f32;
    let mut time_to_empty = 0u64;
    let mut time_to_empty_present = false;
    let mut time_to_full = 0u64;
    let mut time_to_full_present = false;

    for battery in batteries {
        if let Some(capacity) = battery.capacity {
            capacity_sum += capacity;
            capacity_count += 1;
        }
        let now = battery.energy_now_uw();
        let full = battery.energy_full_uw();
        let design = battery.energy_full_design_uw();
        if let Some(v) = now {
            energy_now += v;
            energy_now_present = true;
        }
        if let Some(v) = full {
            energy_full += v;
            energy_full_present = true;
        }
        if let Some(v) = battery.power_now_uw() {
            power += v;
            power_present = true;
        }
        if let Some(s) = &battery.status
            && status_gt(&status, s)
        {
            status = s.clone();
        }
        if let Some(c) = battery.cycle_count {
            cycles = cycles.max(c.min(u16::MAX as u64) as u16);
        }
        if let (Some(full), Some(design)) = (full, design)
            && design > 0
        {
            let percent = 100.0 * full as f32 / design as f32;
            if health == 0.0 || percent < health {
                health = percent;
            }
        }
        if let Some(v) = battery.time_to_empty_now {
            time_to_empty = v;
            time_to_empty_present = true;
        }
        if let Some(v) = battery.time_to_full_now {
            time_to_full = v;
            time_to_full_present = true;
        }
    }

    if adapter_online && (status == "Discharging" || status == "Not charging") {
        status = "Plugged".to_string();
    }

    let capacity = if energy_full_present && energy_now_present && energy_full > 0 {
        ((energy_now as f64 * 100.0 / energy_full as f64).round() as u64).min(100) as u8
    } else if capacity_count > 0 {
        capacity_sum.checked_div(capacity_count).unwrap_or(0) as u8
    } else {
        0
    };

    if capacity == 100 && status == "Charging" {
        status = "Full".to_string();
    }

    let time_remaining_hours = if status == "Discharging" {
        if time_to_empty_present && time_to_empty != 0 {
            time_to_empty as f32 / 3600.0
        } else if power_present && power != 0 && energy_now_present {
            energy_now as f32 / power as f32
        } else {
            0.0
        }
    } else if status == "Charging" {
        let remaining = if time_to_full_present && time_to_full != 0 {
            -(time_to_full as f32 / 3600.0)
        } else if power_present && power != 0 && energy_full_present && energy_now_present {
            -((energy_full - energy_now) as f32 / power as f32)
        } else {
            0.0
        };
        remaining.min(0.0)
    } else {
        0.0
    };

    BatteryInfo {
        capacity,
        status,
        time_remaining_hours,
        power_watts: power as f32 / 1e6,
        cycles,
        health,
    }
}

/// Normalise a status for use as a CSS class / config key suffix:
/// lower-case with spaces replaced by dashes.
pub fn normalize_status(status: &str) -> String {
    status.to_lowercase().replace(' ', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(capacity: u64, status: &str, now: u64, full: u64, design: u64) -> PowerSupply {
        PowerSupply {
            kind: Some("Battery".to_string()),
            capacity: Some(capacity),
            status: Some(status.to_string()),
            energy_now: Some(now),
            energy_full: Some(full),
            energy_full_design: Some(design),
            ..Default::default()
        }
    }

    #[test]
    fn computes_capacity_from_energy() {
        let b = battery(80, "Discharging", 40_000_000, 50_000_000, 60_000_000);
        let info = aggregate(&[b], false);
        assert_eq!(info.capacity, 80);
        assert_eq!(info.status, "Discharging");
        assert_eq!(info.health, 100.0 * 50.0 / 60.0);
    }

    #[test]
    fn falls_back_to_capacity_average() {
        let mut b = battery(0, "Discharging", 0, 0, 0);
        b.energy_now = None;
        b.energy_full = None;
        b.energy_full_design = None;
        b.capacity = Some(60);
        let info = aggregate(&[b], false);
        assert_eq!(info.capacity, 60);
    }

    #[test]
    fn adapter_marks_plugged() {
        let b = battery(90, "Not charging", 45, 50, 60);
        let info = aggregate(&[b], true);
        assert_eq!(info.status, "Plugged");
    }

    #[test]
    fn charging_at_100_becomes_full() {
        let b = battery(100, "Charging", 50, 50, 60);
        let info = aggregate(&[b], true);
        assert_eq!(info.status, "Full");
    }

    #[test]
    fn discharging_time_from_power() {
        // 40 Wh now, 10 W draw -> 4 h
        let b = PowerSupply {
            kind: Some("Battery".to_string()),
            capacity: Some(80),
            status: Some("Discharging".to_string()),
            energy_now: Some(40_000_000),
            energy_full: Some(50_000_000),
            power_now: Some(10_000_000),
            ..Default::default()
        };
        let info = aggregate(&[b], false);
        assert_eq!(info.power_watts, 10.0);
        assert!((info.time_remaining_hours - 4.0).abs() < 0.001);
    }

    #[test]
    fn worst_status_wins() {
        assert!(status_gt("Unknown", "Full"));
        assert!(status_gt("Full", "Not charging"));
        assert!(status_gt("Not charging", "Discharging"));
        assert!(status_gt("Discharging", "Charging"));
        assert!(!status_gt("Charging", "Discharging"));
    }

    #[test]
    fn normalizes_status() {
        assert_eq!(normalize_status("Not charging"), "not-charging");
        assert_eq!(normalize_status("Discharging"), "discharging");
    }
}
