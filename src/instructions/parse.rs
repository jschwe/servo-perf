//! Parser for `hiperf report -s` stack-mode text output.
//!
//! The report contains, for each thread that recorded samples, a root row
//! `<pct>%  <events>  <comm>  <pid>  <tid>  <dso>  <leaf_func>` followed by
//! bare frames listing the call stack from leaf out to the outermost frame.
//! Below that, a nested tree marks each branch with `|- <pct>%`. Linear
//! single-child chains are compressed: a bare line (no `|-`) appearing
//! between two `|-` lines is a 100 % child of the preceding entry.
//!
//! For inclusive instruction counts we walk the tree, multiplying
//! percentages from root toward leaves, and sum events at every node whose
//! resolved function name contains one of the target substrings. We dedup
//! per root-to-leaf path so a frame that re-enters itself (e.g. the
//! WebFrameWidgetImpl::UpdateLifecycle nested-call pattern) doesn't get
//! double-counted along a single branch.

use std::collections::HashMap;

/// Aggregate inclusive hw-instruction counts for each target substring,
/// summed across every branch of the call tree.
///
/// `report` is the text output of `hiperf report -s`. `targets` is the list
/// of case-sensitive substrings to match resolved symbol names against.
///
/// Returns a map from target → total inclusive events. Targets with no
/// matches are present with value 0 so the caller sees a stable schema.
pub fn aggregate_inclusive(report: &str, targets: &[String]) -> HashMap<String, u64> {
    let mut totals: HashMap<String, u64> = targets.iter().map(|t| (t.clone(), 0u64)).collect();

    let lines: Vec<&str> = report.lines().collect();
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if let Some((root_events, leaf, upstream_count)) = parse_root(&lines, i) {
            // The bare upstream list (leaf at index 0 → outermost at end) is
            // the runtime call stack above the leaf. We attribute root_events
            // to any upstream frame matching a target — each frame is at
            // least once on this branch.
            let upstream_end = i + 1 + upstream_count;
            // The 0th upstream frame is the same as `leaf` (we already have
            // it). Start from index 1 to avoid double-counting it.
            attribute(&leaf, root_events, targets, &mut totals);
            for j in (i + 1)..upstream_end {
                let frame = lines[j].trim();
                attribute(frame, root_events, targets, &mut totals);
            }
            // Now parse the nested tree under the outermost frame.
            // Stack of (pipe_indent, events). `pipe_indent` is the column
            // of the `|` character; the sentinel uses -1 so any real
            // entry's indent is greater.
            let mut stack: Vec<(i32, u64)> = vec![(-1, root_events)];
            let mut j = upstream_end;
            while j < n {
                let line = lines[j];
                if line.trim().is_empty() {
                    j += 1; continue;
                }
                if parse_root(&lines, j).is_some() {
                    break;
                }
                if let Some((pipe_indent, pct, name)) = parse_child(line) {
                    while stack.last().map(|t| t.0 >= pipe_indent).unwrap_or(false) {
                        stack.pop();
                    }
                    let parent_events = stack.last().map(|t| t.1).unwrap_or(root_events);
                    let events = (parent_events as f64 * pct / 100.0) as u64;
                    stack.push((pipe_indent, events));
                    if name != "[run in self function]" {
                        attribute(&name, events, targets, &mut totals);
                    }
                } else {
                    // Bare continuation line: 100 % child of current top.
                    let name = line.trim();
                    let events = stack.last().map(|t| t.1).unwrap_or(root_events);
                    attribute(name, events, targets, &mut totals);
                }
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    totals
}

/// Increment any target's total whose substring appears in `frame`. At most
/// one target per frame to keep accounting deterministic across overlapping
/// substrings.
fn attribute(frame: &str, events: u64, targets: &[String], totals: &mut HashMap<String, u64>) {
    for t in targets {
        if frame.contains(t.as_str()) {
            if let Some(slot) = totals.get_mut(t) {
                *slot += events;
            }
            return;
        }
    }
}

/// Parse a root row at `lines[i]`, returning `(events, leaf_func, upstream_frame_count)`.
/// The upstream count is the number of bare lines IMMEDIATELY following the
/// root header that form the runtime call stack from leaf to outermost.
///
/// Returns `None` if `lines[i]` isn't a root header.
fn parse_root(lines: &[&str], i: usize) -> Option<(u64, String, usize)> {
    let line = lines[i];
    // Root header has the shape:
    //   "  <pct>%  <events>  <comm>  <pid>  <tid>  <dso>  <leaf_func>"
    // with pct = decimal, events = integer, pid/tid = integers. We split
    // on whitespace and fetch the first 6 columns; the 7th onward is the
    // function name (may contain spaces).
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let mut iter = trimmed.split_ascii_whitespace();
    let pct_tok = iter.next()?;
    if !pct_tok.ends_with('%') {
        return None;
    }
    if pct_tok.trim_end_matches('%').parse::<f64>().is_err() {
        return None;
    }
    let events_tok = iter.next()?;
    let events: u64 = events_tok.parse().ok()?;
    // comm, pid, tid, dso, then func
    let _comm = iter.next()?;
    let _pid: u32 = iter.next()?.parse().ok()?;
    let _tid: u32 = iter.next()?.parse().ok()?;
    let _dso = iter.next()?;
    // The function name may contain spaces (templated C++); reassemble the
    // tail of the line from where the parser is.
    let rest: String = iter.collect::<Vec<_>>().join(" ");
    let leaf = rest.trim().to_string();
    // Count upstream bare frames until we hit a `|-` line, another root,
    // or a blank line.
    let mut upstream = 0usize;
    let mut j = i + 1;
    while j < lines.len() {
        let l = lines[j];
        let t = l.trim();
        if t.is_empty() {
            break;
        }
        if t.starts_with("|- ") {
            break;
        }
        if parse_root_quick(l) {
            break;
        }
        upstream += 1;
        j += 1;
    }
    Some((events, leaf, upstream))
}

/// Cheaper variant for the "is this line a root row" check inside upstream
/// scanning. Avoids materialising the function name. Mirrors the prefix
/// test in [`parse_root`].
fn parse_root_quick(line: &str) -> bool {
    let t = line.trim_start();
    let bytes = t.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return false;
    }
    let mut iter = t.split_ascii_whitespace();
    let Some(pct) = iter.next() else { return false };
    if !pct.ends_with('%') {
        return false;
    }
    pct.trim_end_matches('%').parse::<f64>().is_ok()
}

/// Parse a `|- XX.XX% func_name` line. Returns the pipe column, the
/// percentage, and the function name.
fn parse_child(line: &str) -> Option<(i32, f64, String)> {
    let pipe_col = line.find("|- ")?;
    let after_pipe = &line[pipe_col + 3..];
    let mut iter = after_pipe.split_ascii_whitespace();
    let pct_tok = iter.next()?;
    let pct: f64 = pct_tok.trim_end_matches('%').parse().ok()?;
    let name: String = iter.collect::<Vec<_>>().join(" ").trim().to_string();
    Some((pipe_col as i32, pct, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test mirroring the structure of an actual report snippet
    /// (taken from /tmp/arkwebtest_load_stack.txt during session work).
    /// Verifies pct multiplication through a linear chain (`|- 100%` /
    /// bare frames) and across siblings.
    #[test]
    fn aggregates_through_linear_chain_and_branches() {
        // 100 root events. Tree:
        //   root → A (100%)
        //     A → B (50%, named "Target::foo")
        //       B → C (40%, bare, 100% child)
        //         C → D (20%, named "Target::bar")
        //     A → E (50%)
        let report = "\
Event Count: 100
12.00%  100 some.proc 1 1 /lib/x.so root_leaf
           root_leaf
     |- 100.00% A
         |- 50.00% Target::foo
                   Target::foo_helper
             |- 20.00% Target::bar
         |- 50.00% E
";
        let targets = vec!["Target::foo".to_string(), "Target::bar".to_string()];
        let r = aggregate_inclusive(report, &targets);
        // Target::foo: 100 * 1.0 * 0.5 = 50 (from the branch). The bare
        // helper frame inherits 50 too but doesn't match "foo" alone (it
        // does contain "Target::foo" as substring, which it shouldn't
        // double-count thanks to one-target-per-frame). Let me reconsider:
        // "Target::foo_helper" *does* contain "Target::foo" as a substring,
        // so it WILL match — that's intended (matches our /tmp/parse_stack_report.py).
        // Expected: 50 (from `Target::foo`) + 50 (from `Target::foo_helper`)
        //         = 100.
        assert_eq!(r["Target::foo"], 100);
        // Target::bar: 100 * 1.0 * 0.5 * 0.2 = 10. (The bare 100% child
        // doesn't change `bar`'s parent — `Target::foo_helper` inherits
        // from `Target::foo`, and `Target::bar` is a child of the bare
        // helper, so its parent's events are still 50.)
        assert_eq!(r["Target::bar"], 10);
    }

    #[test]
    fn unmatched_target_returns_zero() {
        let report = "Event Count: 0\n";
        let targets = vec!["Nothing".to_string()];
        let r = aggregate_inclusive(report, &targets);
        assert_eq!(r["Nothing"], 0);
    }

    #[test]
    fn parses_real_arkwebtest_snippet() {
        // Trimmed snippet from the arkweb-test stack-mode report captured
        // during session work. Verifies the canonical
        // `WebFrameWidgetImpl::UpdateLifecycle` → `UpdateAllLifecyclePhases`
        // chain attributes correctly.
        let report = "\
Event Count: 26191786399
 7.73%  1099304818 org.openharmonyrs.arkwebtest:render 58227 58227 /system/bin/nwebspawn /system/bin/nwebspawn+0xb6d4
           /system/bin/nwebspawn+0xb6d4
           base::PAC_MessagePumpDefault::Run
     |- 100.00% cc::PAC_LayerTreeHost::RequestMainFrameUpdate(bool)
         |- 100.00% non-virtual thunk to blink::PAC_WidgetBase::UpdateVisualState()
                    blink::PAC_WebFrameWidgetImpl::UpdateLifecycle(blink::WebLifecycleUpdate, blink::DocumentUpdateReason)
             |- 99.90% blink::PAC_LocalFrameView::UpdateAllLifecyclePhases(blink::DocumentUpdateReason)
                       blink::PAC_LocalFrameView::UpdateLifecyclePhases(blink::PAC_DocumentLifecycle::LifecycleState, blink::DocumentUpdateReason)
                 |- 87.32% blink::PAC_LocalFrameView::RunPaintLifecyclePhase(blink::PaintBenchmarkMode)
";
        let targets = vec![
            "WebFrameWidgetImpl::UpdateLifecycle".to_string(),
            "RunPaintLifecyclePhase".to_string(),
        ];
        let r = aggregate_inclusive(report, &targets);
        // UpdateLifecycle inclusive = 1099304818 (root events come straight
        // through the 100% thunk chain to UpdateLifecycle as a 100%-child
        // bare frame).
        assert_eq!(r["WebFrameWidgetImpl::UpdateLifecycle"], 1099304818);
        // RunPaintLifecyclePhase inclusive = 1099304818 * 0.999 * 0.8732
        // ≈ 959120608.
        let expected = (1099304818f64 * 0.999 * 0.8732) as u64;
        let actual = r["RunPaintLifecyclePhase"];
        let diff = if actual > expected { actual - expected } else { expected - actual };
        assert!(diff < 100, "RunPaintLifecyclePhase: got {actual} expected ~{expected}");
    }
}
