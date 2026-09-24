//! `/proc/stat` and `/proc/loadavg` parsing.

/// Aggregate or per-CPU jiffie counters as `(idle, total)`, where `idle` is
/// `idle + iowait` and `total` is the sum of all fields.
pub type CpuTimes = (u64, u64);

/// Parse `/proc/stat`.
///
/// Index 0 is the aggregate `cpu` line; subsequent entries are `cpuN`. Gaps
/// (offline CPUs absent from the file) are filled with `(0, 0)` so indices
/// stay aligned, mirroring waybar.
pub fn parse_cpu_times(text: &str) -> Vec<CpuTimes> {
    let mut out = Vec::new();
    let mut next_core: Option<u32> = None;

    for line in text.lines() {
        if !line.starts_with("cpu") {
            break;
        }
        let rest = &line[3..];
        let (index, numbers) = match rest.strip_prefix(' ') {
            // Aggregate line: "cpu  <numbers...>"
            Some(numbers) => (None, numbers),
            // Per-CPU line: "cpuN <numbers...>"
            None => {
                let mut parts = rest.splitn(2, ' ');
                let n = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
                (Some(n), parts.next().unwrap_or(""))
            }
        };

        let times: Vec<u64> = numbers
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        let (idle, total) = if times.len() >= 5 {
            (times[3] + times[4], times.iter().sum())
        } else {
            (0, 0)
        };

        match index {
            None => {
                out.push((idle, total));
                next_core = Some(0);
            }
            Some(n) => {
                if let Some(mut current) = next_core {
                    while n > current {
                        out.push((0, 0));
                        current += 1;
                    }
                }
                out.push((idle, total));
                next_core = Some(n + 1);
            }
        }
    }

    out
}

/// Busy percentage between two samples of the same CPU.
pub fn usage_percent(prev: CpuTimes, curr: CpuTimes) -> u16 {
    let delta_idle = curr.0.saturating_sub(prev.0) as f64;
    let delta_total = curr.1.saturating_sub(prev.1) as f64;
    if delta_total > 0.0 {
        (100.0 * (1.0 - delta_idle / delta_total)) as u16
    } else {
        0
    }
}

/// Parse the three load averages from `/proc/loadavg`.
pub fn parse_load(text: &str) -> (f64, f64, f64) {
    let mut it = text.split_whitespace();
    let one = it.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let five = it.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let fifteen = it.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    (one, five, fifteen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
cpu  100 0 50 800 20 0 10 0 0 0
cpu0 50 0 25 400 10 0 5 0 0 0
cpu1 50 0 25 400 10 0 5 0 0 0
intr 12345
";

    #[test]
    fn parses_aggregate_and_cores() {
        let times = parse_cpu_times(SAMPLE);
        assert_eq!(times.len(), 3);
        // aggregate: idle = 800+20 = 820, total = 100+0+50+800+20+0+10 = 980
        assert_eq!(times[0], (820, 980));
        assert_eq!(times[1], (410, 490));
        assert_eq!(times[2], (410, 490));
    }

    #[test]
    fn fills_missing_offline_cores() {
        let text =
            "cpu  10 0 0 90 0 0 0 0 0 0\ncpu0 5 0 0 45 0 0 0 0 0 0\ncpu2 5 0 0 45 0 0 0 0 0 0\n";
        let times = parse_cpu_times(text);
        // aggregate + cpu0 + (cpu1 offline) + cpu2
        assert_eq!(times.len(), 4);
        assert_eq!(times[2], (0, 0));
    }

    #[test]
    fn usage_between_samples() {
        // idle grew by 20, total by 100 -> 80% busy
        assert_eq!(usage_percent((0, 0), (20, 100)), 80);
        assert_eq!(usage_percent((10, 100), (10, 100)), 0);
    }

    #[test]
    fn parses_loadavg() {
        assert_eq!(parse_load("0.52 0.58 0.59 1/234 5678"), (0.52, 0.58, 0.59));
    }
}
