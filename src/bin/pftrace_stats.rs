//! Read a .pftrace file and print per-span duration histograms for selected names.

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use servoperf::trace::{parse, Slice};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: pftrace_stats <path.pftrace> [span_name1 span_name2 ...]");
        std::process::exit(2);
    }
    let path = PathBuf::from(&args[1]);
    let mut filter: Vec<String> = args[2..].to_vec();
    if filter.is_empty() {
        filter = vec![
            "ScriptEvaluate",
            "image_descriptor_and_serializable_data",
            "render",
            "Painting",
            "Repaint",
            "process_canvas_2d_message",
            "Layout",
            "handle_reflow",
            "perform_updates",
            "spin_event_loop",
            "WindowRenderingContext::present",
            "SoftwareRenderingContext::present",
        ]
        .into_iter()
        .map(String::from)
        .collect();
    }
    let slices = parse(&path)?;
    println!("total slices: {}", slices.len());

    let mut by_name: BTreeMap<String, Vec<&Slice>> = BTreeMap::new();
    for s in &slices {
        if filter.iter().any(|f| f == &s.name) {
            by_name.entry(s.name.clone()).or_default().push(s);
        }
    }

    println!(
        "\n{:40} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "span", "count", "total_s", "avg_ms", "p50_ms", "p75_ms", "p95_ms", "max_ms"
    );
    for name in &filter {
        let Some(v) = by_name.get(name) else { continue };
        let mut durs: Vec<u64> = v.iter().map(|s| s.dur_ns).collect();
        durs.sort_unstable();
        let n = durs.len();
        if n == 0 { continue }
        let total_ns: u128 = durs.iter().map(|x| *x as u128).sum();
        let avg = total_ns as f64 / n as f64 / 1e6;
        let p = |q: f64| durs[((n as f64 * q) as usize).min(n - 1)] as f64 / 1e6;
        println!(
            "{:40} {:>8} {:>10.3} {:>10.3} {:>10.3} {:>10.3} {:>10.3} {:>10.3}",
            name,
            n,
            total_ns as f64 / 1e9,
            avg,
            p(0.50),
            p(0.75),
            p(0.95),
            durs.last().copied().unwrap_or(0) as f64 / 1e6
        );
    }
    Ok(())
}
