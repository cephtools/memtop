//! One tracked quantity: current value, observed peak (with the wall time it was seen), rate of
//! change over the measured interval, and a growth streak. Memory is bounded: one value and one
//! timestamp per row, never a history.
//!
//! Observed peaks are the maximum seen since the tool started (or the last reset or restart); a
//! spike between two samples is invisible to them. Authoritative peaks, which the kernel records
//! itself (VmHWM, memory.peak), are kept separately in `AuthPeak` and are never derived here.

use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Track {
    /// Current value; None when the source is missing this sample.
    pub cur: Option<u64>,
    /// Observed peak and the wall time (ms since epoch) it was seen.
    #[serde(serialize_with = "ser_peak")]
    pub peak: Option<(u64, u64)>,
    /// Bytes per second over the last measured interval between two good samples.
    pub rate: Option<i64>,
    /// How many consecutive good samples the value has grown.
    pub growing: u32,
    /// Last good sample: (value, monotonic ms), for the rate.
    #[serde(skip)]
    prev: Option<(u64, u64)>,
}

fn ser_peak<S: serde::Serializer>(p: &Option<(u64, u64)>, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    match p {
        None => s.serialize_none(),
        Some((v, at)) => {
            let mut st = s.serialize_struct("Peak", 2)?;
            st.serialize_field("value", v)?;
            st.serialize_field("at_ms", at)?;
            st.end()
        }
    }
}

impl Track {
    /// Feed one sample. `good` is false for a degraded sample: it is shown but used for neither
    /// the rate nor the peak.
    pub fn update(&mut self, v: Option<u64>, mono_ms: u64, wall_ms: u64, good: bool) {
        self.cur = v;
        let Some(v) = v else {
            self.rate = None;
            self.growing = 0;
            return;
        };
        if !good {
            return;
        }
        if self.peak.map_or(true, |(p, _)| v > p) {
            self.peak = Some((v, wall_ms));
        }
        if let Some((pv, pt)) = self.prev {
            let dt = mono_ms.saturating_sub(pt);
            if dt > 0 {
                let d = v as i128 - pv as i128;
                self.rate =
                    Some((d * 1000 / dt as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64);
            }
            self.growing = if v > pv {
                self.growing.saturating_add(1)
            } else {
                0
            };
        }
        self.prev = Some((v, mono_ms));
    }

    /// Clear the observed peak; the current value is kept and becomes the peak at the next sample.
    pub fn reset_peak(&mut self) {
        self.peak = None;
    }
}

/// A peak the kernel keeps itself. The value is correct regardless of when the tool started; the
/// time is only when the tool first saw that value (None: it was already there at start).
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AuthPeak {
    pub value: Option<u64>,
    pub seen_ms: Option<u64>,
    #[serde(skip)]
    first: bool,
}

impl AuthPeak {
    pub fn update(&mut self, v: Option<u64>, wall_ms: u64) {
        let Some(v) = v else {
            return;
        };
        if !self.first {
            self.first = true;
            self.value = Some(v);
            self.seen_ms = None;
        } else if self.value.map_or(true, |p| v > p) {
            self.value = Some(v);
            self.seen_ms = Some(wall_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_never_falls_without_reset_or_restart() {
        let mut t = Track::default();
        let seq = [10u64, 50, 20, 49, 50, 5];
        for (i, v) in seq.iter().enumerate() {
            t.update(Some(*v), 1000 * i as u64, 7_000 + 1000 * i as u64, true);
            assert!(t.peak.unwrap().0 >= *v);
        }
        assert_eq!(
            t.peak,
            Some((50, 8_000)),
            "the first 50 keeps its timestamp"
        );
        t.update(Some(5), 9000, 16_000, true);
        assert_eq!(t.peak, Some((50, 8_000)));
        t.reset_peak();
        assert_eq!(t.peak, None);
        t.update(Some(7), 10_000, 17_000, true);
        assert_eq!(t.peak, Some((7, 17_000)));
        t = Track::default(); // what a restart does
        assert_eq!(t.peak, None);
    }

    #[test]
    fn rates_use_the_measured_interval() {
        let mut t = Track::default();
        t.update(Some(1000), 0, 0, true);
        assert_eq!(t.rate, None, "one sample has no rate");
        t.update(Some(4000), 3000, 3000, true);
        assert_eq!(t.rate, Some(1000));
        t.update(Some(3000), 3500, 3500, true);
        assert_eq!(t.rate, Some(-2000));
    }

    #[test]
    fn degraded_samples_feed_neither_peak_nor_rate() {
        let mut t = Track::default();
        t.update(Some(10), 0, 0, true);
        t.update(Some(999), 1000, 1000, false);
        assert_eq!(t.cur, Some(999));
        assert_eq!(t.peak, Some((10, 0)));
        assert_eq!(t.rate, None);
        t.update(Some(20), 2000, 2000, true);
        assert_eq!(t.peak, Some((20, 2000)));
        assert_eq!(
            t.rate,
            Some(5),
            "10 bytes over the 2 s between the two good samples"
        );
    }

    #[test]
    fn missing_values_leave_no_peak_and_no_rate() {
        let mut t = Track::default();
        t.update(None, 0, 0, true);
        assert_eq!(t.cur, None);
        assert_eq!(
            t.peak, None,
            "a peak with no samples behind it is blank, not zero"
        );
        t.update(Some(3), 1000, 1000, true);
        assert_eq!(t.rate, None);
        assert_eq!(t.peak, Some((3, 1000)));
    }

    #[test]
    fn growth_streak_counts_consecutive_rises() {
        let mut t = Track::default();
        for (i, v) in [1u64, 2, 3, 4, 3, 5].iter().enumerate() {
            t.update(Some(*v), 1000 * i as u64, 0, true);
        }
        assert_eq!(t.growing, 1);
        t.update(Some(6), 7000, 0, true);
        assert_eq!(t.growing, 2);
    }

    #[test]
    fn authoritative_peaks_keep_the_kernel_value() {
        let mut a = AuthPeak::default();
        a.update(Some(500), 1000);
        assert_eq!(
            (a.value, a.seen_ms),
            (Some(500), None),
            "already there at start: no time"
        );
        a.update(Some(400), 2000);
        assert_eq!(
            a.value,
            Some(500),
            "the kernel does not lower a peak, nor do we"
        );
        a.update(Some(600), 3000);
        assert_eq!((a.value, a.seen_ms), (Some(600), Some(3000)));
        a = AuthPeak::default(); // what a restart does
        assert_eq!(a.value, None);
    }
}
