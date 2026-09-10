//! Cancellation for long device runs.
//!
//! A campaign is tens of minutes of blocking `hdc` calls, and until now Ctrl-C
//! did not end one: the signal killed the child `hdc` process, servoperf saw
//! that as a failed *iteration*, logged it and went on to the next — so the
//! run appeared to ignore Ctrl-C entirely.
//!
//! Now the first Ctrl-C asks for a stop, taken at the next iteration boundary
//! so the device is left in a known state and the results collected so far are
//! still written. A second Ctrl-C gives up waiting, but still runs the
//! registered cleanups first: abandoning a run must not leave the device with
//! its hitrace level raised and a wakelock held.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

static REQUESTED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type Cleanup = Box<dyn Fn() + Send + 'static>;
static CLEANUPS: Mutex<Vec<(u64, Cleanup)>> = Mutex::new(Vec::new());

/// Install the handler. Safe to call more than once; later calls are no-ops.
pub fn install() {
    let already = ctrlc::set_handler(|| {
        if REQUESTED.swap(true, Ordering::SeqCst) {
            eprintln!("\nservoperf: second interrupt — stopping now");
            run_cleanups();
            std::process::exit(130);
        }
        eprintln!(
            "\nservoperf: interrupt received; stopping after the current iteration so the \
             device is left in a known state and the results so far are written. \
             Press Ctrl-C again to stop immediately."
        );
    });
    if already.is_err() {
        // Another handler owns the signal (a test harness, or a second call).
        // Cancellation still works through `request()`.
    }
}

/// True once a stop has been asked for.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Ask for a stop from code rather than a signal.
pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Register device state to restore if the run is abandoned. The returned id
/// unregisters it once the owning guard has restored it itself.
pub fn register_cleanup(f: impl Fn() + Send + 'static) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut v) = CLEANUPS.lock() {
        v.push((id, Box::new(f)));
    }
    id
}

pub fn unregister_cleanup(id: u64) {
    if let Ok(mut v) = CLEANUPS.lock() {
        v.retain(|(i, _)| *i != id);
    }
}

fn run_cleanups() {
    let taken = match CLEANUPS.lock() {
        Ok(mut v) => std::mem::take(&mut *v),
        Err(_) => return,
    };
    for (_, f) in taken.iter().rev() {
        f();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanups_unregister() {
        let id = register_cleanup(|| {});
        let len_with = CLEANUPS.lock().unwrap().len();
        unregister_cleanup(id);
        let len_without = CLEANUPS.lock().unwrap().len();
        assert_eq!(len_without + 1, len_with);
    }

    #[test]
    fn request_sets_the_flag() {
        // Deliberately not asserting the initial value: another test in this
        // binary may have set it, and the flag is process-wide by design.
        request();
        assert!(requested());
        REQUESTED.store(false, Ordering::SeqCst);
    }
}
