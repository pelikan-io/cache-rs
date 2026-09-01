//! Summarize segbench CSV output: per-series medians, ranges, and speedups.
//!
//! This lives in the harness rather than in a script beside it because the
//! harness already defines the CSV format. One place decides what a row means.
//!
//! Reads any number of CSVs and writes JSON to stdout, keyed by
//! `<op>_<dist>[_<mode>]`. Unknown modes are named rather than dropped, so
//! adding a mode to the harness needs no change here.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;

/// One thread count's worth of repeats.
struct Point {
    threads: usize,
    med: f64,
    min: f64,
    max: f64,
    n: usize,
    speedup: f64,
}

fn series_name(write_pct: u32, dist: &str, mode: &str) -> String {
    let op = match write_pct {
        0 => "read".to_owned(),
        100 => "write".to_owned(),
        // anything else is a mixture, named by its write share
        other => format!("mix{other}"),
    };
    if mode == "base" {
        format!("{op}_{dist}")
    } else {
        format!("{op}_{dist}_{mode}")
    }
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

fn round(v: f64, places: u32) -> f64 {
    let scale = 10f64.powi(places as i32);
    (v * scale).round() / scale
}

/// Parse one data row. Returns `None` for the header, comments, and `DONE`.
///
/// Accepts both the current `threads,write_pct,dist,mode,mops` shape and the
/// earlier one that omitted `mode`.
fn parse_row(line: &str) -> Option<(String, usize, f64)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line == "DONE" {
        return None;
    }
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() < 4 {
        return None;
    }
    // The header fails this parse, which is how it gets skipped.
    let threads: usize = fields[0].parse().ok()?;
    let write_pct: u32 = fields[1].parse().ok()?;
    let dist = fields[2];
    let (mode, mops) = if fields.len() >= 5 {
        (fields[3], fields[4])
    } else {
        ("base", fields[3])
    };
    let mops: f64 = mops.parse().ok()?;
    Some((series_name(write_pct, dist, mode), threads, mops))
}

fn to_points(by_threads: BTreeMap<usize, Vec<f64>>) -> Vec<Point> {
    let mut points: Vec<Point> = by_threads
        .into_iter()
        .map(|(threads, mut samples)| {
            samples.sort_by(f64::total_cmp);
            Point {
                threads,
                med: round(median(&samples), 3),
                min: round(samples[0], 3),
                max: round(samples[samples.len() - 1], 3),
                n: samples.len(),
                speedup: 0.0,
            }
        })
        .collect();

    // Speedup is against this series' own single-thread median, so series with
    // different per-op sampling costs stay comparable.
    let base = points
        .iter()
        .find(|p| p.threads == 1)
        .or(points.first())
        .map(|p| p.med)
        .unwrap_or(1.0);
    if base > 0.0 {
        for p in &mut points {
            p.speedup = round(p.med / base, 2);
        }
    }
    points
}

fn render(series: &BTreeMap<String, Vec<Point>>) -> String {
    let mut out = String::from("{\n");
    for (i, (name, points)) in series.iter().enumerate() {
        let _ = writeln!(out, "  {name:?}: [");
        for (j, p) in points.iter().enumerate() {
            let _ = write!(
                out,
                "    {{\"t\": {}, \"med\": {}, \"min\": {}, \"max\": {}, \"n\": {}, \"speedup\": {}}}",
                p.threads, p.med, p.min, p.max, p.n, p.speedup
            );
            out.push_str(if j + 1 < points.len() { ",\n" } else { "\n" });
        }
        out.push_str("  ]");
        out.push_str(if i + 1 < series.len() { ",\n" } else { "\n" });
    }
    out.push_str("}\n");
    out
}

pub fn run(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("usage: segbench aggregate <results.csv>...".to_owned());
    }

    let mut raw: BTreeMap<String, BTreeMap<usize, Vec<f64>>> = BTreeMap::new();
    for path in paths {
        let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        for line in text.lines() {
            if let Some((name, threads, mops)) = parse_row(line) {
                raw.entry(name)
                    .or_default()
                    .entry(threads)
                    .or_default()
                    .push(mops);
            }
        }
    }
    if raw.is_empty() {
        return Err("no data rows found".to_owned());
    }

    let series: BTreeMap<String, Vec<Point>> =
        raw.into_iter().map(|(k, v)| (k, to_points(v))).collect();
    print!("{}", render(&series));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_ops_by_write_share() {
        assert_eq!(series_name(0, "uniform", "base"), "read_uniform");
        assert_eq!(series_name(100, "zipf", "base"), "write_zipf");
        assert_eq!(series_name(50, "uniform", "base"), "mix50_uniform");
        assert_eq!(series_name(0, "zipf", "shuf"), "read_zipf_shuf");
        // a mode the aggregator has never heard of still gets a name
        assert_eq!(series_name(50, "zipf", "part-p16"), "mix50_zipf_part-p16");
    }

    #[test]
    fn skips_header_comments_and_done() {
        assert!(parse_row("threads,write_pct,dist,mode,mops").is_none());
        assert!(parse_row("# rep 1 done").is_none());
        assert!(parse_row("DONE").is_none());
        assert!(parse_row("").is_none());
    }

    #[test]
    fn reads_both_row_shapes() {
        assert_eq!(
            parse_row("8,0,uniform,base,21.796"),
            Some(("read_uniform".to_owned(), 8, 21.796))
        );
        // legacy rows without a mode column
        assert_eq!(
            parse_row("4,50,zipf,2.969"),
            Some(("mix50_zipf".to_owned(), 4, 2.969))
        );
    }

    #[test]
    fn medians_and_speedups() {
        let mut by_threads = BTreeMap::new();
        by_threads.insert(1, vec![2.0, 1.0, 3.0]);
        by_threads.insert(4, vec![8.0, 4.0]); // even count averages the middle pair
        let points = to_points(by_threads);

        assert_eq!(points[0].med, 2.0);
        assert_eq!(points[0].min, 1.0);
        assert_eq!(points[0].max, 3.0);
        assert_eq!(points[0].n, 3);
        assert_eq!(points[0].speedup, 1.0);

        assert_eq!(points[1].med, 6.0);
        assert_eq!(points[1].speedup, 3.0);
    }

    #[test]
    fn speedup_falls_back_to_the_lowest_thread_count() {
        // a sweep that never ran a single-thread point still normalizes
        let mut by_threads = BTreeMap::new();
        by_threads.insert(2, vec![4.0]);
        by_threads.insert(8, vec![6.0]);
        let points = to_points(by_threads);
        assert_eq!(points[0].speedup, 1.0);
        assert_eq!(points[1].speedup, 1.5);
    }
}
