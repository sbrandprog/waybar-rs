//! `/proc/meminfo` parsing and memory statistics.

use std::collections::HashMap;

/// Parse `/proc/meminfo` into `name -> value` (values stay in kiB, as the file
/// reports them despite the `kB` label).
pub fn parse_meminfo(text: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some(colon) = line.find(':') else { continue };
        let name = line[..colon].trim().to_string();
        let value = line[colon + 1..]
            .split_whitespace()
            .next()
            .and_then(|x| x.parse::<u64>().ok());
        if let Some(value) = value {
            map.insert(name, value);
        }
    }
    map
}

/// Parse the `size` field of `/proc/spl/kstat/zfs/arcstats` (returns kiB).
pub fn parse_zfs_size(text: &str) -> u64 {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("size") {
            // name type data
            let _type = parts.next();
            if let Some(data) = parts.next().and_then(|x| x.parse::<u64>().ok()) {
                return data / 1024;
            }
        }
    }
    0
}

/// Computed memory statistics, all sizes in kiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryStats {
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub swap_avail: u64,
    pub percentage: u8,
    pub swap_percentage: u8,
}

impl MemoryStats {
    pub fn swap_on(&self) -> bool {
        self.swap_total > 0
    }
}

/// Compute statistics from parsed `meminfo` plus the ZFS ARC size in kiB.
///
/// Returns `None` when `MemTotal` is missing or zero.
pub fn compute(meminfo: &HashMap<String, u64>, zfs_size_kb: u64) -> Option<MemoryStats> {
    let total = *meminfo.get("MemTotal")?;
    if total == 0 {
        return None;
    }
    let swap_total = meminfo.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = meminfo.get("SwapFree").copied().unwrap_or(0);

    let avail = if let Some(available) = meminfo.get("MemAvailable") {
        available + zfs_size_kb
    } else {
        let get = |k: &str| meminfo.get(k).copied().unwrap_or(0);
        get("MemFree") + get("Buffers") + get("Cached") + get("SReclaimable") - get("Shmem")
            + zfs_size_kb
    };

    let used = total.saturating_sub(avail);
    let swap_used = swap_total.saturating_sub(swap_free);
    let percentage = (100 * used / total) as u8;
    let swap_percentage = (100 * swap_used).checked_div(swap_total).unwrap_or(0) as u8;

    Some(MemoryStats {
        total,
        used,
        avail,
        swap_total,
        swap_used,
        swap_avail: swap_free,
        percentage,
        swap_percentage,
    })
}

/// Divisor that converts a kiB count to the requested unit, matching waybar's
/// `kUnits` table. Unknown units fall back to `GiB`.
pub fn unit_divisor(unit: &str) -> f64 {
    match unit {
        "B" => 1.0 / 1024.0,
        "kB" => 1000.0 / 1024.0,
        "kiB" => 1.0,
        "MB" => 1_000_000.0 / 1024.0,
        "MiB" => 1024.0,
        "GB" => 1_000_000_000.0 / 1024.0,
        "GiB" => 1024.0 * 1024.0,
        "TB" => 1e12 / 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0,
        _ => 1024.0 * 1024.0,
    }
}

/// Convert a kiB count to `unit`, pre-rounded to two decimals as waybar does
/// so bare `{total}` placeholders print cleanly.
pub fn to_unit(kb: u64, divisor: f64) -> f64 {
    0.01 * ((kb as f64 / divisor * 100.0).round())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "\
MemTotal:       16384000 kB
MemFree:         1000000 kB
MemAvailable:    8000000 kB
Buffers:          200000 kB
Cached:          3000000 kB
SReclaimable:     100000 kB
Shmem:             50000 kB
SwapTotal:       2000000 kB
SwapFree:        1500000 kB
";

    #[test]
    fn parses_values() {
        let m = parse_meminfo(MEMINFO);
        assert_eq!(m["MemTotal"], 16_384_000);
        assert_eq!(m["SwapFree"], 1_500_000);
    }

    #[test]
    fn computes_with_available() {
        let m = parse_meminfo(MEMINFO);
        let s = compute(&m, 0).unwrap();
        assert_eq!(s.total, 16_384_000);
        assert_eq!(s.avail, 8_000_000);
        assert_eq!(s.used, 8_384_000);
        assert_eq!(s.percentage, (100 * 8_384_000 / 16_384_000) as u8);
        assert_eq!(s.swap_used, 500_000);
        assert_eq!(s.swap_percentage, 25);
        assert!(s.swap_on());
    }

    #[test]
    fn zfs_arc_is_added_to_available() {
        let m = parse_meminfo(MEMINFO);
        let with_zfs = compute(&m, 1_000_000).unwrap();
        assert_eq!(with_zfs.avail, 9_000_000);
    }

    #[test]
    fn falls_back_without_mem_available() {
        let m = parse_meminfo(
            "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 200 kB\n\
             SReclaimable: 30 kB\nShmem: 10 kB\n",
        );
        let s = compute(&m, 0).unwrap();
        assert_eq!(s.avail, 370);
        assert_eq!(s.percentage, 63);
    }

    #[test]
    fn parses_zfs_arcstats() {
        let text = "13 1 0x01 80 96 0 0 0\nsize 4 123456789\n";
        assert_eq!(parse_zfs_size(text), 123456789 / 1024);
    }

    #[test]
    fn unit_conversion_pre_rounds() {
        // 16 GiB in kiB -> 16.0 GiB
        assert_eq!(to_unit(16 * 1024 * 1024, unit_divisor("GiB")), 16.0);
        // 1.5 GiB -> 1.5
        assert_eq!(to_unit(1_572_864, unit_divisor("GiB")), 1.5);
    }
}
