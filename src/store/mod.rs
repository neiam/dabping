//! The RRD replacement: file-per-target round-robin storage with
//! multi-resolution consolidation. See series.rs for the on-disk format.

mod series;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::emit::Emitter;
use crate::probe::RoundResult;
use series::TargetSeries;

/// Consolidation function, RRD-style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cf {
    Average,
    Min,
    Max,
}

impl Cf {
    pub fn as_u32(self) -> u32 {
        match self {
            Cf::Average => 0,
            Cf::Min => 1,
            Cf::Max => 2,
        }
    }
}

impl std::str::FromStr for Cf {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Cf> {
        match s.to_ascii_lowercase().as_str() {
            "average" | "avg" => Ok(Cf::Average),
            "min" => Ok(Cf::Min),
            "max" => Ok(Cf::Max),
            other => bail!("unknown consolidation function {other:?} (average|min|max)"),
        }
    }
}

/// One round-robin archive definition — SmokePing/RRD's RRA line.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RraSpec {
    pub cf: Cf,
    /// Primary data points consolidated into one row.
    pub steps: u32,
    pub rows: u64,
    /// Max fraction of a window that may be unknown before the row is too.
    #[serde(default = "default_xff")]
    pub xff: f64,
}

fn default_xff() -> f64 {
    0.5
}

/// SmokePing's default consolidation table (assumes step 300):
/// raw for 3.5 days, hourly for 180 days, half-daily for a year.
pub fn default_rras() -> Vec<RraSpec> {
    let rra = |cf, steps, rows| RraSpec { cf, steps, rows, xff: 0.5 };
    vec![
        rra(Cf::Average, 1, 1008),
        rra(Cf::Average, 12, 4320),
        rra(Cf::Min, 12, 4320),
        rra(Cf::Max, 12, 4320),
        rra(Cf::Average, 144, 720),
        rra(Cf::Min, 144, 720),
        rra(Cf::Max, 144, 720),
    ]
}

#[derive(Debug, Serialize)]
pub struct Point {
    pub ts: i64,
    pub loss: f64,
    pub median: f64,
    /// Sorted per-round RTTs (the smoke). NaN-padded.
    pub pings: Vec<f64>,
}

impl Point {
    pub fn smoke_min(&self) -> f64 {
        self.pings.iter().copied().find(|v| !v.is_nan()).unwrap_or(f64::NAN)
    }
    pub fn smoke_max(&self) -> f64 {
        self.pings.iter().rev().copied().find(|v| !v.is_nan()).unwrap_or(f64::NAN)
    }
}

#[derive(Debug, Serialize)]
pub struct Fetched {
    /// Seconds per returned point.
    pub period: u64,
    pub points: Vec<Point>,
}

pub struct Store {
    dir: PathBuf,
    step: u64,
    pings: u32,
    specs: Vec<RraSpec>,
    series: Mutex<HashMap<String, Arc<Mutex<TargetSeries>>>>,
}

impl Store {
    pub fn open(dir: &std::path::Path, step: u64, pings: u32, specs: &[RraSpec]) -> Result<Store> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create data dir {}", dir.display()))?;
        Ok(Store {
            dir: dir.to_path_buf(),
            step,
            pings,
            specs: specs.to_vec(),
            series: Mutex::new(HashMap::new()),
        })
    }

    pub fn record(&self, r: &RoundResult) -> Result<()> {
        let ts = r
            .started
            .duration_since(UNIX_EPOCH)
            .context("round started before the epoch?")?
            .as_secs() as i64;

        let n = self.pings as usize;
        let mut fields = vec![f64::NAN; n + 2];
        fields[0] = r.loss_pct();
        fields[1] = r.median().map_or(f64::NAN, |d| d.as_secs_f64());
        for (i, d) in r.sorted_rtts().into_iter().take(n).enumerate() {
            fields[2 + i] = d.as_secs_f64();
        }

        let series = self.series(&r.target, true)?;
        let mut s = series.lock().expect("series lock poisoned");
        s.record(ts, &fields)
    }

    pub fn fetch(&self, target: &str, cf: Cf, from: i64, to: i64, max_points: u64) -> Result<Fetched> {
        let series = self.series(target, false)?;
        let s = series.lock().expect("series lock poisoned");
        let (period, raw) = s.fetch(cf, from, to, max_points)?;
        let points = raw
            .into_iter()
            .map(|(ts, mut vals)| {
                let pings = vals.split_off(2);
                Point { ts, loss: vals[0], median: vals[1], pings }
            })
            .collect();
        Ok(Fetched { period, points })
    }

    fn series(&self, target: &str, create: bool) -> Result<Arc<Mutex<TargetSeries>>> {
        let mut open = self.series.lock().expect("series map lock poisoned");
        if let Some(s) = open.get(target) {
            return Ok(s.clone());
        }
        let path = self.target_file(target)?;
        if !create && !path.exists() {
            bail!("no data recorded for target {target:?}");
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = Arc::new(Mutex::new(TargetSeries::open_or_create(
            &path, self.step, self.pings, &self.specs,
        )?));
        open.insert(target.to_string(), s.clone());
        Ok(s)
    }

    fn target_file(&self, target: &str) -> Result<PathBuf> {
        validate_target_path(target)?;
        let mut p = self.dir.clone();
        for comp in target.split('/') {
            p.push(comp);
        }
        p.set_extension("dab");
        Ok(p)
    }
}

/// Target paths become file paths, so keep them boring.
pub fn validate_target_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("empty target path");
    }
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            bail!("invalid target path {path:?}");
        }
        // '@' appears only in agent series keys ("path@agent"); config
        // flattening rejects it in configured target names
        if !comp.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@')) {
            bail!(
                "invalid target path {path:?}: name components may only contain \
                 letters, digits, '_', '-' and '.'"
            );
        }
    }
    Ok(())
}

/// Emitter wiring: every completed round lands in the store.
pub struct StoreEmitter(pub Arc<Store>);

impl Emitter for StoreEmitter {
    fn emit(&self, r: &RoundResult) {
        if let Err(e) = self.0.record(r) {
            tracing::error!(path = %r.target, error = %e, "failed to store round");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const MS: f64 = 1e-3;

    fn round(ts: i64, rtts_ms: &[f64], sent: u32) -> RoundResult {
        RoundResult {
            target: "t/a".into(),
            host: "192.0.2.1".into(),
            addr: "192.0.2.1".parse().unwrap(),
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(ts as u64),
            sent,
            rtts: rtts_ms.iter().map(|ms| Duration::from_secs_f64(ms * MS)).collect(),
        }
    }

    fn rra(cf: Cf, steps: u32, rows: u64) -> RraSpec {
        RraSpec { cf, steps, rows, xff: 0.5 }
    }

    fn store(dir: &std::path::Path, specs: &[RraSpec]) -> Store {
        Store::open(dir, 10, 3, specs).unwrap()
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn raw_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path(), &[rra(Cf::Average, 1, 64)]);
        st.record(&round(1000, &[10.0, 20.0, 30.0], 3)).unwrap();
        st.record(&round(1010, &[5.0, 15.0], 3)).unwrap();

        let f = st.fetch("t/a", Cf::Average, 995, 1015, 0).unwrap();
        assert_eq!(f.period, 10);
        let ts: Vec<i64> = f.points.iter().map(|p| p.ts).collect();
        assert_eq!(ts, vec![990, 1000, 1010]);

        assert!(f.points[0].median.is_nan()); // never recorded
        approx(f.points[1].loss, 0.0);
        approx(f.points[1].median, 20.0 * MS);
        approx(f.points[1].pings[2], 30.0 * MS);
        approx(f.points[2].loss, 100.0 / 3.0);
        approx(f.points[2].median, 10.0 * MS); // (5+15)/2
        assert!(f.points[2].pings[2].is_nan()); // only 2 replies
        approx(f.points[2].smoke_min(), 5.0 * MS);
        approx(f.points[2].smoke_max(), 15.0 * MS);
    }

    #[test]
    fn consolidates_avg_min_max() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(
            dir.path(),
            &[
                rra(Cf::Average, 1, 64),
                rra(Cf::Average, 4, 16),
                rra(Cf::Min, 4, 16),
                rra(Cf::Max, 4, 16),
            ],
        );
        for (i, med) in [10.0, 20.0, 30.0, 40.0].iter().enumerate() {
            st.record(&round(1000 + 10 * i as i64, &[*med], 3)).unwrap();
        }
        // max_points=1 forces the 40s-period archives over raw
        let avg = st.fetch("t/a", Cf::Average, 1000, 1039, 1).unwrap();
        assert_eq!(avg.period, 40);
        approx(avg.points[0].median, 25.0 * MS);
        let min = st.fetch("t/a", Cf::Min, 1000, 1039, 1).unwrap();
        approx(min.points[0].median, 10.0 * MS);
        let max = st.fetch("t/a", Cf::Max, 1000, 1039, 1).unwrap();
        approx(max.points[0].median, 40.0 * MS);
        // loss consolidates too: 2/3 lost every round
        approx(avg.points[0].loss, 200.0 / 3.0);
    }

    #[test]
    fn xff_voids_sparse_windows() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path(), &[rra(Cf::Average, 4, 16)]);
        // one of four PDPs in window [1000, 1040), then jump to the next
        // window so the sparse one gets finalized lazily
        st.record(&round(1000, &[10.0], 3)).unwrap();
        st.record(&round(1050, &[10.0], 3)).unwrap();
        let f = st.fetch("t/a", Cf::Average, 1000, 1039, 0).unwrap();
        assert_eq!(f.points[0].ts, 1000);
        assert!(f.points[0].median.is_nan(), "1/4 known < xff 0.5 must be unknown");
        // ...but with 2/4 known it survives
        let st2 = Store::open(&dir.path().join("b"), 10, 3, &[rra(Cf::Average, 4, 16)]).unwrap();
        st2.record(&round(1000, &[10.0], 3)).unwrap();
        st2.record(&round(1010, &[20.0], 3)).unwrap();
        st2.record(&round(1050, &[10.0], 3)).unwrap();
        let f2 = st2.fetch("t/a", Cf::Average, 1000, 1039, 0).unwrap();
        approx(f2.points[0].median, 15.0 * MS);
    }

    #[test]
    fn wraparound_invalidates_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path(), &[rra(Cf::Average, 1, 4)]);
        for i in 0..6 {
            st.record(&round(1000 + 10 * i, &[10.0], 3)).unwrap();
        }
        let f = st.fetch("t/a", Cf::Average, 1000, 1059, 0).unwrap();
        assert!(f.points[0].median.is_nan()); // 1000: overwritten by 1040
        assert!(f.points[1].median.is_nan()); // 1010: overwritten by 1050
        for p in &f.points[2..] {
            approx(p.median, 10.0 * MS);
        }
    }

    #[test]
    fn reopen_continues_in_progress_window() {
        let dir = tempfile::tempdir().unwrap();
        let specs = [rra(Cf::Average, 4, 16)];
        {
            let st = store(dir.path(), &specs);
            st.record(&round(1000, &[10.0], 3)).unwrap();
            st.record(&round(1010, &[20.0], 3)).unwrap();
        } // dropped: mmap flushed
        let st = store(dir.path(), &specs);
        st.record(&round(1020, &[30.0], 3)).unwrap();
        st.record(&round(1030, &[40.0], 3)).unwrap();
        let f = st.fetch("t/a", Cf::Average, 1000, 1039, 0).unwrap();
        approx(f.points[0].median, 25.0 * MS); // all four PDPs, across reopen
    }

    #[test]
    fn rejects_layout_change() {
        let dir = tempfile::tempdir().unwrap();
        let specs = [rra(Cf::Average, 1, 64)];
        store(dir.path(), &specs).record(&round(1000, &[10.0], 3)).unwrap();
        let st = Store::open(dir.path(), 10, 5, &specs).unwrap(); // pings 3 -> 5
        assert!(st.record(&round(1010, &[10.0], 3)).is_err());
    }

    #[test]
    fn total_loss_round() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path(), &[rra(Cf::Average, 1, 64)]);
        st.record(&round(1000, &[], 3)).unwrap();
        let f = st.fetch("t/a", Cf::Average, 1000, 1009, 0).unwrap();
        approx(f.points[0].loss, 100.0);
        assert!(f.points[0].median.is_nan());
        assert!(f.points[0].smoke_min().is_nan());
    }

    #[test]
    fn validates_target_paths() {
        assert!(validate_target_path("isp/gw-1.example").is_ok());
        for bad in ["", "a//b", "../a", "a/..", "a/b c", "a/é"] {
            assert!(validate_target_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
