//! Pure string/number formatting helpers for GUI readouts. No egui, no state.

use std::time::Duration;

/// Format a byte count compactly in SI units (kB = 1000 B, MB = 1000 kB, …)
/// for liveness readouts: channel-list rows and detail headers.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.3} {}", UNITS[u])
    }
}

/// Format a byte-per-second rate in the same SI units as [`human_bytes`], so a
/// readout showing a total beside a rate scales both the same way.
///
/// One decimal throughout, including zero: a stopped channel reads `0.0 B/s`
/// rather than dropping the segment, so the row keeps its shape at rest.
pub fn human_byte_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B/s", "kB/s", "MB/s", "GB/s", "TB/s"];
    let mut value = bytes_per_sec.max(0.0);
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Group a count with thousands separators.
///
/// For quantities with no SI unit to shrink them — sends, samples, skipped
/// cadence points — where the reader's question is usually "how many digits"
/// and an ungrouped run of them has to be counted by eye.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Format a configured send interval in milliseconds, always.
///
/// Intervals are entered in milliseconds and stored as `interval_ms`, so a
/// readout that rescales them to seconds makes the reader convert back to check
/// it against what they typed. Grouping carries the magnitude instead.
pub fn interval(interval: Duration) -> String {
    format!("{} ms", thousands(interval.as_millis() as u64))
}

/// Format a configured wait exactly, in the largest unit that holds it whole:
/// `5 min`, `30 s`, or `1500 ms`.
///
/// Waits are entered and shown as the value the operator chose, so unlike
/// `wiredata_cli::describe` nothing is truncated: a wait that is not a whole
/// number of seconds stays in milliseconds. The unit names match `describe`.
pub fn wait(wait: Duration) -> String {
    let millis = wait.as_millis();
    if millis != 0 && millis.is_multiple_of(60_000) {
        format!("{} min", millis / 60_000)
    } else if millis.is_multiple_of(1_000) {
        format!("{} s", millis / 1_000)
    } else {
        format!("{millis} ms")
    }
}

/// Format a percentage as a whole number, rounded **down**.
///
/// Flooring is the point: several of these sit next to a threshold (80% is low
/// margin, 100% is physically impossible), and rounding up would let a reading
/// claim a limit it has not reached. A non-zero value below one percent says so
/// rather than collapsing to `0%`.
pub fn percent(value: f64) -> String {
    if value > 0.0 && value < 1.0 {
        return "<1%".to_owned();
    }
    // Flooring alone would render 100.4% as "100%", which reads as *at* the
    // limit next to an alert that fired for exceeding it. Over the limit says
    // so; only an exact 100 reads as 100.
    if value > 100.0 {
        return ">100%".to_owned();
    }
    format!("{}%", value.floor() as i64)
}

/// Qualify a serial-port failure with whether that port is still enumerated.
///
/// The operating system reports an unusable port as though it were absent —
/// Windows lists a device as soon as it recognises the hardware, before any
/// driver has started, so a missing or refused driver surfaces as "the device
/// does not exist". That sends a reader looking for a port sitting right there
/// in the dropdown. Only the UI knows which ports are currently listed, so only
/// the UI can separate the two cases. A listed port can also be held by
/// another program: on Windows that and a missing device are the same error
/// kind, so the hint names both.
///
/// Lives here, beside the other pure string helpers, so both apps say it the
/// same way; it takes no egui and decides nothing.
pub fn serial_port_hint(port: &str, listed: bool) -> String {
    if listed {
        format!(
            "{port} is in the port list, so another program may have it open, or its driver \
             may not be running."
        )
    } else {
        format!("{port} is no longer in the port list, so the device may have been removed.")
    }
}

/// A hint for a serial port that failed to open with 5 or 6 data bits, and
/// `None` for any other number.
///
/// Many USB serial adapters, FTDI's among them, take only 7 or 8 data bits,
/// and the Windows driver refuses the rest at open in words that do not say
/// so ("The parameter is incorrect"). Those words are the system's, in the
/// system's language, so they cannot be matched; the hint goes by the setting.
/// Beside [`serial_port_hint`] so Linemon, a Listener channel and talker say
/// it the same way.
pub fn data_bits_hint(data_bits: u8) -> Option<&'static str> {
    matches!(data_bits, 5 | 6).then_some("Many USB serial adapters take only 7 or 8 data bits.")
}

/// Format a duration compactly for timing telemetry readouts.
pub fn compact_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 1_000 {
        return format!("{nanos} ns");
    }
    if nanos < 1_000_000 {
        return scaled_duration(nanos as f64 / 1_000.0, "µs");
    }
    if nanos < 1_000_000_000 {
        // Past a couple of milliseconds the fraction is finer than the wake
        // path being measured: Windows' default scheduler tick is 15.6 ms, and
        // 1 ms with the resolution request held. Printing hundredths there
        // shows precision the measurement does not have.
        let millis = nanos as f64 / 1_000_000.0;
        return if millis > 2.0 {
            format!("{millis:.0} ms")
        } else {
            format!("{millis:.2} ms")
        };
    }
    scaled_duration(duration.as_secs_f64(), "s")
}

fn scaled_duration(value: f64, unit: &str) -> String {
    if value < 10.0 {
        format!("{value:.2} {unit}")
    } else if value < 100.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_5_or_6_data_bits_get_the_adapter_hint() {
        for bits in [5, 6] {
            assert_eq!(
                data_bits_hint(bits),
                Some("Many USB serial adapters take only 7 or 8 data bits.")
            );
        }
        for bits in [7, 8] {
            assert_eq!(data_bits_hint(bits), None);
        }
    }

    #[test]
    fn counts_group_by_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(4_300), "4,300");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn intervals_stay_in_the_unit_they_were_configured_in() {
        assert_eq!(interval(Duration::from_millis(50)), "50 ms");
        assert_eq!(interval(Duration::from_secs(1)), "1,000 ms");
        assert_eq!(interval(Duration::from_secs(15)), "15,000 ms");
    }

    #[test]
    fn waits_keep_their_exact_value_in_the_largest_whole_unit() {
        assert_eq!(wait(Duration::from_millis(250)), "250 ms");
        assert_eq!(wait(Duration::from_millis(1_500)), "1500 ms");
        assert_eq!(wait(Duration::from_secs(1)), "1 s");
        assert_eq!(wait(Duration::from_secs(30)), "30 s");
        assert_eq!(wait(Duration::from_secs(90)), "90 s");
        assert_eq!(wait(Duration::from_secs(60)), "1 min");
        assert_eq!(wait(Duration::from_secs(600)), "10 min");
        assert_eq!(wait(Duration::ZERO), "0 s");
    }

    #[test]
    fn percentages_are_whole_numbers_that_never_round_up_into_a_threshold() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(2.9), "2%");
        assert_eq!(percent(87.3), "87%");
        // The reason for flooring: 99.6% is not at capacity, and must not say
        // it is. Only an actual 100% reads as 100%.
        assert_eq!(percent(99.6), "99%");
        assert_eq!(percent(100.0), "100%");
        // ...and the mirror of it: 100.4% is over the limit, and must not read
        // as sitting on it beside an alert that fired for exceeding it.
        assert_eq!(percent(100.4), ">100%");
        // A small non-zero share is not nothing.
        assert_eq!(percent(0.3), "<1%");
    }

    #[test]
    fn milliseconds_past_two_drop_a_precision_the_wake_path_lacks() {
        assert_eq!(compact_duration(Duration::from_micros(900)), "900 µs");
        assert_eq!(compact_duration(Duration::from_millis(1)), "1.00 ms");
        assert_eq!(compact_duration(Duration::from_micros(1_500)), "1.50 ms");
        assert_eq!(compact_duration(Duration::from_millis(2)), "2.00 ms");
        assert_eq!(compact_duration(Duration::from_micros(3_150)), "3 ms");
        assert_eq!(compact_duration(Duration::from_millis(40)), "40 ms");
        assert_eq!(compact_duration(Duration::from_millis(128)), "128 ms");
    }

    #[test]
    fn bytes_below_1k_are_plain() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
    }

    #[test]
    fn si_units_scale_by_1000() {
        assert_eq!(human_bytes(1000), "1.000 kB");
        assert_eq!(human_bytes(1_500_000), "1.500 MB");
        assert_eq!(human_bytes(2_000_000_000), "2.000 GB");
    }

    #[test]
    fn byte_rates_scale_like_totals_and_keep_a_decimal_at_rest() {
        assert_eq!(human_byte_rate(0.0), "0.0 B/s");
        assert_eq!(human_byte_rate(999.0), "999.0 B/s");
        assert_eq!(human_byte_rate(1_000.0), "1.0 kB/s");
        assert_eq!(human_byte_rate(12_400.0), "12.4 kB/s");
        assert_eq!(human_byte_rate(1_500_000.0), "1.5 MB/s");
        // A negative rate is not meaningful; clamp rather than print "-0.0".
        assert_eq!(human_byte_rate(-5.0), "0.0 B/s");
    }

    #[test]
    fn durations_choose_compact_units_and_precision() {
        assert_eq!(compact_duration(Duration::ZERO), "0 ns");
        assert_eq!(compact_duration(Duration::from_nanos(999)), "999 ns");
        assert_eq!(compact_duration(Duration::from_micros(1)), "1.00 µs");
        assert_eq!(compact_duration(Duration::from_micros(12)), "12.0 µs");
        assert_eq!(compact_duration(Duration::from_micros(999)), "999 µs");
        assert_eq!(compact_duration(Duration::from_micros(1_250)), "1.25 ms");
        assert_eq!(compact_duration(Duration::from_millis(15)), "15 ms");
        assert_eq!(compact_duration(Duration::from_millis(999)), "999 ms");
        assert_eq!(compact_duration(Duration::from_millis(1_250)), "1.25 s");
    }
}
