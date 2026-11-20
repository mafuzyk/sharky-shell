//! Signal delivery for `trap`.
//!
//! A signal handler cannot run shell code: it may only touch state that is
//! async-signal-safe. The handler here records the signal number in one atomic
//! bitmask, and the run loop consumes that mask at command boundaries, which is
//! also when bash runs a trap — after the foreground command it interrupted has
//! finished. Deferral therefore falls out of the delivery design instead of
//! being emulated, and no trap can start while a child owns the terminal.
//!
//! Handlers are installed with `SA_RESTART` so a trap never turns a wait for a
//! child into a spurious failure: the wait restarts, the child keeps the
//! foreground process group, and the action runs after the wait returns.

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

/// Every signal `trap` accepts, by name and number, in number order. Bash also
/// accepts the real-time signals; sharky keeps the standard set.
pub const NAMES: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("SYS", 31),
];

/// Signals whose default action terminates the shell, and which therefore
/// cannot be caught at all: `trap` accepts the specification, as bash does, but
/// there is nothing to install.
pub fn cannot_catch(name: &str) -> bool {
    matches!(name, "KILL" | "STOP")
}

/// The name for a signal number, as `trap` spells it.
pub fn name(number: i32) -> Option<&'static str> {
    NAMES
        .iter()
        .find(|(_, candidate)| *candidate == number)
        .map(|(name, _)| *name)
}

/// The number for a bare signal name such as `USR1`.
pub fn number(name: &str) -> Option<i32> {
    NAMES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, number)| *number)
}

static PENDING: AtomicU64 = AtomicU64::new(0);

// SAFETY: the handler is asynchronous, so it only sets a bit in an atomic and
// returns. It never allocates, locks or calls into the shell.
extern "C" fn record(number: libc::c_int) {
    let shifted = u32::try_from(number).ok().and_then(|n| n.checked_sub(1));
    if let Some(shift) = shifted.filter(|shift| *shift < 64) {
        PENDING.fetch_or(1u64 << shift, Ordering::Relaxed);
    }
}

/// Collects and clears the signals delivered since the last call, lowest number
/// first. Delivery order is lost by the bitmask on purpose: two signals that
/// arrive before one safe point are both handled, and their order is not
/// observable by a script that is waiting for the command to finish.
pub fn take_pending() -> Vec<i32> {
    let bits = PENDING.swap(0, Ordering::Relaxed);
    (1..=64)
        .filter(|number| bits & (1u64 << (number - 1)) != 0)
        .collect()
}

/// Drops inherited pending signals; a forked child must not run the parent's
/// traps on the parent's deliveries.
pub fn clear_pending() {
    PENDING.store(0, Ordering::Relaxed);
}

/// A signal that asked the shell to abandon the command it is running. An
/// interactive shell cannot let SIGINT have its default action, and ignoring it
/// would make a builtin loop unstoppable, so the handler only records the
/// request and `run_list` breaks out at the next command boundary.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// SAFETY: the handler only stores into an atomic and returns.
extern "C" fn note_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// The interactive shell's baseline SIGINT action while it executes shell code.
/// The flag is checked between commands, and no `SA_RESTART` is requested so a
/// builtin blocked in `read` sees `EINTR` and can abort.
pub fn interruptible(number: i32) {
    install(number, SigHandler::Handler(note_interrupt), SaFlags::empty());
}

/// Records an interrupt the shell observed itself, such as a foreground child
/// killed by SIGINT, so the enclosing command list stops like a typed Ctrl-C.
pub fn mark_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn interrupt_pending() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

pub fn clear_interrupt() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

/// Upper bound on the job groups a hangup forwards to. A shell that keeps more
/// jobs than this still gets its own terminal restored; the extra ones are not
/// reachable from a signal handler, which may not allocate.
const MAX_PUBLISHED_JOBS: usize = 64;

static JOB_GROUPS: [AtomicI32; MAX_PUBLISHED_JOBS] =
    [const { AtomicI32::new(0) }; MAX_PUBLISHED_JOBS];
static JOB_GROUPS_LEN: AtomicUsize = AtomicUsize::new(0);

/// Publishes the process groups of the current job table for the hangup
/// handler. Called whenever the table changes.
pub fn publish_job_groups(groups: &[i32]) {
    let count = groups.len().min(MAX_PUBLISHED_JOBS);
    for (slot, group) in JOB_GROUPS.iter().zip(groups) {
        slot.store(*group, Ordering::Relaxed);
    }
    JOB_GROUPS_LEN.store(count, Ordering::Release);
}

/// Forwards a terminal hangup to every background job, resuming stopped ones so
/// they can act on it, exactly as bash does. Async-signal-safe: two `killpg`
/// calls per job and no allocation.
///
/// SAFETY: `killpg` is on the async-signal-safe list.
pub fn forward_hangup() {
    let count = JOB_GROUPS_LEN
        .load(Ordering::Acquire)
        .min(MAX_PUBLISHED_JOBS);
    for slot in &JOB_GROUPS[..count] {
        let group = slot.load(Ordering::Relaxed);
        if group > 0 {
            let _ = unsafe { libc::killpg(group, libc::SIGHUP) };
            let _ = unsafe { libc::killpg(group, libc::SIGCONT) };
        }
    }
}

fn install(number: i32, handler: SigHandler, flags: SaFlags) {
    let Ok(signal) = Signal::try_from(number) else {
        return;
    };
    let action = SigAction::new(handler, flags, SigSet::empty());
    // SAFETY: the handler is safe for any signal and the mask is empty, so no
    // code that touches the shell runs inside a handler.
    match unsafe { sigaction(signal, &action) } {
        Ok(_) | Err(Errno::EINVAL) => {}
        Err(_) => {}
    }
}

/// Runs the trap action instead of the default disposition.
pub fn catch(number: i32) {
    install(number, SigHandler::Handler(record), SaFlags::SA_RESTART);
}

/// `trap '' SIG`: ignore, and the disposition survives into child processes.
pub fn ignore(number: i32) {
    install(number, SigHandler::SigIgn, SaFlags::empty());
}

/// `trap - SIG`: back to the shell's own baseline for the signal.
pub fn reset(number: i32) {
    install(number, SigHandler::SigDfl, SaFlags::empty());
}

/// The interactive shell's fatal-signal handler, which restores the terminal
/// before dying. An interactive `trap - TERM` returns to this, not to SIG_DFL.
pub fn restore_terminal(number: i32) {
    install(
        number,
        SigHandler::Handler(crate::term::restore_terminal_and_exit),
        SaFlags::SA_RESTART,
    );
}
