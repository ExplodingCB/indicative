//! `indicative.exe --bench` - measures indexing and search speed against the
//! real index on this machine and prints a summary to stdout
//! (run it from a terminal: `indicative.exe --bench | more`).

use crate::apps::{self, AppsCache};
use crate::config::Config;
use crate::history::History;
use crate::{index, search};
use std::time::{Duration, Instant};

fn pct(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn us(d: Duration) -> String {
    format!("{:.0} us", d.as_secs_f64() * 1e6)
}

/// `--bench` uses the configured folders; `--bench <dir> [dir...]` indexes
/// only the given folders (up to 1M entries) to test large indexes.
pub fn run(dirs: &[std::path::PathBuf]) {
    let cfg = Config::load();
    let t = Instant::now();
    let (roots, cap) = if dirs.is_empty() {
        (index::default_roots(&cfg.extra_roots, cfg.index_user_folders), cfg.max_files)
    } else {
        (index::default_roots(dirs, false), 1_000_000)
    };
    let fi = index::scan(roots, cap);
    let scan = t.elapsed();
    let bytes = fi.names.capacity() + fi.entries.capacity() * std::mem::size_of::<index::Entry>();

    let t = Instant::now();
    let ac = apps::newest_cache().and_then(|p| AppsCache::open(&p));
    let map = t.elapsed();
    let history = History::load();
    let n_apps = ac.as_ref().map_or(0, |a| a.apps.len());

    println!("Indicative benchmark");
    println!("  file index : {} entries, {:.1} MB, scanned in {} ms", fi.entries.len(), bytes as f64 / 1048576.0, scan.as_millis());
    println!("  apps       : {} (cache mapped in {})", n_apps, us(map));
    println!();
    println!("  full search per keystroke (apps + every file name + calculator + ranking)");
    println!("  {:<16} {:>9} {:>9} {:>9}  {}", "query", "median", "p99", "max", "results");

    let cx = search::Ctx { apps: ac.as_ref(), files: Some(&fi), history: &history, web_search: &cfg.web_search };
    let queries = ["c", "co", "cod", "code", "vsc", "vis code", "report", "settings", "readme.md", "2^10/4+sqrt(16)"];
    let mut all = Vec::new();
    for q in queries {
        let w: Vec<u16> = q.encode_utf16().collect();
        for _ in 0..20 {
            std::hint::black_box(search::run(&w, &cx));
        }
        let mut times = Vec::with_capacity(300);
        let mut rows = 0;
        for _ in 0..300 {
            let t = Instant::now();
            let r = std::hint::black_box(search::run(&w, &cx));
            times.push(t.elapsed());
            rows = r.iter().map(|s| s.items.len()).sum();
        }
        times.sort();
        all.extend_from_slice(&times);
        println!("  {:<16} {:>9} {:>9} {:>9}  {}", q, us(pct(&times, 0.5)), us(pct(&times, 0.99)), us(*times.last().unwrap()), rows);
    }
    all.sort();
    println!();
    println!("  overall: median {}, p99 {} across {} searches", us(pct(&all, 0.5)), us(pct(&all, 0.99)), all.len());
}
