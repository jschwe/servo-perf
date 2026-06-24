// tools/servoperf/src/cmd/dump.rs
//! Ad-hoc pftrace dump. Given a .pftrace, prints every span above a
//! duration threshold in the [0, until] window, grouped by thread. The
//! baseline (t=0) is the earliest slice timestamp in the file — good
//! enough when we only care about within-trace relative timing.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::cli::DumpArgs;
use crate::trace;

pub fn run(args: DumpArgs) -> Result<()> {
    let slices = trace::parse(&args.pftrace)?;
    if slices.is_empty() {
        println!("(trace empty)");
        return Ok(());
    }
    let t0_ns = slices.iter().map(|s| s.ts_ns).min().unwrap_or(0);
    let min_dur_ns = (args.min_dur_ms * 1_000_000.0) as u64;
    let until_ns = t0_ns + (args.until_ms * 1_000_000.0) as u64;

    // Group by thread.
    let mut by_thread: BTreeMap<String, Vec<&trace::Slice>> = BTreeMap::new();
    for s in &slices {
        if s.ts_ns >= until_ns {
            continue;
        }
        if s.dur_ns < min_dur_ns && s.dur_ns != 0 {
            // dur_ns == 0 means instant event; keep regardless of filter.
            continue;
        }
        by_thread.entry(s.thread.clone()).or_default().push(s);
    }

    // Pre-compute per-thread aggregates (name → total_dur_ns, count) for the
    // top-N "many tiny slices that add up" view.
    for (thread, items) in &by_thread {
        println!("\n=== thread: {} ===", thread);
        println!(
            "{:<8} {:>8} {:>7}  {}",
            "ts_ms", "dur_ms", "count", "name"
        );

        let mut seen: BTreeMap<String, (f64, u32)> = BTreeMap::new();
        for s in items {
            let entry = seen.entry(s.name.clone()).or_insert((0.0, 0));
            entry.0 += s.dur_ns as f64 / 1_000_000.0;
            entry.1 += 1;
        }
        let mut unique: Vec<_> = seen.into_iter().collect();
        unique.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap());

        // Print a short "top by total time" summary first.
        println!("  -- top by total duration --");
        for (name, (total, count)) in unique.iter().take(15) {
            println!("  {:>8.2} ms ({:>3}×)  {}", total, count, name);
        }

        // Then a timeline of the largest slices in order.
        println!("  -- timeline --");
        let mut chrono = items.clone();
        chrono.sort_by_key(|s| s.ts_ns);
        for s in chrono.iter().take(40) {
            let ts_ms = (s.ts_ns - t0_ns) as f64 / 1_000_000.0;
            let dur_ms = s.dur_ns as f64 / 1_000_000.0;
            println!(
                "  {:>8.2} {:>8.2}          {}",
                ts_ms, dur_ms, s.name
            );
        }
        if chrono.len() > 40 {
            println!("  ... ({} more)", chrono.len() - 40);
        }
    }

    Ok(())
}
