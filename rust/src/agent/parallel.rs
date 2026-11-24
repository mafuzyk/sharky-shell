//! Execution of `op:"parallel"`: one group of tasks over forked children.
//!
//! The isolation model is the design's D2 (`docs/parallel-executor-design.md`
//! §3): every task is a `fork` of the session process that runs the command on
//! its own copy-on-write `Shell`, writes its output to its own captures and
//! reports the detail an exit status cannot carry (cwd, signal, env delta)
//! through one small status record. The parent `Shell` is never borrowed while
//! a task runs, so a task cannot mutate the session even by accident.
//!
//! Reaping stays where it was: `crate::exec` remains the only module that
//! calls `waitpid`, and this module only ever asks for one of its own pids,
//! never for `-1`. `supervisor::service` keeps running while the group waits,
//! so the protocol is pumped exactly as it is during an `exec`.
//!
//! Scope: this module is the window loop and the group response, for both
//! policies. One attempt's spawn, isolation and result live in [`attempt`], the
//! single spawn/reap path a second caller shares
//! (`docs/workflow-executor-design.md` §4.1). Supervision is not here:
//! `supervisor::begin_parallel` owns the task table
//! (ids, pgids, deadlines, pending escalation), so `cancel` of the group or of
//! one task, the group and task deadlines, and a `close`/EOF seen while the
//! group runs all act from one place (design §3.5/§5). This loop spawns in the
//! window, reaps its own pids, asks the table for the verdict of each task, and
//! on `fail-fast` records a task-driven stop there when a real failure appears
//! (design §4/§7). The `&`-job group guard landed with U6.
//!
//! Descriptor discipline (design §6): captures are created in the parent by
//! [`attempt::spawn_attempt`], above fd 10 and `O_CLOEXEC`, one set per running
//! task. A task child keeps only its own three (stdout, stderr, status record)
//! and closes every other descriptor of the session and of the other tasks
//! before it evaluates anything.

use std::os::fd::RawFd;
use std::time::Instant;

use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;

use super::attempt::{
    Attempt, CANCELLED_STATUS, TIMEOUT_STATUS, effective_cwd, spawn_attempt, spawn_error,
    unknown_result,
};
use super::json::{
    GroupState, Limits, ParallelPolicy, ParallelResult, TaskReason, TaskResult, TaskState,
    Termination,
};
use super::protocol::{ParallelRequest, TaskRequest};
use super::store::CaptureStore;
use super::supervisor::{self, StopReason};
use crate::exec;
use crate::shell::Shell;

/// Why a task was refused before the fork: recording its group would exceed
/// the session's registry of live groups (supervisor design §6.6).
const REGISTRY_FULL_MESSAGE: &str = "process-group registry is full";

/// Executes one validated `op:"parallel"` request and returns its single
/// `type:"parallel"` response body.
///
/// `Err` is a failure the driver must report as a fatal `INTERNAL`: it can only
/// come from reading a capture back, which means result integrity is gone.
///
/// Supervision (deadlines, `cancel`, `close`/EOF) lives in [`supervisor`]: this
/// loop spawns, reaps and asks the table for verdicts. That is what makes the
/// per-task escalation the same code path an `exec` cancel uses, and why a
/// `cancel` for this group is no longer answered `CANCEL_TOO_LATE` (design §5).
pub(crate) fn run(
    shell: &mut Shell,
    null_fd: RawFd,
    limits: &Limits,
    captures: &mut CaptureStore,
    request: &ParallelRequest,
) -> Result<ParallelResult, String> {
    let started = Instant::now();
    let window = request.max_concurrency.min(request.tasks.len());
    let mut group = Group::new(request, limits, captures, shell.cwd.to_string_lossy().into_owned());
    let specs: Vec<(String, Option<i64>)> = request
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.timeout_ms))
        .collect();
    supervisor::begin_parallel(&request.id, &specs, request.timeout_ms);
    let mut next_spawn = 0usize;

    loop {
        // The cancel ack and the deadline pump of the session run here,
        // between spawns and reaps, exactly as they do between the polls of an
        // `exec` wait (design §3.5/§4).
        supervisor::service();
        if crate::signal::fatal_signal().is_some() {
            supervisor::record_stop(StopReason::CancelGroup);
            group.cancel_unfinished();
            break;
        }

        // Window: spawn in request order while a slot is free. FIFO, no
        // reordering, no work stealing; the queue is the validated task list
        // itself, so it cannot grow (design §4). A task supervision already
        // stopped (cancel, group deadline) is answered without spawning.
        while next_spawn < request.tasks.len() && group.running() < window && !group.stopping() {
            if let Some(stop) = supervisor::take_task_stop(next_spawn) {
                group.record_stop(next_spawn, stop);
                next_spawn += 1;
                continue;
            }
            group.spawn(shell, next_spawn, null_fd);
            next_spawn += 1;
        }

        if !group.drain()? {
            if group.running() == 0 {
                break;
            }
            group.wait_for_one()?;
            if crate::signal::fatal_signal().is_some() {
                // The session is leaving: stop the group so running tasks get
                // SIGTERM immediately and shutdown cleanup finishes the reap.
                supervisor::record_stop(StopReason::CancelGroup);
                group.cancel_unfinished();
                break;
            }
        }
    }

    // A stop may have left tasks that never got a window slot; the supervisor
    // holds their verdict, so every requested task is still reported.
    group.fill_unscheduled();
    let stopped = supervisor::parallel_stopped();
    supervisor::end_parallel();
    let result = group.into_result(started, stopped);
    Ok(result)
}

/// The answer to a `parallel` request cancelled while it was still queued: it
/// never spawns, every task is `cancelled`/`130` with `reason:"group_cancel"`,
/// and the group reports `state:"cancelled"` (design §5).
pub(crate) fn cancelled(request: &ParallelRequest, session_cwd: String) -> ParallelResult {
    let mut result = ParallelResult::new(&request.id, request.policy);
    result.max_concurrency = request.max_concurrency.min(request.tasks.len());
    result.status = CANCELLED_STATUS;
    result.state = GroupState::Cancelled;
    for task in &request.tasks {
        let mut entry = TaskResult::new(task.id.clone(), effective_cwd(task, &session_cwd));
        entry.state = TaskState::Cancelled;
        entry.termination = Termination::Cancelled;
        entry.status = CANCELLED_STATUS;
        entry.reason = Some(TaskReason::GroupCancel);
        result.tasks.push(entry);
    }
    result
}

// ---------------------------------------------------------------------- group

/// Where a task is in its lifecycle. A running slot owns its attempt, so the
/// attempt's captures exist exactly while the child can still write to them.
enum Phase {
    Queued,
    Running(Attempt),
    Done,
}

struct Slot {
    phase: Phase,
    result: Option<TaskResult>,
}

struct Group<'a> {
    id: &'a str,
    policy: ParallelPolicy,
    window: usize,
    tasks: &'a [TaskRequest],
    slots: Vec<Slot>,
    limits: &'a Limits,
    captures: &'a mut CaptureStore,
    session_cwd: String,
    /// Rendered-byte budget left for the whole response (design §6).
    budget: usize,
    output_truncated: bool,
    started_tasks: usize,
}

impl<'a> Group<'a> {
    fn new(
        request: &'a ParallelRequest,
        limits: &'a Limits,
        captures: &'a mut CaptureStore,
        session_cwd: String,
    ) -> Self {
        Self {
            id: &request.id,
            policy: request.policy,
            window: request.max_concurrency.min(request.tasks.len()),
            tasks: &request.tasks,
            slots: request
                .tasks
                .iter()
                .map(|_| Slot {
                    phase: Phase::Queued,
                    result: None,
                })
                .collect(),
            limits,
            captures,
            session_cwd,
            budget: limits.max_parallel_output_bytes,
            output_truncated: false,
            started_tasks: 0,
        }
    }

    fn running(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot.phase, Phase::Running(_)))
            .count()
    }

    /// The supervisor is the single source of a stop: a group cancel, a
    /// `close`/EOF, or the group deadline. This loop only reads it.
    fn stopping(&self) -> bool {
        supervisor::parallel_stopped().is_some()
    }

    /// Descriptors a task child must not inherit: the session's `/dev/null`
    /// and every capture of the tasks running right now.
    fn other_fds(&self, null_fd: RawFd) -> Vec<RawFd> {
        let mut fds = vec![null_fd];
        for slot in &self.slots {
            if let Phase::Running(attempt) = &slot.phase {
                fds.extend(attempt.fds());
            }
        }
        fds
    }

    /// Spawns one task into its slot. A task that cannot spawn is answered
    /// `spawn_error`/`126` and the group keeps going (design §7).
    fn spawn(&mut self, shell: &mut Shell, index: usize, null_fd: RawFd) {
        // A group the session's registry cannot record could outlive the
        // shutdown cleanup, so the refusal is checked before the fork: the
        // task is answered without a process ever existing.
        if !supervisor::can_track_group() {
            self.spawn_refused(index, String::from(REGISTRY_FULL_MESSAGE));
            return;
        }
        let close_fds = self.other_fds(null_fd);
        match spawn_attempt(shell, &self.tasks[index], null_fd, &close_fds) {
            Ok(attempt) => {
                self.started_tasks += 1;
                // Registers the task's own pgid for cleanup and starts its
                // deadline in the supervisor, which is the only escalation
                // path from here on (design §3.5).
                supervisor::task_running(index, attempt.leader);
                self.slots[index].phase = Phase::Running(attempt);
            }
            Err(message) => self.spawn_refused(index, message),
        }
    }

    /// Answers a task that never got a pid, in the parent, before any process
    /// existed: `spawn_error`/`126` with the reason visible on the task.
    fn spawn_refused(&mut self, index: usize, message: String) {
        self.slots[index].result =
            Some(spawn_error(&self.tasks[index], &self.session_cwd, message));
        self.slots[index].phase = Phase::Done;
        // A task that never got a pid is final for the supervisor too, so a
        // later `cancel` for it is `CANCEL_TOO_LATE` rather than `delivered`
        // with a stop nobody will read.
        supervisor::spawn_failed(index);
        // A spawn error also fails the group (design §7). Record the stop after
        // the slot is done so the task keeps its own result.
        if self.policy == ParallelPolicy::FailFast {
            supervisor::record_stop(StopReason::FailFast);
        }
    }

    /// Records a task supervision stopped before it ever spawned (queued
    /// cancel, group cancel, group deadline). `reason` is the one the response
    /// carries; the status is `130` because it never ran (design §2.4/§7).
    fn record_stop(&mut self, index: usize, stop: supervisor::TaskStop) {
        if self.slots[index].result.is_some() {
            return;
        }
        let task = &self.tasks[index];
        let mut result = TaskResult::new(task.id.clone(), effective_cwd(task, &self.session_cwd));
        match stop.outcome {
            supervisor::Outcome::Cancelled => {
                result.state = TaskState::Cancelled;
                result.termination = Termination::Cancelled;
                result.status = CANCELLED_STATUS;
            }
            supervisor::Outcome::Timeout => {
                result.state = TaskState::Timeout;
                result.termination = Termination::Timeout;
                result.status = TIMEOUT_STATUS;
            }
        }
        result.reason = stop.reason;
        self.slots[index].result = Some(result);
        self.slots[index].phase = Phase::Done;
    }

    /// Answers every task that never got a window slot after the group stopped.
    /// A task with no recorded stop is left to [`Group::cancel_unfinished`].
    fn fill_unscheduled(&mut self) {
        for index in 0..self.slots.len() {
            if !matches!(self.slots[index].phase, Phase::Queued) {
                continue;
            }
            if let Some(stop) = supervisor::take_task_stop(index) {
                self.record_stop(index, stop);
            }
        }
    }

    /// Reaps every task leader that already changed state, without blocking.
    /// Returns whether anything was reaped, which is what refills the window.
    fn drain(&mut self) -> Result<bool, String> {
        let mut reaped = false;
        for index in 0..self.slots.len() {
            let leader = match &mut self.slots[index].phase {
                Phase::Running(attempt) => {
                    attempt.drain_pty();
                    attempt.leader
                }
                _ => continue,
            };
            let status = exec::wait_scan(Pid::from_raw(leader));
            match status {
                Some(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => {
                    self.finish(index, leader, status)?;
                    reaped = true;
                }
                // A stop or a resume is not a final status: the task is still
                // running, exactly as `exec` treats it in session mode.
                Some(
                    WaitStatus::StillAlive | WaitStatus::Stopped(..) | WaitStatus::Continued(..),
                ) => {}
                // A status this build does not expect, or one consumed
                // elsewhere: reported as unknown, never as a silent success
                // (design §3.7).
                Some(_) | None => {
                    self.finish(index, leader, None)?;
                    reaped = true;
                }
            }
        }
        Ok(reaped)
    }

    /// Waits for one event. `fail-fast` always polls: a failure may come from
    /// any slot, so blocking on the oldest running task would hide it. A
    /// `wait-all` window with more than one task polls for the same reason the
    /// window exists: a finished sibling must free its slot as soon as it ends,
    /// and blocking on the oldest leader would keep it occupied until that
    /// leader exits, turning the window into waves. An armed deadline polls
    /// because its 50 ms backstop is what lets the supervisor escalate. Only a
    /// single running task with no deadline blocks, so the loop never spins. A
    /// `cancel` arms a pending in the supervisor, and both wait paths pump it
    /// between polls.
    fn wait_for_one(&mut self) -> Result<(), String> {
        // The self-pipe poll wakes on SIGCHLD (50 ms backstop) and `drain`
        // then scans every slot, collecting whichever pid changed state.
        if self.policy == ParallelPolicy::FailFast
            || self.running() > 1
            || supervisor::parallel_deadline_armed()
        {
            supervisor::wait_poll();
            return Ok(());
        }
        let Some((index, leader)) = self.first_running() else {
            return Ok(());
        };
        // The session wait path: `exec` stays the only module that calls
        // `waitpid`, and it pumps the supervisor between polls.
        match exec::wait_member(Pid::from_raw(leader)) {
            Some(status) if matches!(status, WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => {
                self.finish(index, leader, Some(status))?;
            }
            // A stop or a resume is not a final status.
            Some(_) => {}
            // `ECHILD` or a fatal signal: if a signal interrupted the wait,
            // leave the slot for the outer loop's fatal-signal handler instead
            // of inventing a missing-status error.
            None if crate::signal::fatal_signal().is_some() => return Ok(()),
            None => self.finish(index, leader, None)?,
        }
        Ok(())
    }

    fn first_running(&self) -> Option<(usize, i32)> {
        self.slots
            .iter()
            .enumerate()
            .find_map(|(index, slot)| match &slot.phase {
                Phase::Running(attempt) => Some((index, attempt.leader)),
                _ => None,
            })
    }

    /// Reads a reaped task's captures and records its result. The attempt holds
    /// what only it knows (captures, leader, spawn instant); everything the group
    /// decides around the result — the truncation flag, fail-fast, slot
    /// bookkeeping — stays here.
    ///
    /// `stop` is the supervisor's verdict for a task it terminated; it wins over
    /// the wait status, exactly as `exec` reports cancel/timeout over the
    /// command's own exit (design §5).
    fn finish(
        &mut self,
        index: usize,
        leader: i32,
        status: Option<WaitStatus>,
    ) -> Result<(), String> {
        let stop = supervisor::finish_task(index, leader);
        let attempt = {
            let slot = &mut self.slots[index];
            match std::mem::replace(&mut slot.phase, Phase::Done) {
                Phase::Running(attempt) => attempt,
                other => {
                    slot.phase = other;
                    return Ok(());
                }
            }
        };

        let task = &self.tasks[index];
        let mut outcome = attempt.finish(
            task,
            &self.session_cwd,
            status,
            stop,
            self.limits,
            &mut self.budget,
        )?;
        outcome.result.capture_id = self
            .captures
            .record(outcome.raw_stdout, outcome.raw_stderr);
        if outcome.truncated {
            self.output_truncated = true;
        }
        let fail_fast =
            self.policy == ParallelPolicy::FailFast && triggers_fail_fast(&outcome.result);
        self.slots[index].result = Some(outcome.result);
        if fail_fast {
            // `finish_task` already marked this slot Done in the supervisor, so
            // recording the stop cannot arm the trigger against itself: it
            // keeps its real status, termination and output. Everyone else
            // running is TERMed and everyone queued is answered without a spawn.
            supervisor::record_stop(StopReason::FailFast);
        }
        Ok(())
    }

    /// Labels every task still outstanding after the session was told to die.
    fn cancel_unfinished(&mut self) {
        for index in 0..self.slots.len() {
            if self.slots[index].result.is_some() {
                continue;
            }
            let mut result = TaskResult::new(
                self.tasks[index].id.clone(),
                effective_cwd(&self.tasks[index], &self.session_cwd),
            );
            result.state = TaskState::Cancelled;
            result.termination = Termination::Cancelled;
            result.status = CANCELLED_STATUS;
            result.reason = Some(TaskReason::GroupCancel);
            self.slots[index].result = Some(result);
            self.slots[index].phase = Phase::Done;
        }
    }

    /// The single response: every requested task, in request order (design
    /// §2.3), plus the aggregate status rule of design §2.4.
    fn into_result(mut self, started: Instant, stopped: Option<StopReason>) -> ParallelResult {
        let mut result = ParallelResult::new(self.id, self.policy);
        let elapsed_us = started.elapsed().as_micros().min(i64::MAX as u128) as i64;
        result.duration_us = elapsed_us;
        result.duration_ms = elapsed_us / 1000;
        result.max_concurrency = self.window;
        result.started_tasks = self.started_tasks;
        result.output_truncated = self.output_truncated;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let task = slot.result.take().unwrap_or_else(|| {
                unknown_result(
                    self.tasks[index].id.clone(),
                    &effective_cwd(&self.tasks[index], &self.session_cwd),
                )
            });
            result.tasks.push(task);
        }
        result.status = aggregate_status(&result.tasks);
        result.state = if stopped == Some(StopReason::CancelGroup) {
            GroupState::Cancelled
        } else {
            GroupState::Completed
        };
        result
    }
}

// -------------------------------------------------------------------- helpers

/// Whether a finished task is a genuine failure signal for `fail-fast`.
///
/// The failure part is the aggregate rule of design §2.4 (`termination !=
/// exited || status != 0`), which also covers signal death, a task deadline and
/// a child-side spawn error. A task supervision already ended carries a
/// `reason`: design §7 says a `cancel` is not a failure signal, and a stop for
/// another reason (group cancel, group deadline, an earlier fail-fast) must not
/// rewrite the first stop. Its non-zero status still feeds the aggregate.
pub(crate) fn triggers_fail_fast(result: &TaskResult) -> bool {
    result.reason.is_none() && (result.termination != Termination::Exited || result.status != 0)
}

/// The aggregate status rule (design §2.4): the status of the first task in
/// request order that did not exit 0, or 0. A task that ended any other way
/// than `exited` counts as a failure even with a zero status, so a group whose
/// task lost its status can never publish `0`.
fn aggregate_status(tasks: &[TaskResult]) -> i64 {
    tasks
        .iter()
        .find(|task| task.termination != Termination::Exited || task.status != 0)
        .map(|task| task.status)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::json::ErrorCode;
    use super::*;

    fn ended(id: &str, status: i64) -> TaskResult {
        let mut task = TaskResult::new(String::from(id), String::from("/"));
        task.status = status;
        task
    }

    /// A task whose status was lost is `INTERNAL` with a non-zero status, and
    /// the aggregate must publish it: a group that could not read one of its
    /// tasks can never look successful (design §2.4/§3.7).
    #[test]
    fn an_unknown_task_status_is_non_zero_and_fails_the_aggregate() {
        let lost = unknown_result(String::from("lost"), "/");
        assert_eq!(lost.status, 1);
        assert_eq!(lost.termination, Termination::Exited);
        assert_eq!(
            lost.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Internal)
        );

        assert_eq!(aggregate_status(&[ended("ok", 0), lost]), 1);
    }

    /// The aggregate is the first failure in request order, not the first one to
    /// arrive, and an all-zero group stays zero.
    #[test]
    fn the_aggregate_is_the_first_failure_in_request_order() {
        let tasks = [ended("ok", 0), ended("three", 3), ended("one", 1)];
        assert_eq!(aggregate_status(&tasks), 3);
        assert_eq!(aggregate_status(&tasks[..1]), 0);
    }
}
