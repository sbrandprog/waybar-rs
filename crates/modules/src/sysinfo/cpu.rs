//! `/proc/stat` and `/proc/loadavg` parsing.

/// Aggregate or per-CPU jiffie counters as `(idle, total)`, where `idle` is
/// `idle + iowait` and `total` is the sum of every field except `guest` and
/// `guest_nice` (which the kernel already accounts for in `user`/`nice`).
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
            let idle = times.get(3).copied().unwrap_or(0) + times.get(4).copied().unwrap_or(0);
            // `guest` (index 8) and `guest_nice` (9) are already included in
            // `user`/`nice` by the kernel, so adding them again would
            // double-count that time and inflate the reported usage.
            let total: u64 = times
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != 8 && *i != 9)
                .map(|(_, value)| value)
                .sum();
            (idle, total)
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
        (100.0 * (1.0 - delta_idle / delta_total))
            .round()
            .clamp(0.0, 100.0) as u16
    } else {
        0
    }
}

/// Per-CPU usages and tooltip lines from two samples.
///
/// `usages[0]` is the aggregate `cpu` line, `usages[i]` is core `i - 1`, and
/// the returned strings are parallel to the usages. A core missing from the
/// current sample (`total == 0`, an offline CPU filled in by
/// [`parse_cpu_times`]) is reported as offline rather than as 0% busy.
pub fn cpu_usages(previous: &[CpuTimes], current: &[CpuTimes]) -> (Vec<u16>, Vec<String>) {
    let mut usages = Vec::new();
    let mut lines = Vec::new();

    if previous.len() == current.len() && !current.is_empty() {
        for (i, (&prev, &curr)) in previous.iter().zip(current).enumerate() {
            if i > 0 && curr.1 == 0 {
                lines.push(format!("Core{}: offline", i - 1));
                usages.push(0);
                continue;
            }
            let usage = usage_percent(prev, curr);
            lines.push(if i == 0 {
                format!("Total: {usage}%")
            } else {
                format!("Core{}: {usage}%", i - 1)
            });
            usages.push(usage);
        }
    } else if !current.is_empty() && !previous.is_empty() {
        let usage = usage_percent(previous[0], current[0]);
        lines.push(format!("Total: {usage}%"));
        lines.push("Cores: (pending)".to_string());
        usages.push(usage);
    }

    (usages, lines)
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
    fn usage_rounds_to_nearest() {
        // idle grew by 1 of 10 -> 90%, by 1 of 3 -> 67% (not 66)
        assert_eq!(usage_percent((0, 0), (1, 10)), 90);
        assert_eq!(usage_percent((0, 0), (1, 3)), 67);
    }

    #[test]
    fn guest_time_is_not_double_counted() {
        // `guest` is already part of `user`; `guest_nice` part of `nice`.
        let text = "cpu  100 10 50 800 20 0 10 0 40 5\n";
        let times = parse_cpu_times(text);
        // idle = 800+20 = 820, total = user+nice+system+idle+iowait+irq+softirq+steal
        assert_eq!(times[0], (820, 990));
    }

    #[test]
    fn usages_first_sample_is_cumulative_average() {
        // A zero seed yields the average since boot and never marks online
        // cores as offline just because the previous sample was zero.
        let current =
            parse_cpu_times("cpu  500 0 100 350 50 0 0 0 0 0\ncpu0 200 0 50 240 10 0 0 0 0 0\n");
        let previous = vec![(0, 0); current.len()];
        let (usages, lines) = cpu_usages(&previous, &current);
        assert_eq!(usages.len(), 2);
        assert_eq!(lines[0], "Total: 60%"); // 1 - 400/1000
        assert_eq!(lines[1], "Core0: 50%"); // 1 - 250/500
    }

    #[test]
    fn offline_core_is_reported() {
        let previous = parse_cpu_times(
            "cpu  30 0 0 270 0 0 0 0 0 0\n\
             cpu0 10 0 0 90 0 0 0 0 0 0\n\
             cpu1 10 0 0 90 0 0 0 0 0 0\n\
             cpu2 10 0 0 90 0 0 0 0 0 0\n",
        );
        // cpu1 goes offline: it is absent and filled with a (0, 0) placeholder.
        let current = parse_cpu_times(
            "cpu  32 0 0 288 0 0 0 0 0 0\n\
             cpu0 11 0 0 99 0 0 0 0 0 0\n\
             cpu2 11 0 0 99 0 0 0 0 0 0\n",
        );
        let (_, lines) = cpu_usages(&previous, &current);
        assert!(lines.iter().any(|l| l == "Core1: offline"));
    }

    #[test]
    fn changed_core_count_is_pending() {
        let previous = parse_cpu_times("cpu  10 0 0 90 0 0 0 0 0 0\ncpu0 5 0 0 45 0 0 0 0 0 0\n");
        let current = parse_cpu_times(
            "cpu  12 0 0 100 0 0 0 0 0 0\ncpu0 6 0 0 50 0 0 0 0 0 0\ncpu1 6 0 0 50 0 0 0 0 0 0\n",
        );
        let (usages, lines) = cpu_usages(&previous, &current);
        assert_eq!(usages.len(), 1);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "Cores: (pending)");
    }

    #[test]
    fn parses_loadavg() {
        assert_eq!(parse_load("0.52 0.58 0.59 1/234 5678"), (0.52, 0.58, 0.59));
    }
}
