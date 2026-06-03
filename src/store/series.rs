//! One target's on-disk series: a fixed-size, memory-mapped file holding
//! RRD-style round-robin archives.
//!
//! Layout (all integers little-endian, all floats f64, F = pings + 2):
//!
//! ```text
//! file header (64 B):
//!   0  magic "DABPING1"
//!   8  step (u64, seconds)
//!  16  pings (u32, N)
//!  20  n_archives (u32)
//!  24  last_update (i64, unix ts of newest PDP)
//!  32  reserved
//! per archive header (32 + 16·F B):
//!   0  cf (u32: 0 avg, 1 min, 2 max)
//!   4  steps (u32, PDPs per consolidated point)
//!   8  rows (u64)
//!  16  xff (f64)
//!  24  acc_window_start (i64, i64::MIN = no window in progress)
//!  32  acc_vals (F × f64: running sum, or running min/max)
//!  32+8F  acc_counts (F × f64: known PDPs per field this window)
//! per archive data: rows × row, row (8 + 8·F B):
//!   0  ts (i64: window start; row is valid iff ts matches its slot's time)
//!   8  fields (F × f64: loss%, median, ping_1..ping_N sorted; NaN = unknown)
//! ```
//!
//! Slot index is a pure function of time: `(ts / period) % rows`, so writes
//! are O(1) and a torn row is self-invalidating via its ts field. The mmap
//! is flushed on drop; in between, the OS owns durability.

use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{Context, Result, bail};
use memmap2::MmapMut;

use super::{Cf, RraSpec};

const MAGIC: &[u8; 8] = b"DABPING1";
const FILE_HEADER_SIZE: usize = 64;
const ARCHIVE_HEADER_FIXED: usize = 32;
const EMPTY_WINDOW: i64 = i64::MIN;
/// Refuse to materialize absurd fetches (e.g. a year of 1s data).
const MAX_FETCH_POINTS: u64 = 1_000_000;

pub struct TargetSeries {
    map: MmapMut,
    step: u64,
    fields: usize, // F = pings + 2
    archives: Vec<Geom>,
}

#[derive(Clone, Copy)]
struct Geom {
    cf: Cf,
    steps: u32,
    rows: u64,
    xff: f64,
    header_off: usize,
    data_off: usize,
}

fn align(t: i64, period: u64) -> i64 {
    t.div_euclid(period as i64) * period as i64
}

impl TargetSeries {
    pub fn open_or_create(path: &Path, step: u64, pings: u32, specs: &[RraSpec]) -> Result<TargetSeries> {
        let fields = pings as usize + 2;
        let ah_size = ARCHIVE_HEADER_FIXED + 16 * fields;
        let row_size = 8 + 8 * fields;

        let mut archives = Vec::with_capacity(specs.len());
        let mut data_off = FILE_HEADER_SIZE + specs.len() * ah_size;
        for (i, s) in specs.iter().enumerate() {
            archives.push(Geom {
                cf: s.cf,
                steps: s.steps,
                rows: s.rows,
                xff: s.xff,
                header_off: FILE_HEADER_SIZE + i * ah_size,
                data_off,
            });
            data_off += s.rows as usize * row_size;
        }
        let total_size = data_off;

        let exists = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("cannot open series file {}", path.display()))?;
        if !exists {
            file.set_len(total_size as u64)?;
        } else if file.metadata()?.len() != total_size as u64 {
            bail!(
                "{}: file size does not match the configured layout — \
                 the step/pings/rra config changed; move the file away or restore the config",
                path.display()
            );
        }
        // SAFETY: we own the file; concurrent mappings are guarded by the
        // per-series Mutex in Store.
        let map = unsafe { MmapMut::map_mut(&file)? }.into();
        let mut series = TargetSeries { map, step, fields, archives };

        if !exists {
            series.init_headers(step, pings, specs);
        } else {
            series.validate_headers(path, step, pings, specs)?;
        }
        Ok(series)
    }

    fn init_headers(&mut self, step: u64, pings: u32, specs: &[RraSpec]) {
        self.map[0..8].copy_from_slice(MAGIC);
        self.w_u64(8, step);
        self.w_u32(16, pings);
        self.w_u32(20, specs.len() as u32);
        self.w_i64(24, 0);
        for (g, s) in self.archives.clone().iter().zip(specs) {
            let h = g.header_off;
            self.w_u32(h, s.cf.as_u32());
            self.w_u32(h + 4, s.steps);
            self.w_u64(h + 8, s.rows);
            self.w_f64(h + 16, s.xff);
            self.w_i64(h + 24, EMPTY_WINDOW);
            // acc_vals/acc_counts are zero from the fresh file; reset_acc
            // runs before any accumulation, so that is fine
        }
    }

    fn validate_headers(&self, path: &Path, step: u64, pings: u32, specs: &[RraSpec]) -> Result<()> {
        let complain = |what: &str| {
            bail!(
                "{}: existing series file does not match the config ({what} changed); \
                 move the file away or restore the config",
                path.display()
            )
        };
        if &self.map[0..8] != MAGIC {
            return complain("magic — not a dabping series file?");
        }
        if self.r_u64(8) != step {
            return complain("database.step");
        }
        if self.r_u32(16) != pings {
            return complain("database.pings");
        }
        if self.r_u32(20) as usize != specs.len() {
            return complain("rra count");
        }
        for (g, s) in self.archives.iter().zip(specs) {
            let h = g.header_off;
            if self.r_u32(h) != s.cf.as_u32()
                || self.r_u32(h + 4) != s.steps
                || self.r_u64(h + 8) != s.rows
                || self.r_f64(h + 16) != s.xff
            {
                return complain("rra table");
            }
        }
        Ok(())
    }

    pub fn last_update(&self) -> i64 {
        self.r_i64(24)
    }

    /// Record one round's primary data point. `fields` is
    /// [loss%, median, ping_1..ping_N] with NaN for unknowns.
    pub fn record(&mut self, ts: i64, fields: &[f64]) -> Result<()> {
        assert_eq!(fields.len(), self.fields);
        let t = align(ts, self.step);
        let last = self.last_update();
        if last != 0 && t <= last {
            // duplicate or out-of-order round; first write wins
            return Ok(());
        }
        for ai in 0..self.archives.len() {
            let g = self.archives[ai];
            let period = self.step * g.steps as u64;
            let w = align(t, period);
            let aws = self.acc_window_start(ai);
            if aws != w {
                if aws != EMPTY_WINDOW {
                    // the previous window never completed (daemon gap);
                    // finalize it with what it has — xff decides validity
                    self.finalize(ai, aws);
                }
                self.reset_acc(ai, w);
            }
            self.accumulate(ai, fields);
            if (t - w) as u64 / self.step == (g.steps - 1) as u64 {
                self.finalize(ai, w);
                self.set_acc_window_start(ai, EMPTY_WINDOW);
            }
        }
        self.w_i64(24, t);
        Ok(())
    }

    /// Fetch points for [from, to], picking the best-resolution archive that
    /// can serve the requested cf, covers `from`, and (when max_points > 0)
    /// doesn't return more than max_points. Returns (period, points); rows
    /// with no valid data come back NaN-filled, never omitted.
    pub fn fetch(&self, cf: Cf, from: i64, to: i64, max_points: u64) -> Result<(u64, Vec<(i64, Vec<f64>)>)> {
        // a steps=1 archive holds raw samples, which serve any cf
        let mut cands: Vec<&Geom> =
            self.archives.iter().filter(|g| g.cf == cf || g.steps == 1).collect();
        if cands.is_empty() {
            bail!("no archive can serve {cf:?}; add a matching [[database.rra]]");
        }
        cands.sort_by_key(|g| g.steps);

        let last = self.last_update();
        let span = (to - from).max(0) as u64;
        let fits = |g: &Geom| max_points == 0 || span / (self.step * g.steps as u64) <= max_points;
        let covers = |g: &Geom| {
            let period = self.step * g.steps as u64;
            last == 0 || align(from, period) >= align(last, period) - ((g.rows - 1) * period) as i64
        };
        let chosen = *cands
            .iter()
            .find(|g| fits(g) && covers(g))
            .or_else(|| cands.iter().rev().find(|g| fits(g)))
            .unwrap_or(cands.last().expect("cands is non-empty"));

        let period = self.step * chosen.steps as u64;
        let (start, end) = (align(from, period), align(to, period));
        let mut n = ((end - start) as u64 / period) + 1;
        // when even the coarsest archive over-delivers (e.g. tiny step with a
        // short rra table), stride-sample rows to stay near the budget; the
        // skipped rows' peaks are lost, which mirrors what the caller asked
        // for by bounding points
        let stride = if max_points > 0 && n > max_points {
            n.div_ceil(max_points)
        } else {
            1
        };
        n = n.div_ceil(stride);
        if n > MAX_FETCH_POINTS {
            bail!("fetch would return {n} points; narrow the range");
        }

        let row_size = 8 + 8 * self.fields;
        let mut points = Vec::with_capacity(n as usize);
        let mut w = start;
        while w <= end {
            let slot = (w.div_euclid(period as i64) as u64) % chosen.rows;
            let off = chosen.data_off + slot as usize * row_size;
            let vals = if self.r_i64(off) == w {
                (0..self.fields).map(|i| self.r_f64(off + 8 + 8 * i)).collect()
            } else {
                vec![f64::NAN; self.fields]
            };
            points.push((w, vals));
            w += (period * stride) as i64;
        }
        Ok((period * stride, points))
    }

    // ---- consolidation ----

    fn acc_window_start(&self, ai: usize) -> i64 {
        self.r_i64(self.archives[ai].header_off + 24)
    }

    fn set_acc_window_start(&mut self, ai: usize, v: i64) {
        self.w_i64(self.archives[ai].header_off + 24, v);
    }

    fn acc_off(&self, ai: usize) -> (usize, usize) {
        let h = self.archives[ai].header_off + ARCHIVE_HEADER_FIXED;
        (h, h + 8 * self.fields)
    }

    fn reset_acc(&mut self, ai: usize, window: i64) {
        let init = match self.archives[ai].cf {
            Cf::Average => 0.0,
            Cf::Min | Cf::Max => f64::NAN,
        };
        let (vals, counts) = self.acc_off(ai);
        for i in 0..self.fields {
            self.w_f64(vals + 8 * i, init);
            self.w_f64(counts + 8 * i, 0.0);
        }
        self.set_acc_window_start(ai, window);
    }

    fn accumulate(&mut self, ai: usize, fields: &[f64]) {
        let cf = self.archives[ai].cf;
        let (vals, counts) = self.acc_off(ai);
        for (i, &v) in fields.iter().enumerate() {
            if v.is_nan() {
                continue;
            }
            let cur = self.r_f64(vals + 8 * i);
            let next = match cf {
                Cf::Average => {
                    if cur.is_nan() {
                        v
                    } else {
                        cur + v
                    }
                }
                Cf::Min if cur.is_nan() || v < cur => v,
                Cf::Max if cur.is_nan() || v > cur => v,
                Cf::Min | Cf::Max => cur,
            };
            self.w_f64(vals + 8 * i, next);
            let c = self.r_f64(counts + 8 * i);
            self.w_f64(counts + 8 * i, c + 1.0);
        }
    }

    /// Write the consolidated row for the window starting at `w`.
    fn finalize(&mut self, ai: usize, w: i64) {
        let g = self.archives[ai];
        let (vals, counts) = self.acc_off(ai);
        let min_known = (1.0 - g.xff) * g.steps as f64;
        let row: Vec<f64> = (0..self.fields)
            .map(|i| {
                let c = self.r_f64(counts + 8 * i);
                if c <= 0.0 || c < min_known {
                    return f64::NAN; // too much of the window is unknown (xff)
                }
                let v = self.r_f64(vals + 8 * i);
                match g.cf {
                    Cf::Average => v / c,
                    Cf::Min | Cf::Max => v,
                }
            })
            .collect();

        let period = self.step * g.steps as u64;
        let row_size = 8 + 8 * self.fields;
        let slot = (w.div_euclid(period as i64) as u64) % g.rows;
        let off = g.data_off + slot as usize * row_size;
        self.w_i64(off, w);
        for (i, v) in row.iter().enumerate() {
            self.w_f64(off + 8 + 8 * i, *v);
        }
    }

    // ---- raw little-endian accessors ----

    fn r_u32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.map[off..off + 4].try_into().unwrap())
    }
    fn r_u64(&self, off: usize) -> u64 {
        u64::from_le_bytes(self.map[off..off + 8].try_into().unwrap())
    }
    fn r_i64(&self, off: usize) -> i64 {
        i64::from_le_bytes(self.map[off..off + 8].try_into().unwrap())
    }
    fn r_f64(&self, off: usize) -> f64 {
        f64::from_le_bytes(self.map[off..off + 8].try_into().unwrap())
    }
    fn w_u32(&mut self, off: usize, v: u32) {
        self.map[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn w_u64(&mut self, off: usize, v: u64) {
        self.map[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn w_i64(&mut self, off: usize, v: i64) {
        self.map[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn w_f64(&mut self, off: usize, v: f64) {
        self.map[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
}

impl Drop for TargetSeries {
    fn drop(&mut self) {
        let _ = self.map.flush();
    }
}
