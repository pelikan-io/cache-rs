//! Run a grid of measurements and emit CSV.
//!
//! This is a subcommand rather than a shell script so that sweeping works
//! wherever the crate builds. The scripts it replaces needed bash and
//! `taskset`, neither of which exists on two of the three platforms this
//! repository is tested on.
//!
//! Each measurement runs in a fresh child process — the same isolation a
//! per-run shell loop gave — so no cache, allocator, or metrics state carries
//! from one point of the grid to the next.
//!
//! Pinning is deliberately absent. Pin the whole sweep from outside instead,
//! which pins every child with it:
//!
//! ```text
//! taskset -c 8-15 segbench sweep --threads 1,2,4,6,8 > scaling.csv
//! ```

use std::process::Command;

pub struct Grid {
    pub threads: Vec<usize>,
    pub writes: Vec<u32>,
    pub dists: Vec<String>,
    /// Mode and its arguments, already split (empty means `base`).
    pub mode: Vec<String>,
    pub reps: usize,
    pub warmup: u64,
    pub measure: u64,
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            threads: vec![1, 2, 4, 6, 8],
            writes: vec![0, 50, 100],
            dists: vec!["uniform".to_owned(), "zipf".to_owned()],
            mode: Vec::new(),
            reps: 3,
            warmup: 2,
            measure: 8,
        }
    }
}

const USAGE: &str = "usage: segbench sweep [options] > results.csv
  --threads 1,2,4,6,8     thread counts to sweep
  --write   0,50,100      write percentages
  --dist    uniform,zipf  key distributions
  --mode    'part 16'     mode and its arguments (default: base)
  --reps    3             repeats, interleaved across the grid
  --warmup  2             warmup seconds per measurement
  --measure 8             measured seconds per measurement

Pin the whole sweep from outside to hold it on one kind of core, e.g.
  taskset -c 8-15 segbench sweep --threads 1,2,4,6,8 > scaling.csv";

fn list<T: std::str::FromStr>(v: &str, flag: &str) -> Result<Vec<T>, String> {
    v.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.trim()
                .parse::<T>()
                .map_err(|_| format!("{flag}: cannot parse {s:?}"))
        })
        .collect()
}

pub fn parse(args: &[String]) -> Result<Grid, String> {
    let mut g = Grid::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if flag == "-h" || flag == "--help" {
            return Err(USAGE.to_owned());
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{flag}: missing value\n\n{USAGE}"))?;
        match flag {
            "--threads" => g.threads = list(value, flag)?,
            "--write" => g.writes = list(value, flag)?,
            "--dist" => g.dists = value.split(',').map(str::to_owned).collect(),
            "--mode" => g.mode = value.split_whitespace().map(str::to_owned).collect(),
            "--reps" => g.reps = list::<usize>(value, flag)?[0],
            "--warmup" => g.warmup = list::<u64>(value, flag)?[0],
            "--measure" => g.measure = list::<u64>(value, flag)?[0],
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
        i += 2;
    }
    if g.threads.is_empty() || g.writes.is_empty() || g.dists.is_empty() {
        return Err("--threads, --write, and --dist must each name at least one value".to_owned());
    }
    Ok(g)
}

pub fn run(g: &Grid) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let total = g.reps * g.threads.len() * g.writes.len() * g.dists.len();
    println!("threads,write_pct,dist,mode,mops");

    let mut done = 0;
    // Repeats are the outer loop so drift and thermals spread across the whole
    // grid instead of landing on whichever points ran last.
    for rep in 1..=g.reps {
        for &t in &g.threads {
            for &w in &g.writes {
                for d in &g.dists {
                    let mut cmd = Command::new(&exe);
                    cmd.arg(t.to_string())
                        .arg(w.to_string())
                        .arg(d)
                        .arg(g.warmup.to_string())
                        .arg(g.measure.to_string())
                        .args(&g.mode);
                    let out = cmd.output().map_err(|e| format!("spawning {exe:?}: {e}"))?;
                    if !out.status.success() {
                        return Err(format!(
                            "measurement t={t} write={w} dist={d} failed ({}): {}",
                            out.status,
                            String::from_utf8_lossy(&out.stderr).trim()
                        ));
                    }
                    print!("{}", String::from_utf8_lossy(&out.stdout));
                    done += 1;
                    eprintln!("[{done}/{total}] rep {rep}  t={t}  write={w}%  {d}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Grid {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn defaults_cover_read_write_and_mix() {
        let g = parse_ok(&[]);
        assert_eq!(g.threads, vec![1, 2, 4, 6, 8]);
        assert_eq!(g.writes, vec![0, 50, 100]);
        assert_eq!(g.reps, 3);
        assert!(g.mode.is_empty());
    }

    #[test]
    fn parses_lists_and_mode_arguments() {
        let g = parse_ok(&["--threads", "1,8", "--dist", "zipf", "--mode", "part 16"]);
        assert_eq!(g.threads, vec![1, 8]);
        assert_eq!(g.dists, vec!["zipf"]);
        assert_eq!(g.mode, vec!["part", "16"]);
    }

    #[test]
    fn rejects_bad_input_instead_of_guessing() {
        let bad = |a: &[&str]| parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>()).is_err();
        assert!(bad(&["--threads", "eight"]));
        assert!(bad(&["--nope", "1"]));
        assert!(bad(&["--threads"])); // missing value
        assert!(bad(&["--threads", ""])); // empty grid axis
    }
}
