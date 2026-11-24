//! Process-group supervision for `sharky agent --stdio` (design §8).
//!
//! This module is the only place the agent knows which process groups it
//! created and which request is running, so it is what `cancel`, `timeout_ms`
//! and the shutdown cleanup act on. While a `parallel` request runs, the same
//! state carries its task table (ids, pgids, per-task deadlines), which is what
//! makes a group cancel, a task cancel and the TERM→KILL escalation atomic with
//! the signalling (design §3.5/§5).
//!
//! Gating: outside a session `ACTIVE` is false, so [`note_group`],
//! [`child_guard`], [`service`], [`wait_poll`] and friends cost one relaxed
//! atomic load in the REPL, `-c`, scripts and batch stdin. That is the whole
//! reason the S1–S4 edits cannot change non-agent behavior.
//!
//! Fork discipline: [`child_guard`] runs in a freshly forked child before any
//! shell code. It must not touch the `Mutex` (the parent could have held it at
//! fork time), so it only flips the `ACTIVE` atomic and closes the recorded
//! control descriptors with plain `close(2)`. The flip is also what keeps a
//! subshell from pumping the protocol: its waits take the ordinary blocking
//! `waitpid` path, and the agent's own wait on the subshell is the one that
//! services cancellation.
//!
//! Cancellation is driven by the single thread that already owns every
//! `waitpid`: [`service`] is called from the session wait hook, so no second
//! reaper, no timer thread and no handler-driven interpreter re-entry is
//! introduced. The only signal-handler addition is one async-signal-safe
//! `write(2)` into a self-pipe from `signal::note_child`.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::os::fd::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::libc;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, pipe2};

use super::capture::write_safely;
use super::{Frame, FrameReader};
use crate::agent::json::{Ack, ErrorCode, ErrorResponse, Limits, Response, TaskReason};
use crate::agent::protocol::{self, CancelRequest};

/// Default grace between `SIGTERM` and `SIGKILL`, advertised in `ready`.
pub(crate) const CANCEL_GRACE_MS: i64 = 2000;

/// Default `close.wait_ms`/shutdown grace for recorded groups.
pub(crate) const CLEANUP_WAIT_MS: i64 = 2000;

/// Hard ceiling on a shutdown wait, so a client cannot pin the process.
const MAX_CLEANUP_WAIT_MS: i64 = 60_000;

/// Ceiling on `timeout_ms`; a huge value would only overflow `Instant`.
pub(crate) const MAX_TIMEOUT_MS: i64 = 86_400_000;

/// Recorded process groups per session (design §8.1).
const MAX_GROUPS: usize = 1024;

/// Frames read ahead during an `exec` and answered after it. The bound counts
/// what the queue already holds, not what one pump call reads, so a client
/// cannot grow it by writing during a long request.
const MAX_QUEUED_FRAMES: usize = 1024;

/// Maximum frames [`Supervisor::pump_input`] consumes in one wait-loop tick so
/// a sustained client flood cannot starve deadline checks, escalations or reap.
const MAX_PUMP_FRAMES_PER_TICK: usize = 256;

/// The self-pipe backstop: a missed wake-up still costs at most this long.
const SERVICE_POLL_MS: i32 = 50;

/// Session active and this process is the driver, not a forked child.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// A forked child closes these; recorded as numbers because `child_guard`
/// cannot lock or allocate. The two control descriptors come first, then the
/// two ends of the self-pipe: a child must inherit neither, and the pipe is the
/// session's own wake-up channel, not something a subshell may hold open.
static CONTROL_FDS: [AtomicI32; 8] = [const { AtomicI32::new(-1) }; 8];
static CONTROL_FDS_LEN: AtomicUsize = AtomicUsize::new(0);

static STATE: Mutex<Option<Supervisor>> = Mutex::new(None);

/// How the in-flight request ended when supervision, not the command, decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Cancelled,
    Timeout,
}

/// Which op owns a supervised [`ParallelGroup`] table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupKind {
    Parallel,
    Workflow,
}

/// Why a group stopped spawning (design §5 / workflow design §4.3).
/// `FailFast` and `FailurePolicy` are task-driven stops; the other two come
/// from the client or a deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    FailFast,
    FailurePolicy,
    CancelGroup,
    GroupTimeout,
}

impl StopReason {
    fn task_reason(self) -> TaskReason {
        match self {
            StopReason::FailFast => TaskReason::FailFast,
            StopReason::FailurePolicy => TaskReason::FailurePolicy,
            StopReason::CancelGroup => TaskReason::GroupCancel,
            StopReason::GroupTimeout => TaskReason::GroupTimeout,
        }
    }
}

/// How supervision ended one task, in the form the response reports it: the
/// verdict and, for a cancellation, why it happened (design §2.3/§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskStop {
    pub(crate) outcome: Outcome,
    pub(crate) reason: Option<TaskReason>,
}

impl TaskStop {
    fn cancelled(reason: TaskReason) -> Self {
        Self {
            outcome: Outcome::Cancelled,
            reason: Some(reason),
        }
    }

    /// A deadline (task or group) always reports `timeout`/124, and the reason
    /// vocabulary is reserved for tasks supervision cancelled (design §7).
    fn timed_out() -> Self {
        Self {
            outcome: Outcome::Timeout,
            reason: None,
        }
    }
}

/// One task's supervision state. The deadline counts from the task's own
/// spawn, so time spent queued never counts (design §2.1).
#[derive(Debug, Clone, Copy)]
enum TaskPhase {
    Queued,
    Running { pid: i32, deadline: Option<Instant> },
    Done,
}

/// One group task as the supervisor knows it: the id a `cancel` addresses,
/// its own deadline, the last spawned group leader, and the escalation that is
/// already in progress.
struct TaskSlot {
    id: String,
    timeout_ms: Option<i64>,
    phase: TaskPhase,
    last_pid: Option<i32>,
    /// Set once supervision ended the task; consumed by the driver so a task
    /// is labelled exactly once.
    stop: Option<TaskStop>,
    pending: Option<Pending>,
}

/// The table the single-request fields cannot describe (design §3.5). It is
/// what makes `cancel` of a group or of one task atomic with the signalling.
struct ParallelGroup {
    id: String,
    kind: GroupKind,
    tasks: Vec<TaskSlot>,
    group_timeout_ms: Option<i64>,
    /// Armed at the first spawn, for the same reason a task deadline is.
    group_deadline: Option<Instant>,
    stop: Option<StopReason>,
}

/// A termination already started, waiting for its escalation or for the child
/// to die. `term_sent` is separate from `pending` so a cancel recorded while the
/// request is between commands signals the group the request starts next.
struct Pending {
    verdict: Outcome,
    kill_at: Instant,
    term_sent: bool,
    kill_sent: bool,
}

/// A stop whose escalation outlives the leader it started with. A leader can be
/// reaped while descendants of the same process group are still alive (a
/// `SIGTERM`-ignoring grandchild, a background job), so the deadline is kept per
/// group until `killpg(pgid, 0)` reports the group empty — not per task slot,
/// which dies with the leader (design §8.2).
struct GroupStop {
    pgid: i32,
    kill_at: Instant,
    kill_sent: bool,
}

struct QueuedFrame {
    frame: Frame,
    id: Option<String>,
    op: Option<String>,
}

impl QueuedFrame {
    fn line(line: Vec<u8>, id: Option<String>, op: Option<String>) -> Self {
        Self {
            frame: Frame::Line(line),
            id,
            op,
        }
    }

    fn other(frame: Frame) -> Self {
        Self {
            frame,
            id: None,
            op: None,
        }
    }
}

struct Supervisor {
    input: RawFd,
    output: RawFd,
    pipe_read: RawFd,
    pipe_write: RawFd,
    limits: Limits,
    frames: FrameReader,
    /// Frames read while an `exec` was running, answered after it.
    queued: VecDeque<QueuedFrame>,
    /// The one terminal frame the queue could not take because it was full: a
    /// `close` the driver must still handle, or the last `Eof` with a partial
    /// line. Bounded to one so a full queue cannot grow through it.
    pinned: Option<QueuedFrame>,
    /// A read failure observed while pumping; surfaced to the driver next.
    read_error: Option<String>,
    /// Groups recorded for the session, oldest first, deduplicated.
    groups: Vec<i32>,
    /// Escalations still owed to a group whose leader was already reaped.
    stops: Vec<GroupStop>,
    /// Group of the foreground command that is running right now, 0 when
    /// there is none. `cancel` signals this, never the agent's own group.
    current: i32,
    /// First foreground group of the request, reported as `result.pgid`.
    request_group: i32,
    in_flight: Option<String>,
    timeout_at: Option<Instant>,
    pending: Option<Pending>,
    /// The running parallel group, if the in-flight request is one (design
    /// §3.5). `current` is forced to 0 while it exists.
    parallel: Option<ParallelGroup>,
    /// Every id that named a `parallel` or `workflow` request, so a `cancel`
    /// carrying `task` can tell "a finished group" from "an `exec` request".
    group_seen: HashSet<String>,
    /// Queued requests cancelled before they started.
    cancelled_queued: HashSet<String>,
    /// Every id the connection accepted: duplicate detection and the
    /// `CANCEL_TOO_LATE`/`CANCEL_UNKNOWN_ID` distinction.
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    eof: bool,
    client_gone: bool,
    ended: bool,
}

fn state() -> MutexGuard<'static, Option<Supervisor>> {
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn with_state<R>(f: impl FnOnce(&mut Supervisor) -> R) -> Option<R> {
    let mut guard = state();
    guard.as_mut().map(f)
}

// ------------------------------------------------------------------ lifecycle

/// Installs the session supervisor. Called once, after the control channel has
/// been moved to its high descriptors and before any shell code runs.
pub(crate) fn install(input: RawFd, output: RawFd, limits: Limits) -> io::Result<()> {
    let (read, write) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).map_err(errno_to_io)?;
    let pipe_read = read.into_raw_fd();
    let pipe_write = write.into_raw_fd();

    *state() = Some(Supervisor {
        input,
        output,
        pipe_read,
        pipe_write,
        limits,
        frames: FrameReader::new(limits.max_line_bytes),
        queued: VecDeque::new(),
        pinned: None,
        read_error: None,
        groups: Vec::new(),
        stops: Vec::new(),
        current: 0,
        request_group: 0,
        in_flight: None,
        timeout_at: None,
        pending: None,
        parallel: None,
        group_seen: HashSet::new(),
        cancelled_queued: HashSet::new(),
        seen: HashSet::new(),
        seen_order: VecDeque::new(),
        eof: false,
        client_gone: false,
        ended: false,
    });

    for (slot, fd) in CONTROL_FDS
        .iter()
        .zip([input, output, pipe_read, pipe_write])
    {
        slot.store(fd, Ordering::Relaxed);
    }
    CONTROL_FDS_LEN.store(4, Ordering::Release);
    crate::signal::set_child_pipe(pipe_write);
    ACTIVE.store(true, Ordering::SeqCst);
    Ok(())
}

/// Registers extra parent-only control descriptors to close in forked children.
pub(crate) fn close_in_children(fds: &[RawFd]) {
    let mut len = CONTROL_FDS_LEN.load(Ordering::Acquire);
    for &fd in fds {
        if fd >= 0 && len < CONTROL_FDS.len() {
            CONTROL_FDS[len].store(fd, Ordering::Relaxed);
            len += 1;
        }
    }
    CONTROL_FDS_LEN.store(len, Ordering::Release);
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

/// One relaxed load; the wait hook's only cost outside a session.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

// --------------------------------------------------------------- group tracking

/// Whether one more group fits the session registry, asked in the spawn window
/// before the fork. A `false` answer means the task must not spawn at all: a
/// group the session cannot record could outlive its cleanup (design §6.6), so
/// the driver answers the task `spawn_error`/`126` with a visible message
/// instead of forking a process nobody can stop.
///
/// The probe compacts groups that no longer exist, exactly as an insertion at
/// the bound does, so only a registry full of live groups refuses. A group that
/// registers between the probe and the fork (an `exec` or a background job) can
/// still lose the race; [`Supervisor::add_group`] keeps the kill-the-untracked
/// group defense for that case.
pub(crate) fn can_track_group() -> bool {
    if !active() {
        return true;
    }
    // Test-only: a registry full of live groups cannot be built honestly in a
    // test, so the refusal path is exercised through this hook (debug builds
    // only, never in a release binary).
    #[cfg(debug_assertions)]
    if std::env::var_os("SHARKY_PARALLEL_TEST_REGISTRY_FULL").is_some() {
        return false;
    }
    with_state(|state| can_insert_group(&mut state.groups, MAX_GROUPS)).unwrap_or(true)
}

/// Records a background job's group for shutdown cleanup.
pub(crate) fn note_group(pgid: i32) {
    if pgid <= 0 || !active() {
        return;
    }
    with_state(|state| state.add_group(pgid));
}

/// Records the foreground group of the request and makes it the group `cancel`
/// signals. Called from the parent after `setpgid` created the group.
pub(crate) fn note_foreground(pgid: i32) {
    if pgid <= 0 || !active() {
        return;
    }
    with_state(|state| {
        state.add_group(pgid);
        state.current = pgid;
        if state.request_group == 0 {
            state.request_group = pgid;
        }
        // A cancel that arrived between two commands now has a group to stop.
        let arm = match state.pending.as_mut() {
            Some(pending) if !pending.term_sent => {
                pending.term_sent = true;
                signal(pgid, libc::SIGTERM);
                Some(pending.kill_at)
            }
            _ => None,
        };
        if let Some(kill_at) = arm {
            state.note_group_stop(pgid, kill_at);
        }
    });
}

/// The foreground group finished; `cancel` must not signal it any more.
pub(crate) fn clear_foreground() {
    if !active() {
        return;
    }
    with_state(|state| state.current = 0);
}

/// The foreground group of the finished request, for `result.pgid`.
pub(crate) fn request_group() -> Option<i64> {
    if !active() {
        return None;
    }
    with_state(|state| (state.request_group > 0).then_some(i64::from(state.request_group)))
        .flatten()
}

impl Supervisor {
    /// Records a group for cleanup.
    fn add_group(&mut self, pgid: i32) {
        if insert_group(&mut self.groups, pgid, MAX_GROUPS) == Registered::Refused {
            // The driver asks [`can_track_group`] before forking a task, so a
            // refusal here is a race with another fork site (an `exec` or a
            // background job that registered in between). An untracked group is
            // exactly what could outlive the session's cleanup (design §6.6),
            // so it is killed rather than forgotten: the bound stays a refusal.
            signal(pgid, libc::SIGKILL);
        }
    }

    /// Drops groups that no longer exist, so a recycled pgid is not signalled
    /// long after the job that owned it is gone.
    fn purge_dead_groups(&mut self) {
        self.groups.retain(|group| group_alive(*group));
    }
}

/// What [`insert_group`] did with a pgid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Registered {
    /// Already in the registry; nothing changed.
    Known,
    /// Added, possibly after dropping entries whose group no longer exists.
    Added,
    /// The bound is reached and every recorded group is still alive.
    Refused,
}

/// Inserts `pgid` into `groups` without ever evicting a live group.
///
/// At the bound the dead entries are compacted first, because a group whose
/// pgid answers `ESRCH` owes nothing to cleanup. If every entry is still alive
/// the insertion is refused: the registry stays a superset of the live groups,
/// which is what makes the shutdown cleanup complete (design §6.6). The old
/// policy dropped `groups[0]`, so a long-lived group could be forgotten while
/// the session kept running and survive the cleanup entirely.
fn insert_group(groups: &mut Vec<i32>, pgid: i32, limit: usize) -> Registered {
    if groups.contains(&pgid) {
        return Registered::Known;
    }
    if !can_insert_group(groups, limit) {
        return Registered::Refused;
    }
    groups.push(pgid);
    Registered::Added
}

/// Makes room at the bound by dropping groups that no longer exist, and reports
/// whether the registry can still take one. This is the probe the spawn window
/// uses ([`can_track_group`]) and the first half of [`insert_group`], so the
/// answer before a fork and the insertion after it agree on what "full" means.
fn can_insert_group(groups: &mut Vec<i32>, limit: usize) -> bool {
    if groups.len() >= limit {
        groups.retain(|group| group_alive(*group));
    }
    groups.len() < limit
}

// -------------------------------------------------------------------- fork guard

/// Runs in a forked child beside `reset_child_traps`.
///
/// Closes the control descriptors so a subshell cannot write into the protocol
/// by accident, and turns the supervisor off for the child so its waits take
/// the ordinary blocking path instead of pumping the parent's channel. No-op
/// outside a session and on the second call in the same child.
pub(crate) fn child_guard() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    crate::signal::set_child_pipe(-1);
    let len = CONTROL_FDS_LEN
        .load(Ordering::Acquire)
        .min(CONTROL_FDS.len());
    for slot in &CONTROL_FDS[..len] {
        let fd = slot.load(Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: the descriptor belongs to this child's table, the session
            // recorded it, and `close` is async-signal-safe.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

// ------------------------------------------------------------------ wait hook

/// The pump the session wait hook runs while a child is alive: read what the
/// client sent without blocking (a `cancel` is answered out of order; every
/// other frame is queued for the driver), then enforce the deadlines.
pub(crate) fn service() {
    if !active() {
        return;
    }
    with_state(|state| {
        state.pump_input();
        state.check_deadlines();
        state.check_group_stops();
    });
}

/// Sleeps until the child self-pipe, the control channel or the 50 ms backstop
/// wakes the wait loop. Without the self-pipe every command would pay the whole
/// timeout; with it, a child state change returns immediately.
pub(crate) fn wait_poll() {
    wait_poll_inner(None);
}

/// Same as [`wait_poll`], plus one extra descriptor watched for `POLLIN` (such
/// as the read end of an in-flight command substitution pipe).
pub(crate) fn wait_poll_fd(extra: RawFd) {
    wait_poll_inner(Some(extra));
}

fn wait_poll_inner(extra: Option<RawFd>) {
    if !active() || crate::signal::fatal_signal().is_some() {
        return;
    }
    let Some((pipe_read, input)) = with_state(|state| {
        if !state.eof && state.frames.has_buffered_frame() {
            return None;
        }
        Some((state.pipe_read, state.input))
    })
    .flatten()
    else {
        return;
    };
    let mut descriptors = [
        libc::pollfd {
            fd: pipe_read,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: input,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: extra.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let nfds: libc::nfds_t = if extra.is_some() { 3 } else { 2 };
    // SAFETY: the polled descriptors are owned by this process and `nfds`
    // matches the initialized slice prefix of `descriptors`.
    unsafe {
        libc::poll(descriptors.as_mut_ptr(), nfds, SERVICE_POLL_MS);
    }

    // Drain the self-pipe so the next poll can sleep again. It is O_NONBLOCK,
    // so this stops at EAGAIN; partial writes are irrelevant, the byte is a
    // wake-up and not a message.
    let mut buffer = [0u8; 64];
    loop {
        // SAFETY: the buffer is valid for writes of its own length.
        let count = unsafe {
            libc::read(
                pipe_read,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
            )
        };
        if count <= 0 {
            break;
        }
    }
}

impl Supervisor {
    fn check_deadlines(&mut self) {
        if self.parallel.is_some() {
            self.check_parallel_deadlines();
            return;
        }
        let now = Instant::now();
        if self.timeout_at.is_some_and(|at| now >= at) {
            self.timeout_at = None;
            if self.pending.is_none() {
                self.pending = Some(Pending {
                    verdict: Outcome::Timeout,
                    kill_at: now + grace(),
                    term_sent: false,
                    kill_sent: false,
                });
            }
        }

        let mut term = None;
        let mut kill = false;
        let current = self.current;
        if let Some(pending) = self.pending.as_mut() {
            if !pending.term_sent && current > 0 {
                pending.term_sent = true;
                term = Some((current, pending.kill_at));
            } else if pending.term_sent && !pending.kill_sent && now >= pending.kill_at {
                pending.kill_sent = true;
                kill = true;
            }
        }
        if let Some((group, kill_at)) = term {
            signal(group, libc::SIGTERM);
            // The group may outlive the leader, so the deadline is armed per
            // group from the moment the stop starts.
            self.note_group_stop(group, kill_at);
        }
        if kill {
            signal(current, libc::SIGKILL);
        }
    }

    /// The parallel branch of [`Supervisor::check_deadlines`] (design §3.5): the
    /// group deadline stops spawning, times out every running task and cancels
    /// what never started; each task's own deadline times out just that task;
    /// and every armed `pending` escalates `SIGTERM` → `SIGKILL` on its own
    /// pgid, bounded by the grace. The single-request path cannot fire while a
    /// group exists because `current` is 0.
    fn check_parallel_deadlines(&mut self) {
        let now = Instant::now();
        let wait = grace();
        let Some(group) = self.parallel.as_mut() else {
            return;
        };

        if group.stop.is_none()
            && let Some(deadline) = group.group_deadline
            && now >= deadline
        {
            group.stop = Some(StopReason::GroupTimeout);
            for slot in &mut group.tasks {
                match slot.phase {
                    TaskPhase::Queued => {
                        arm_queued(slot, TaskStop::cancelled(TaskReason::GroupTimeout));
                    }
                    TaskPhase::Running { pid, .. } => {
                        arm_running(slot, pid, TaskStop::timed_out(), now, wait);
                    }
                    TaskPhase::Done => {}
                }
            }
        }

        for slot in &mut group.tasks {
            if let TaskPhase::Running {
                pid,
                deadline: Some(at),
            } = slot.phase
                && now >= at
            {
                arm_running(slot, pid, TaskStop::timed_out(), now, wait);
            }
        }

        for slot in &mut group.tasks {
            let TaskPhase::Running { pid, .. } = slot.phase else {
                continue;
            };
            let Some(pending) = slot.pending.as_mut() else {
                continue;
            };
            if pending.term_sent && !pending.kill_sent && now >= pending.kill_at {
                pending.kill_sent = true;
                signal(pid, libc::SIGKILL);
            }
        }
    }

    /// Stops the whole group for a reason that is not a `cancel` frame: a
    /// `close` or EOF seen while the group runs (design §5). The driver still
    /// answers the group, then the queued `close` frame is handled normally.
    fn stop_parallel_group(&mut self, reason: StopReason, wait_ms: Option<i64>) {
        let now = Instant::now();
        let wait = wait_ms
            .map(|ms| Duration::from_millis(ms.clamp(0, MAX_CLEANUP_WAIT_MS) as u64))
            .unwrap_or_else(grace);
        let Some(group) = self.parallel.as_mut() else {
            return;
        };
        group.stop.get_or_insert(reason);
        for slot in &mut group.tasks {
            match slot.phase {
                TaskPhase::Queued => arm_queued(slot, TaskStop::cancelled(reason.task_reason())),
                TaskPhase::Running { pid, .. } => {
                    arm_running(
                        slot,
                        pid,
                        TaskStop::cancelled(reason.task_reason()),
                        now,
                        wait,
                    );
                }
                TaskPhase::Done => {}
            }
        }
    }

    // ---------------------------------------------------- group stops

    /// Arms the group-level escalation of a stop that was already signalled
    /// (`SIGTERM` to the group happened before this call). First stop wins and
    /// a group with no member left arms nothing.
    fn note_group_stop(&mut self, pgid: i32, kill_at: Instant) {
        if pgid <= 0 || !group_alive(pgid) {
            return;
        }
        if self.stops.iter().any(|stop| stop.pgid == pgid) {
            return;
        }
        self.stops.push(GroupStop {
            pgid,
            kill_at,
            kill_sent: false,
        });
    }

    /// Escalates every armed stop whose deadline passed and forgets the groups
    /// that are provably empty. `killpg(pgid, 0)` answers `ESRCH` only when the
    /// group has no member left — the leader being reaped is not that moment.
    fn check_group_stops(&mut self) {
        if self.stops.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut index = 0;
        while index < self.stops.len() {
            let pgid = self.stops[index].pgid;
            if !group_alive(pgid) {
                self.stops.swap_remove(index);
                continue;
            }
            let stop = &mut self.stops[index];
            if !stop.kill_sent && now >= stop.kill_at {
                stop.kill_sent = true;
                signal(pgid, libc::SIGKILL);
            }
            index += 1;
        }
    }

    /// Waits for every owed stop to settle, bounded by its own deadline, so the
    /// response that reports a cancel is not written while a member of the
    /// cancelled group can still be alive. Called once when a request ends;
    /// a request that armed nothing returns immediately.
    fn settle_group_stops(&mut self) {
        if self.stops.is_empty() {
            return;
        }
        let latest = self.stops.iter().map(|stop| stop.kill_at).max();
        // `SIGKILL` cannot be ignored, so one poll interval past the last
        // deadline is enough for the kernel to take the group down.
        let deadline = latest.map_or_else(Instant::now, |at| at + Duration::from_millis(200));
        loop {
            if crate::signal::fatal_signal().is_some() {
                return;
            }
            self.check_group_stops();
            if self.stops.is_empty() || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Moves the escalation of every still-running task slot into the group
    /// list before the task table is dropped. `finish_task` does the same for a
    /// task it reaped; this covers a slot that is still running when the group
    /// ends (a fatal signal, a capture failure), so no armed `SIGKILL` is lost
    /// with the slot.
    fn drain_running_stops(&mut self) {
        let mut armed = Vec::new();
        if let Some(group) = self.parallel.as_mut() {
            for slot in &mut group.tasks {
                if let TaskPhase::Running { pid, .. } = slot.phase
                    && let Some(pending) = slot.pending.take()
                    && !pending.kill_sent
                {
                    armed.push((pid, pending.kill_at));
                }
            }
        }
        for (pgid, kill_at) in armed {
            self.note_group_stop(pgid, kill_at);
        }
    }
}

/// Records the stop of a task that has not started; the first reason wins, so
/// a repeated cancel is idempotent and never rewrites the label.
fn arm_queued(slot: &mut TaskSlot, stop: TaskStop) {
    if slot.stop.is_none() {
        slot.stop = Some(stop);
    }
}

/// Starts (or leaves alone) the termination of a running task: the stop is
/// recorded once, `SIGTERM` is sent once, and the escalation is bounded by the
/// grace measured from this moment.
fn arm_running(slot: &mut TaskSlot, pid: i32, stop: TaskStop, now: Instant, wait: Duration) {
    if slot.stop.is_none() {
        slot.stop = Some(stop);
    }
    if slot.pending.is_none() {
        slot.pending = Some(Pending {
            verdict: stop.outcome,
            kill_at: now + wait,
            term_sent: true,
            kill_sent: false,
        });
        signal(pid, libc::SIGTERM);
    }
}

fn milliseconds(ms: i64) -> Duration {
    Duration::from_millis(ms.clamp(0, MAX_TIMEOUT_MS) as u64)
}

fn grace() -> Duration {
    Duration::from_millis(CANCEL_GRACE_MS as u64)
}

fn signal(pgid: i32, number: i32) {
    if pgid <= 0 {
        return;
    }
    // SAFETY: `killpg` on a positive group only reports to the kernel.
    unsafe {
        libc::killpg(pgid, number);
    }
}

pub(crate) fn group_is_alive(pgid: i32) -> bool {
    group_alive(pgid)
}

fn group_alive(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs the existence check without delivering.
    let result = unsafe { libc::killpg(pgid, 0) };
    result == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ------------------------------------------------------------------ request state

/// Marks a request as the in-flight one and arms its deadline.
pub(crate) fn begin_request(id: &str, timeout_ms: Option<i64>) {
    if !active() {
        return;
    }
    with_state(|state| {
        state.in_flight = Some(id.to_string());
        state.current = 0;
        state.request_group = 0;
        state.pending = None;
        state.timeout_at = timeout_ms.map(|ms| {
            let capped = ms.clamp(0, MAX_TIMEOUT_MS) as u64;
            Instant::now() + Duration::from_millis(capped)
        });
        // An `exec` never inherits a parallel table; the group path clears its
        // own when it ends, and this is the defensive half of that contract.
        state.parallel = None;
        state.purge_dead_groups();
    });
}

/// Ends the in-flight request and reports whether supervision decided how it
/// should be reported.
pub(crate) fn end_request() -> Option<Outcome> {
    if !active() {
        return None;
    }
    with_state(|state| {
        let outcome = state.pending.take().map(|pending| pending.verdict);
        state.in_flight = None;
        state.current = 0;
        state.timeout_at = None;
        // The response must not say `cancelled` while a member of the group is
        // still alive: the escalation is owed to the group, not to the leader.
        state.settle_group_stops();
        outcome
    })
    .flatten()
}

/// Whether a queued request was cancelled before it started. The driver answers
/// it with `termination:"cancelled"`, `status:130` and runs nothing.
pub(crate) fn take_queued_cancelled(id: &str) -> bool {
    if !active() {
        return false;
    }
    with_state(|state| state.cancelled_queued.remove(id)).unwrap_or(false)
}

/// Returns any cancellation or timeout verdict already pending on the
/// in-flight request without clearing it.
pub(crate) fn pending_outcome() -> Option<Outcome> {
    if !active() {
        return None;
    }
    with_state(|state| state.pending.as_ref().map(|pending| pending.verdict)).flatten()
}

// -------------------------------------------------------------- parallel table

/// Installs the table for a `parallel` request before its first spawn.
pub(crate) fn begin_parallel(id: &str, tasks: &[(String, Option<i64>)], timeout_ms: Option<i64>) {
    begin_group(id, GroupKind::Parallel, tasks, timeout_ms);
}

/// Installs the table for a `parallel` or `workflow` request before its first
/// spawn. The group id becomes the in-flight request, so a `cancel` addressed
/// to it is no longer answered `CANCEL_TOO_LATE` (design §3.5/§5).
pub(crate) fn begin_group(
    id: &str,
    kind: GroupKind,
    tasks: &[(String, Option<i64>)],
    timeout_ms: Option<i64>,
) {
    if !active() {
        return;
    }
    with_state(|state| {
        state.in_flight = Some(id.to_string());
        state.current = 0;
        state.request_group = 0;
        state.pending = None;
        state.timeout_at = None;
        state.group_seen.insert(id.to_string());
        state.parallel = Some(ParallelGroup {
            id: id.to_string(),
            kind,
            tasks: tasks
                .iter()
                .map(|(id, timeout_ms)| TaskSlot {
                    id: id.clone(),
                    timeout_ms: *timeout_ms,
                    phase: TaskPhase::Queued,
                    last_pid: None,
                    stop: None,
                    pending: None,
                })
                .collect(),
            group_timeout_ms: timeout_ms,
            group_deadline: None,
            stop: None,
        });
        state.purge_dead_groups();
    });
}

/// Records one spawned task: its process group goes into the cleanup set, the
/// group clock starts at the first spawn, and its own deadline starts now
/// (design §2.1/§3.5). Called by the driver right after `fork` and `setpgid`.
pub(crate) fn task_running(index: usize, pid: i32) {
    if pid <= 0 || !active() {
        return;
    }
    with_state(|state| {
        state.add_group(pid);
        let Some(group) = state.parallel.as_mut() else {
            return;
        };
        let now = Instant::now();
        if group.group_deadline.is_none() {
            group.group_deadline = group.group_timeout_ms.map(|ms| now + milliseconds(ms));
        }
        if let Some(slot) = group.tasks.get_mut(index)
            && slot.stop.is_none()
            && matches!(slot.phase, TaskPhase::Queued)
        {
            slot.last_pid = Some(pid);
            slot.phase = TaskPhase::Running {
                pid,
                deadline: slot.timeout_ms.map(|ms| now + milliseconds(ms)),
            };
        }
    });
}

/// Whether the group stopped spawning (cancelled or out of time). The driver
/// keeps the same window loop and only consults this between spawns.
pub(crate) fn parallel_stopped() -> Option<StopReason> {
    if !active() {
        return None;
    }
    with_state(|state| state.parallel.as_ref().and_then(|group| group.stop)).flatten()
}

/// Records a group-wide stop decided by the driver. `fail-fast` uses it when a
/// task finishes with a real failure: queued tasks get `cancelled`/`130` with
/// the matching reason, and running tasks get the ordinary TERM→KILL escalation
/// (design §4/§5/§7).
///
/// The first stop wins everywhere — on the group and on every slot — so a
/// `fail-fast` trigger after a group cancel or deadline cannot rewrite their
/// verdicts, and a repeated trigger is a no-op.
pub(crate) fn record_stop(reason: StopReason) {
    if !active() {
        return;
    }
    with_state(|state| state.stop_parallel_group(reason, None));
}

/// Whether any deadline is armed. The driver polls instead of blocking so the
/// supervisor can escalate a `SIGTERM`-ignoring task (design §5).
pub(crate) fn parallel_deadline_armed() -> bool {
    if !active() {
        return false;
    }
    with_state(|state| {
        state.parallel.as_ref().is_some_and(|group| {
            group.group_deadline.is_some()
                || group
                    .tasks
                    .iter()
                    .any(|slot| slot.timeout_ms.is_some())
        })
    })
    .unwrap_or(false)
}

impl ParallelGroup {
    /// Ends one task's supervision without a pid: either it never got one (the
    /// parent could not spawn it), it was skipped by graph rules, or its status
    /// is gone. What is left is a final task, so no `cancel` can arm a stop for
    /// it and no deadline can fire.
    fn abandon(&mut self, index: usize) {
        if let Some(slot) = self.tasks.get_mut(index) {
            slot.phase = TaskPhase::Done;
            slot.stop = None;
            slot.pending = None;
        }
    }

    /// Transitions a finished workflow slot back to `Queued` for another
    /// attempt (workflow design §4.5/§13). Refuses parallel groups, stopped
    /// groups, or slots that are not `Done`. Returns the prior attempt's pid so
    /// the driver can wait until its process group is empty before respawning.
    fn retry_slot(&mut self, index: usize) -> Option<Option<i32>> {
        if self.kind != GroupKind::Workflow || self.stop.is_some() {
            return None;
        }
        let slot = self.tasks.get_mut(index)?;
        if !matches!(slot.phase, TaskPhase::Done) {
            return None;
        }
        slot.phase = TaskPhase::Queued;
        slot.stop = None;
        slot.pending = None;
        Some(slot.last_pid.take())
    }
}

/// Marks a task the driver could not fork as Done. A spawn failure is final, so
/// a later `cancel` for it must be answered `CANCEL_TOO_LATE`, not `delivered`
/// with a stop nobody would ever read (design §5).
pub(crate) fn spawn_failed(index: usize) {
    abandon_task(index);
}

/// Marks a task that finished without running (`spawn_error`, `skipped`,
/// `blocked`) as Done in the supervisor table.
pub(crate) fn abandon_task(index: usize) {
    if !active() {
        return;
    }
    with_state(|state| {
        let Some(group) = state.parallel.as_mut() else {
            return;
        };
        group.abandon(index);
    });
}

/// Moves a finished workflow slot back to `Queued` so it may retry without
/// losing its identity for `cancel` or inheriting stale escalation state
/// (workflow design §4.5). Returns `Some(prev_pid)` when accepted, `None` when
/// refused.
pub(crate) fn task_retry(index: usize) -> Option<Option<i32>> {
    if !active() {
        return None;
    }
    with_state(|state| state.parallel.as_mut()?.retry_slot(index)).flatten()
}

/// Takes the stop of a task that never started (a queued cancel, a group
/// cancel, or the group deadline). The driver answers it without spawning.
pub(crate) fn take_task_stop(index: usize) -> Option<TaskStop> {
    if !active() {
        return None;
    }
    with_state(|state| {
        let group = state.parallel.as_mut()?;
        let slot = group.tasks.get_mut(index)?;
        if !matches!(slot.phase, TaskPhase::Queued) {
            return None;
        }
        let stop = slot.stop.take()?;
        slot.phase = TaskPhase::Done;
        Some(stop)
    })
    .flatten()
}

/// Ends one reaped task's supervision and returns the verdict supervision had
/// already decided for it, if any. The driver calls this exactly once per
/// status it reaps, which is what keeps "labelled once" true.
pub(crate) fn finish_task(index: usize, pid: i32) -> Option<TaskStop> {
    if !active() {
        return None;
    }
    with_state(|state| {
        let (stop, arming) = {
            let group = state.parallel.as_mut()?;
            let slot = group.tasks.get_mut(index)?;
            match slot.phase {
                TaskPhase::Running { pid: running, .. } if running == pid => {
                    slot.phase = TaskPhase::Done;
                    // The leader is gone, but the group may not be: a descendant
                    // that survives `SIGTERM` still owes the `SIGKILL` at the
                    // same deadline (design §8.2). Reaping the leader must not
                    // abandon it, so the escalation moves to the group list.
                    let arming = slot
                        .pending
                        .take()
                        .filter(|pending| !pending.kill_sent)
                        .map(|pending| pending.kill_at);
                    (slot.stop.take(), arming)
                }
                _ => return None,
            }
        };
        if let Some(kill_at) = arming {
            state.note_group_stop(pid, kill_at);
        }
        stop
    })
    .flatten()
}

/// Ends the group: the id is no longer cancellable, and the single-request
/// escalation path is free again. Called after the last reap, before the
/// response is written (design §3.5).
pub(crate) fn end_parallel() {
    if !active() {
        return;
    }
    with_state(|state| {
        state.drain_running_stops();
        state.parallel = None;
        state.in_flight = None;
        state.current = 0;
        state.timeout_at = None;
        state.pending = None;
        state.settle_group_stops();
    });
}

/// `cancel` from the driver (session idle). Returns false when the ack could
/// not be written, which means the client is gone.
pub(crate) fn cancel_op(request: &CancelRequest) -> bool {
    if !active() {
        return false;
    }
    with_state(|state| {
        state.handle_cancel(
            request.id.as_str(),
            &request.target,
            request.task.as_deref(),
            request.grace_ms,
        )
    })
    .unwrap_or(false)
}

// ------------------------------------------------------------- id bookkeeping

pub(crate) fn id_seen(id: &str) -> bool {
    with_state(|state| state.seen.contains(id)).unwrap_or(false)
}

pub(crate) fn touch_id(id: &str) {
    with_state(|state| state.touch(id));
}

// --------------------------------------------------------------- frame reading

/// Next frame the driver must handle: whatever the service pump queued first,
/// then a blocking read (design §3.6 keeps response order).
pub(crate) fn next_frame() -> io::Result<Frame> {
    let mut guard = state();
    let Some(state) = guard.as_mut() else {
        return Ok(Frame::Eof { partial: None });
    };
    if let Some(entry) = state.queued.pop_front() {
        return Ok(entry.frame);
    }
    if let Some(entry) = state.pinned.take() {
        return Ok(entry.frame);
    }
    if let Some(error) = state.read_error.take() {
        return Err(io::Error::other(error));
    }
    if state.eof {
        return Ok(Frame::Eof { partial: None });
    }
    let input = state.input;
    state.frames.next(input)
}

impl Supervisor {
    /// Reads what the client sent without blocking, bounded by what the queue
    /// can still hold (design §3.6). A full queue is back-pressure for frames
    /// nobody can answer yet, but not for the ones the protocol promises out of
    /// order: a `cancel` is still read and answered, a valid `close` is still
    /// read (pinned, at most one, since it ends the session), and any other
    /// frame is rejected on the spot instead of being queued, so a client
    /// cannot grow the queue with the length of a request.
    fn pump_input(&mut self) {
        let mut processed = 0usize;
        while !self.eof && processed < MAX_PUMP_FRAMES_PER_TICK {
            let full = self.queued.len() >= MAX_QUEUED_FRAMES;
            match self.frames.next_nonblocking(self.input) {
                Ok(Some(Frame::Line(line))) => {
                    processed += 1;
                    self.consume_line(line, full);
                }
                Ok(Some(frame)) => {
                    processed += 1;
                    let eof = matches!(frame, Frame::Eof { .. });
                    if eof {
                        self.eof = true;
                        // EOF during a group: nobody can answer it any more,
                        // but every task group still has to die (design §5).
                        self.stop_parallel_group(StopReason::CancelGroup, None);
                    }
                    if !full {
                        self.queued.push_back(QueuedFrame::other(frame));
                    } else if eof {
                        // The trailing bytes of an unterminated request are a
                        // request of their own, so `Eof` keeps the one slot that
                        // survives a full queue: the session must still see it.
                        if self.pinned.is_none() {
                            self.pinned = Some(QueuedFrame::other(frame));
                        }
                    } else {
                        // A line discarded for its size is not a request, and
                        // the driver would answer it out of order anyway. The
                        // pinned slot stays free for the frame that ends the
                        // session.
                        let message =
                            format!("request line exceeds {} bytes", self.limits.max_line_bytes);
                        self.write_error(None, ErrorCode::MessageTooLarge, message);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    self.eof = true;
                    self.read_error = Some(error.to_string());
                    self.stop_parallel_group(StopReason::CancelGroup, None);
                    break;
                }
            }
        }
    }

    /// Answers a frame the queue cannot take, so the client learns why its
    /// request was not served instead of waiting for an answer that would only
    /// come after a long request. The id is not recorded, so the request may be
    /// repeated once the queue drains; the error is not fatal.
    fn reject_overflow(&mut self, line: &[u8]) {
        match crate::agent::json::parse_line(line) {
            Ok(value) => {
                let id = protocol::request_id(&value, &self.limits);
                self.write_error(
                    id.as_deref(),
                    ErrorCode::QueueFull,
                    format!(
                        "the pending request queue is full ({MAX_QUEUED_FRAMES} frames in flight); retry after the running request is answered"
                    ),
                );
            }
            Err(error) => {
                self.write_error(None, ErrorCode::InvalidJson, error.to_string());
            }
        }
    }

    /// Queues one line for the driver, or, when the bounded queue is full,
    /// keeps the single frame the driver must still see (`control`) and rejects
    /// the rest. Every line the pump reads ends here, which is what makes the
    /// bound hold across calls.
    fn defer_line(
        &mut self,
        line: Vec<u8>,
        full: bool,
        control: bool,
        id: Option<String>,
        op: Option<String>,
    ) {
        if !full {
            self.queued.push_back(QueuedFrame::line(line, id, op));
            return;
        }
        if control && self.pinned.is_none() {
            self.pinned = Some(QueuedFrame::line(line, id, op));
            return;
        }
        self.reject_overflow(&line);
    }

    fn is_queued_id(&self, id: &str) -> bool {
        self.queued
            .iter()
            .chain(self.pinned.iter())
            .any(|entry| entry.id.as_deref() == Some(id))
    }

    /// Handles one line the pump may act on out of order (design §8.2/§5).
    /// A `cancel` is answered immediately; a `close` seen while a group runs
    /// stops the group only when it decodes and its id is unused, and goes to
    /// the driver, so the group's own response is still written first and the
    /// close is answered right after it. Every other line, including a close
    /// the driver would reject, is the driver's, which keeps the normal
    /// validation and response order for it.
    ///
    /// `full` is the bounded-queue state: a line the driver needs (a valid
    /// `close`) then takes the pinned slot, and any other line is answered with
    /// the overflow error instead of growing the queue.
    fn consume_line(&mut self, line: Vec<u8>, full: bool) {
        let Ok(value) = crate::agent::json::parse_line(&line) else {
            self.defer_line(line, full, false, None, None);
            return;
        };
        let id = protocol::request_id(&value, &self.limits);
        let op = value
            .get("op")
            .and_then(|op| op.as_str())
            .map(str::to_string);
        match op.as_deref() {
            Some("cancel") => {}
            Some("close") => {
                // Only a close the driver would accept may stop the group.
                // A decode error or a repeated id (already seen or waiting
                // ahead in the queue) belongs to the driver, so a rejected
                // request has no side effect.
                let Ok(protocol::DecodedRequest::Close(request)) =
                    protocol::decode(&value, &self.limits)
                else {
                    self.defer_line(line, full, false, id, op);
                    return;
                };
                if self.seen.contains(&request.id) || self.is_queued_id(&request.id) {
                    self.defer_line(line, full, false, Some(request.id), op);
                    return;
                }
                // `close.wait_ms` is the grace the group cleanup gets before
                // SIGKILL, so the group's escalation must use it too.
                self.stop_parallel_group(StopReason::CancelGroup, request.wait_ms);
                self.defer_line(line, full, true, Some(request.id), op);
                return;
            }
            _ => {
                self.defer_line(line, full, false, id, op);
                return;
            }
        }
        if let Some(ref id) = id
            && (self.seen.contains(id) || self.is_queued_id(id))
        {
            let message = format!("id {id} was already used on this connection");
            self.write_error(Some(id), ErrorCode::DuplicateId, message);
            return;
        }
        match protocol::decode(&value, &self.limits) {
            Ok(protocol::DecodedRequest::Cancel(request)) => {
                self.handle_cancel(
                    &request.id,
                    &request.target,
                    request.task.as_deref(),
                    request.grace_ms,
                );
            }
            Ok(_) => self.defer_line(line, full, false, id, op),
            Err(error) => {
                self.write_error(error.id.as_deref(), error.code, error.message);
            }
        }
    }

    fn handle_cancel(
        &mut self,
        id: &str,
        target: &str,
        task: Option<&str>,
        grace_ms: Option<i64>,
    ) -> bool {
        let keep = self.resolve_cancel(id, target, task, grace_ms);
        // Touched after resolution so a cancel targeting its own id is not
        // mistaken for "an id that already finished".
        self.touch(id);
        keep
    }

    fn resolve_cancel(
        &mut self,
        id: &str,
        target: &str,
        task: Option<&str>,
        grace_ms: Option<i64>,
    ) -> bool {
        if self.in_flight.as_deref() == Some(target) {
            if self.parallel.is_some() {
                let now = Instant::now();
                let wait = grace_ms.map(grace_for).unwrap_or_else(grace);
                return match self.stop_parallel_task(task, now, wait) {
                    Ok(()) => self.write_ack(id),
                    Err((code, message)) => self.write_error(Some(id), code, message),
                };
            }
            if task.is_some() {
                return self.write_error(
                    Some(id),
                    ErrorCode::InvalidRequest,
                    format!(
                        "task applies only to a parallel request; {target} is not one"
                    ),
                );
            }
            if self.pending.is_none() {
                let now = Instant::now();
                let mut pending = Pending {
                    verdict: Outcome::Cancelled,
                    kill_at: now + grace_ms.map(grace_for).unwrap_or_else(grace),
                    term_sent: false,
                    kill_sent: false,
                };
                let mut arming = None;
                if self.current > 0 {
                    signal(self.current, libc::SIGTERM);
                    pending.term_sent = true;
                    arming = Some((self.current, pending.kill_at));
                }
                self.pending = Some(pending);
                self.timeout_at = None;
                if let Some((pgid, kill_at)) = arming {
                    self.note_group_stop(pgid, kill_at);
                }
            }
            return self.write_ack(id);
        }

        if self.targets_queued(target) {
            if task.is_some() {
                return self.write_error(
                    Some(id),
                    ErrorCode::InvalidRequest,
                    format!(
                        "task cannot address the queued request {target}; cancel the whole request"
                    ),
                );
            }
            self.cancelled_queued.insert(target.to_string());
            return self.write_ack(id);
        }

        if self.seen.contains(target) {
            if task.is_some() && !self.group_seen.contains(target) {
                return self.write_error(
                    Some(id),
                    ErrorCode::InvalidRequest,
                    format!(
                        "task applies only to a parallel or workflow request; {target} is not one"
                    ),
                );
            }
            return self.write_error(
                Some(id),
                ErrorCode::CancelTooLate,
                format!("request {target} has already finished"),
            );
        }
        self.write_error(
            Some(id),
            ErrorCode::CancelUnknownId,
            format!("request {target} was never seen"),
        )
    }

    /// The third resolution path (design §3.5): `task` selects one slot of the
    /// in-flight group, its absence the whole group. Answers with the error the
    /// caller writes, so the ack and the failure stay with the same lock.
    fn stop_parallel_task(
        &mut self,
        task: Option<&str>,
        now: Instant,
        wait: Duration,
    ) -> Result<(), (ErrorCode, String)> {
        let Some(group) = self.parallel.as_mut() else {
            return Err((
                ErrorCode::CancelTooLate,
                String::from("the parallel request has already finished"),
            ));
        };
        let group_id = group.id.clone();
        let Some(task_id) = task else {
            group.stop.get_or_insert(StopReason::CancelGroup);
            for slot in &mut group.tasks {
                match slot.phase {
                    TaskPhase::Queued => {
                        arm_queued(slot, TaskStop::cancelled(TaskReason::GroupCancel));
                    }
                    TaskPhase::Running { pid, .. } => {
                        arm_running(
                            slot,
                            pid,
                            TaskStop::cancelled(TaskReason::GroupCancel),
                            now,
                            wait,
                        );
                    }
                    TaskPhase::Done => {}
                }
            }
            return Ok(());
        };
        let Some(index) = group.tasks.iter().position(|slot| slot.id == task_id) else {
            return Err((
                ErrorCode::CancelUnknownId,
                format!("task {task_id} is not in request {group_id}"),
            ));
        };
        let slot = &mut group.tasks[index];
        match slot.phase {
            TaskPhase::Done => Err((
                ErrorCode::CancelTooLate,
                format!("task {task_id} has already finished"),
            )),
            TaskPhase::Queued => {
                arm_queued(slot, TaskStop::cancelled(TaskReason::TaskCancel));
                Ok(())
            }
            TaskPhase::Running { pid, .. } => {
                arm_running(
                    slot,
                    pid,
                    TaskStop::cancelled(TaskReason::TaskCancel),
                    now,
                    wait,
                );
                Ok(())
            }
        }
    }

    /// A queued request whose id is `target`, which has not started yet.
    fn targets_queued(&self, target: &str) -> bool {
        self.queued.iter().any(|entry| {
            entry.id.as_deref() == Some(target)
                && matches!(
                    entry.op.as_deref(),
                    Some("exec") | Some("parallel") | Some("workflow") | Some("code")
                )
        })
    }

    fn touch(&mut self, id: &str) {
        if self.seen.insert(id.to_string()) {
            self.seen_order.push_back(id.to_string());
            while self.seen_order.len() > protocol::TRACKED_ID_LIMIT {
                if let Some(oldest) = self.seen_order.pop_front() {
                    self.seen.remove(&oldest);
                    self.group_seen.remove(&oldest);
                }
            }
        }
    }

    fn write_ack(&mut self, id: &str) -> bool {
        self.write(&Response::Ack(Ack::delivered(id, "cancel")))
    }

    fn write_error(&mut self, id: Option<&str>, code: ErrorCode, message: String) -> bool {
        self.write(&Response::Error(ErrorResponse::new(
            id.map(str::to_string),
            code,
            message,
        )))
    }

    fn write(&mut self, response: &Response) -> bool {
        let mut line = response.encode();
        line.push('\n');
        match write_safely(self.output, line.as_bytes()) {
            Ok(()) => true,
            Err(_) => {
                self.client_gone = true;
                false
            }
        }
    }
}

fn grace_for(ms: i64) -> Duration {
    Duration::from_millis(ms.clamp(0, CANCEL_GRACE_MS) as u64)
}

/// Whether a protocol write from the service pump already failed.
pub(crate) fn client_gone() -> bool {
    with_state(|state| state.client_gone).unwrap_or(false)
}

// --------------------------------------------------------------------- cleanup

/// Kills every group the session created and waits up to `wait_ms` for them to
/// disappear, escalating to `SIGKILL`. This is the "no leaked processes"
/// guarantee of design §6.6: a background job started in any request is in a
/// recorded group, so a `close` (and every other terminal path) takes it down.
pub(crate) fn cleanup(wait_ms: i64) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }

    let mut guard = state();
    let Some(state) = guard.as_mut() else {
        ACTIVE.store(false, Ordering::SeqCst);
        return;
    };
    if state.ended {
        return;
    }
    state.ended = true;

    let groups = state.groups.clone();
    for group in &groups {
        signal(*group, libc::SIGTERM);
    }

    let limit = wait_ms.clamp(0, MAX_CLEANUP_WAIT_MS) as u64;
    let deadline = Instant::now() + Duration::from_millis(limit);
    loop {
        reap_all();
        if groups.iter().all(|group| !group_alive(*group)) {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    for group in &groups {
        if group_alive(*group) {
            signal(*group, libc::SIGKILL);
        }
    }
    let reap_deadline = Instant::now() + Duration::from_millis(150);
    loop {
        reap_all();
        if groups.iter().all(|group| !group_alive(*group)) || Instant::now() >= reap_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let (pipe_read, pipe_write) = (state.pipe_read, state.pipe_write);
    state.pipe_read = -1;
    state.pipe_write = -1;
    *guard = None;
    ACTIVE.store(false, Ordering::SeqCst);
    crate::signal::set_child_pipe(-1);
    for fd in [pipe_read, pipe_write] {
        if fd >= 0 {
            // SAFETY: the self-pipe belongs to this process and is closed once.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

fn reap_all() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    use super::*;

    /// A spawn failure is final: the slot leaves the queue, so a later `cancel`
    /// for that task is answered `CANCEL_TOO_LATE` instead of `delivered`, and
    /// a stop armed while the task was still queued is dropped with it.
    #[test]
    fn abandoning_a_task_takes_its_slot_out_of_the_queue() {
        let mut group = ParallelGroup {
            id: String::from("p1"),
            kind: GroupKind::Parallel,
            tasks: vec![
                TaskSlot {
                    id: String::from("broken"),
                    timeout_ms: None,
                    phase: TaskPhase::Queued,
                    last_pid: None,
                    stop: Some(TaskStop::cancelled(TaskReason::TaskCancel)),
                    pending: None,
                },
                TaskSlot {
                    id: String::from("queued"),
                    timeout_ms: None,
                    phase: TaskPhase::Queued,
                    last_pid: None,
                    stop: None,
                    pending: None,
                },
            ],
            group_timeout_ms: None,
            group_deadline: None,
            stop: None,
        };

        group.abandon(0);

        assert!(matches!(group.tasks[0].phase, TaskPhase::Done));
        assert!(group.tasks[0].stop.is_none(), "a done task has no verdict");
        assert!(
            matches!(group.tasks[1].phase, TaskPhase::Queued),
            "only the abandoned slot changes"
        );
    }

    #[test]
    fn task_retry_reopens_done_workflow_slot_and_refuses_parallel_or_stopped() {
        let now = Instant::now();
        let make_slot = |id: &str| TaskSlot {
            id: id.to_string(),
            timeout_ms: Some(250),
            phase: TaskPhase::Done,
            last_pid: Some(4321),
            stop: Some(TaskStop::timed_out()),
            pending: Some(Pending {
                verdict: Outcome::Timeout,
                kill_at: now + Duration::from_secs(1),
                term_sent: true,
                kill_sent: false,
            }),
        };

        let mut parallel = ParallelGroup {
            id: String::from("p1"),
            kind: GroupKind::Parallel,
            tasks: vec![make_slot("a")],
            group_timeout_ms: None,
            group_deadline: None,
            stop: None,
        };
        assert_eq!(parallel.retry_slot(0), None);
        assert!(matches!(parallel.tasks[0].phase, TaskPhase::Done));

        let mut stopped = ParallelGroup {
            id: String::from("w-stopped"),
            kind: GroupKind::Workflow,
            tasks: vec![make_slot("a")],
            group_timeout_ms: None,
            group_deadline: None,
            stop: Some(StopReason::GroupTimeout),
        };
        assert_eq!(stopped.retry_slot(0), None);

        let mut workflow = ParallelGroup {
            id: String::from("w1"),
            kind: GroupKind::Workflow,
            tasks: vec![make_slot("a")],
            group_timeout_ms: None,
            group_deadline: None,
            stop: None,
        };
        assert_eq!(workflow.retry_slot(0), Some(Some(4321)));
        assert!(matches!(workflow.tasks[0].phase, TaskPhase::Queued));
        assert_eq!(workflow.tasks[0].last_pid, None);
        assert!(workflow.tasks[0].stop.is_none());
        assert!(workflow.tasks[0].pending.is_none());
        assert_eq!(workflow.tasks[0].timeout_ms, Some(250));
    }

    // ---------------------------------------------------- group registry policy

    /// A real live process group: a child that leads its own group, so
    /// `killpg(pgid, 0)` answers for it exactly as it would for a job.
    fn live_group() -> Child {
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        assert!(
            group_alive(child.id() as i32),
            "the child must lead its own group"
        );
        child
    }

    fn reap(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// At the bound, a live group is never evicted to make room: the insertion
    /// is refused instead, so the cleanup set keeps covering every live group.
    #[test]
    fn the_registry_refuses_a_live_group_instead_of_evicting_one() {
        let first = live_group();
        let second = live_group();
        let (first_pgid, second_pgid) = (first.id() as i32, second.id() as i32);
        let mut groups = vec![first_pgid, second_pgid];
        let third = live_group();

        let outcome = insert_group(&mut groups, third.id() as i32, 2);

        assert_eq!(
            groups,
            vec![first_pgid, second_pgid],
            "a live group must never be dropped to make room"
        );
        assert_eq!(outcome, Registered::Refused);

        reap(first);
        reap(second);
        reap(third);
    }

    /// A group whose pgid no longer exists owes nothing to cleanup, so it is
    /// what makes room at the bound.
    #[test]
    fn the_registry_compacts_dead_groups_at_the_bound() {
        let live = live_group();
        // No process can lead this group, so the entry answers `ESRCH`.
        let dead = 900_000;
        let mut groups = vec![dead, live.id() as i32];
        let fresh = live_group();

        let outcome = insert_group(&mut groups, fresh.id() as i32, 2);

        assert_eq!(outcome, Registered::Added);
        assert_eq!(groups, vec![live.id() as i32, fresh.id() as i32]);

        reap(live);
        reap(fresh);
    }

    /// The pre-fork probe agrees with the insertion: a registry full of live
    /// groups has no room, so the driver must not fork the task at all.
    #[test]
    fn the_group_probe_refuses_a_registry_full_of_live_groups() {
        let first = live_group();
        let second = live_group();
        let mut groups = vec![first.id() as i32, second.id() as i32];

        assert!(
            !can_insert_group(&mut groups, 2),
            "live groups leave no room, so the probe must refuse"
        );
        assert_eq!(
            groups,
            vec![first.id() as i32, second.id() as i32],
            "the probe must not drop a live group"
        );

        reap(first);
        reap(second);
    }

    /// The probe and the insertion share the compaction rule: once the group in
    /// the registry is gone, the probe reports room and the insertion adds the
    /// new group.
    #[test]
    fn the_group_probe_compacts_dead_groups_before_refusing() {
        let live = live_group();
        let mut groups = vec![900_000, live.id() as i32];

        assert!(
            can_insert_group(&mut groups, 2),
            "a dead group owes nothing to cleanup, so it makes room"
        );
        assert_eq!(groups, vec![live.id() as i32]);
        assert_eq!(
            insert_group(&mut groups, 900_001, 2),
            Registered::Added,
            "the insertion after the probe must still fit"
        );

        reap(live);
    }

    /// Recording the same group twice is a no-op, so the bound counts groups
    /// and not registration attempts.
    #[test]
    fn a_group_already_recorded_is_not_added_twice() {
        let live = live_group();
        let mut groups = vec![live.id() as i32];

        let outcome = insert_group(&mut groups, live.id() as i32, MAX_GROUPS);

        assert_eq!(outcome, Registered::Known);
        assert_eq!(groups.len(), 1);

        reap(live);
    }
}
