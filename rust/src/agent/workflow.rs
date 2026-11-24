//! Execution of `op:"workflow"`: DAG scheduler over the shared task attempt
//! machinery (`docs/workflow-executor-design.md`).
//!
//! There is no second command evaluator: every attempt spawns through
//! [`attempt::spawn_attempt`] and finishes through [`Attempt::finish`], and the
//! supervisor's existing group table (`begin_group(GroupKind::Workflow)`) owns
//! cancellation, deadlines and process-group cleanup.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;

use super::attempt::{
    Attempt, CANCELLED_STATUS, effective_cwd, spawn_attempt, spawn_error, unknown_result,
};
use super::json::{
    AttemptDetail, ConditionEcho, ExitCodeMatcher, FailurePolicy, GroupState, Limits,
    ParallelPolicy, StreamData, TaskReason, TaskResult, TaskState, Termination, WorkflowResult,
    WorkflowTaskResult,
};
use super::parallel::triggers_fail_fast;
use super::protocol::{
    Backoff, Condition, RetryOn, RetryPolicy, WorkflowRequest, WorkflowTask,
};
use super::store::{CaptureStore, StoredStream};
use super::supervisor::{self, GroupKind, StopReason};
use crate::exec;
use crate::shell::Shell;

/// Why a task was refused before the fork: recording its group would exceed
/// the session's registry of live groups (supervisor design §6.6).
const REGISTRY_FULL_MESSAGE: &str = "process-group registry is full";

/// Numeric status reserved for nodes skipped because a dependency did not
/// succeed or a condition evaluated false (`workflow-executor-design.md` §2.3).
const SKIPPED_STATUS: i64 = 125;

/// Executes one validated `op:"workflow"` request and returns its single
/// `type:"workflow"` response body.
pub(crate) fn run(
    shell: &mut Shell,
    null_fd: RawFd,
    limits: &Limits,
    captures: &mut CaptureStore,
    request: &WorkflowRequest,
) -> Result<WorkflowResult, String> {
    let started = Instant::now();
    let mut workflow = Workflow::new(
        request,
        limits,
        captures,
        shell.cwd.to_string_lossy().into_owned(),
    );
    let specs: Vec<(String, Option<i64>)> = request
        .tasks
        .iter()
        .map(|task| (task.base.id.clone(), task.base.timeout_ms))
        .collect();
    supervisor::begin_group(&request.id, GroupKind::Workflow, &specs, request.timeout_ms);

    loop {
        supervisor::service();
        if crate::signal::fatal_signal().is_some() {
            supervisor::record_stop(StopReason::CancelGroup);
            workflow.cancel_unfinished();
            break;
        }

        workflow.consume_stops();
        let classified = workflow.classify_pending();
        let spawned = workflow.spawn_window(shell, null_fd);
        let reaped = workflow.drain()?;

        if crate::signal::fatal_signal().is_some() {
            supervisor::record_stop(StopReason::CancelGroup);
            workflow.cancel_unfinished();
            break;
        }

        if reaped || classified || spawned {
            continue;
        }
        if workflow.running() == 0 && !workflow.any_ready() {
            break;
        }
        supervisor::wait_poll();
        if crate::signal::fatal_signal().is_some() {
            supervisor::record_stop(StopReason::CancelGroup);
            workflow.cancel_unfinished();
            break;
        }
    }

    workflow.fill_unscheduled();
    let stopped = supervisor::parallel_stopped();
    supervisor::end_parallel();
    Ok(workflow.into_result(started, stopped))
}

/// Answer to a `workflow` request cancelled while queued behind an earlier
/// request: nothing spawns, every node is `blocked`/`130` (`group_cancel`),
/// `order` is preserved, and `state` is `cancelled`.
pub(crate) fn cancelled(request: &WorkflowRequest, session_cwd: String) -> WorkflowResult {
    let mut result = WorkflowResult::new(&request.id, request.policy, request.failure_policy);
    result.max_concurrency = request.max_concurrency.min(request.tasks.len());
    result.status = CANCELLED_STATUS;
    result.state = GroupState::Cancelled;
    result.blocked_tasks = request.tasks.len();
    result.order = request
        .order
        .iter()
        .map(|&index| request.tasks[index].base.id.clone())
        .collect();
    for task in &request.tasks {
        let mut base = TaskResult::new(
            task.base.id.clone(),
            effective_cwd(&task.base, &session_cwd),
        );
        base.state = TaskState::Blocked;
        base.termination = Termination::Cancelled;
        base.status = CANCELLED_STATUS;
        base.reason = Some(TaskReason::GroupCancel);
        result.tasks.push(WorkflowTaskResult {
            base,
            depends_on: dep_ids(task, &request.tasks),
            lock: task.lock.clone(),
            condition: task
                .condition
                .as_ref()
                .map(|condition| condition_echo(condition, None)),
            attempts: 0,
            attempts_detail: Vec::new(),
        });
    }
    result
}

// ------------------------------------------------------------------- workflow

enum NodePhase {
    Pending,
    Ready {
        ready_at: Option<Instant>,
        previous_pgid: Option<i32>,
    },
    Running {
        attempt: Attempt,
        started_ms: i64,
    },
    Done,
}

struct NodeSlot {
    phase: NodePhase,
    result: Option<TaskResult>,
    attempts: usize,
    attempts_detail: Vec<AttemptDetail>,
    total_duration_us: i64,
    ever_spawned: bool,
    last_pgid: Option<i64>,
    condition_outcome: Option<bool>,
    os_lock: Option<super::capture::CrossSessionLock>,
}

struct Workflow<'a> {
    id: &'a str,
    policy: ParallelPolicy,
    failure_policy: FailurePolicy,
    window: usize,
    tasks: &'a [WorkflowTask],
    dependents: Vec<Vec<usize>>,
    order: &'a [usize],
    slots: Vec<NodeSlot>,
    limits: &'a Limits,
    captures: &'a mut CaptureStore,
    session_cwd: String,
    budget: usize,
    output_truncated: bool,
    started_tasks: usize,
    first_spawn: Option<Instant>,
    held_locks: HashMap<String, usize>,
}

impl<'a> Workflow<'a> {
    fn new(
        request: &'a WorkflowRequest,
        limits: &'a Limits,
        captures: &'a mut CaptureStore,
        session_cwd: String,
    ) -> Self {
        let mut dependents = vec![Vec::new(); request.tasks.len()];
        for (index, task) in request.tasks.iter().enumerate() {
            for &dep in &task.depends_on {
                dependents[dep].push(index);
            }
        }
        Self {
            id: &request.id,
            policy: request.policy,
            failure_policy: request.failure_policy,
            window: request.max_concurrency.min(request.tasks.len()),
            tasks: &request.tasks,
            dependents,
            order: &request.order,
            slots: request
                .tasks
                .iter()
                .map(|_| NodeSlot {
                    phase: NodePhase::Pending,
                    result: None,
                    attempts: 0,
                    attempts_detail: Vec::new(),
                    total_duration_us: 0,
                    ever_spawned: false,
                    last_pgid: None,
                    condition_outcome: None,
                    os_lock: None,
                })
                .collect(),
            limits,
            captures,
            session_cwd,
            budget: limits.max_parallel_output_bytes,
            output_truncated: false,
            started_tasks: 0,
            first_spawn: None,
            held_locks: HashMap::new(),
        }
    }

    fn running(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot.phase, NodePhase::Running { .. }))
            .count()
    }

    fn any_ready(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot.phase, NodePhase::Ready { .. }))
    }

    fn stopping(&self) -> bool {
        supervisor::parallel_stopped().is_some()
    }

    fn other_fds(&self, null_fd: RawFd) -> Vec<RawFd> {
        let mut fds = vec![null_fd];
        for slot in &self.slots {
            if let NodePhase::Running { attempt, .. } = &slot.phase {
                fds.extend(attempt.fds());
            }
            if let Some(lock) = &slot.os_lock {
                fds.push(lock.fd());
            }
        }
        fds
    }

    fn record_stop(&mut self, index: usize, stop: supervisor::TaskStop) {
        if self.slots[index].result.is_some() {
            return;
        }
        let task = &self.tasks[index];
        let mut result = TaskResult::new(
            task.base.id.clone(),
            effective_cwd(&task.base, &self.session_cwd),
        );
        result.state = if stop.reason == Some(TaskReason::TaskCancel) {
            TaskState::Cancelled
        } else {
            TaskState::Blocked
        };
        result.termination = Termination::Cancelled;
        result.status = CANCELLED_STATUS;
        result.reason = stop.reason;
        result.duration_us = self.slots[index].total_duration_us;
        result.duration_ms = result.duration_us / 1000;
        result.pgid = self.slots[index].last_pgid;
        self.slots[index].result = Some(result);
        self.slots[index].phase = NodePhase::Done;
        self.release_lock(index);
    }

    fn mark_skipped(&mut self, index: usize, reason: TaskReason) {
        if self.slots[index].result.is_some() {
            return;
        }
        let task = &self.tasks[index];
        let mut result = TaskResult::new(
            task.base.id.clone(),
            effective_cwd(&task.base, &self.session_cwd),
        );
        result.state = TaskState::Skipped;
        result.termination = Termination::Skipped;
        result.status = SKIPPED_STATUS;
        result.reason = Some(reason);
        self.slots[index].result = Some(result);
        self.slots[index].phase = NodePhase::Done;
        self.release_lock(index);
        supervisor::abandon_task(index);
    }

    fn consume_stops(&mut self) {
        for &index in self.order {
            if !matches!(
                self.slots[index].phase,
                NodePhase::Pending | NodePhase::Ready { .. }
            ) {
                continue;
            }
            if let Some(stop) = supervisor::take_task_stop(index) {
                self.record_stop(index, stop);
            }
        }
    }

    fn has_unfinished_dependent(&self, index: usize) -> bool {
        self.dependents[index]
            .iter()
            .any(|&dep| !matches!(self.slots[dep].phase, NodePhase::Done))
    }

    fn classify_pending(&mut self) -> bool {
        if self.stopping() {
            return false;
        }
        let mut changed = false;
        for &index in self.order {
            if !matches!(self.slots[index].phase, NodePhase::Pending) {
                continue;
            }
            let ready = self.tasks[index]
                .depends_on
                .iter()
                .all(|&dep| matches!(self.slots[dep].phase, NodePhase::Done));
            if !ready {
                continue;
            }
            let dep_failed = self.tasks[index]
                .depends_on
                .iter()
                .any(|&dep| !is_success(self.slots[dep].result.as_ref()));
            if dep_failed {
                match self.failure_policy {
                    FailurePolicy::Skip => {
                        self.mark_skipped(index, TaskReason::DependencyFailed);
                        changed = true;
                        continue;
                    }
                    FailurePolicy::Fail => {
                        supervisor::record_stop(StopReason::FailurePolicy);
                        self.record_stop(
                            index,
                            supervisor::TaskStop {
                                outcome: supervisor::Outcome::Cancelled,
                                reason: Some(TaskReason::FailurePolicy),
                            },
                        );
                        supervisor::abandon_task(index);
                        changed = true;
                        continue;
                    }
                    FailurePolicy::Continue => {}
                }
            }
            if let Some(condition) = &self.tasks[index].condition {
                let passed =
                    evaluate_condition(condition, self.slots[condition.task_index].result.as_ref());
                self.slots[index].condition_outcome = Some(passed);
                if !passed {
                    self.mark_skipped(index, TaskReason::ConditionFalse);
                    if self.failure_policy == FailurePolicy::Fail
                        && self.has_unfinished_dependent(index)
                    {
                        supervisor::record_stop(StopReason::FailurePolicy);
                    }
                    changed = true;
                    continue;
                }
            }
            self.slots[index].phase = NodePhase::Ready {
                ready_at: None,
                previous_pgid: None,
            };
            changed = true;
        }
        changed
    }

    fn try_acquire_lock(&mut self, index: usize) -> bool {
        let Some(name) = &self.tasks[index].lock else {
            return true;
        };
        if let Some(&owner) = self.held_locks.get(name) {
            return owner == index;
        }
        match super::capture::CrossSessionLock::try_acquire(name) {
            Ok(Some(guard)) => {
                self.held_locks.insert(name.clone(), index);
                self.slots[index].os_lock = Some(guard);
                true
            }
            Ok(None) | Err(_) => false,
        }
    }

    fn release_lock(&mut self, index: usize) {
        if let Some(name) = &self.tasks[index].lock
            && self.held_locks.get(name) == Some(&index)
        {
            self.held_locks.remove(name);
            self.slots[index].os_lock = None;
        }
    }

    fn spawn_window(&mut self, shell: &mut Shell, null_fd: RawFd) -> bool {
        let mut spawned = false;
        let now = Instant::now();
        let ready: Vec<usize> = self
            .order
            .iter()
            .copied()
            .filter(|&index| {
                matches!(
                    self.slots[index].phase,
                    NodePhase::Ready {
                        ready_at,
                        previous_pgid,
                    } if ready_at.is_none_or(|at| now >= at)
                        && previous_pgid.is_none_or(|pgid| !supervisor::group_is_alive(pgid))
                )
            })
            .collect();
        for index in ready {
            if self.running() >= self.window || self.stopping() {
                break;
            }
            if let Some(stop) = supervisor::take_task_stop(index) {
                self.record_stop(index, stop);
                continue;
            }
            if self.try_acquire_lock(index) {
                self.spawn(shell, index, null_fd);
                spawned = true;
            }
        }
        spawned
    }

    fn spawn(&mut self, shell: &mut Shell, index: usize, null_fd: RawFd) {
        let origin = *self.first_spawn.get_or_insert_with(Instant::now);
        let started_ms = origin.elapsed().as_millis().min(i64::MAX as u128) as i64;
        #[cfg(debug_assertions)]
        if std::env::var("SHARKY_WORKFLOW_TEST_SPAWN_FAILURE")
            .ok()
            .as_deref()
            == Some(self.tasks[index].base.id.as_str())
        {
            self.spawn_refused(index, started_ms, String::from("injected test spawn failure"));
            return;
        }
        if !supervisor::can_track_group() {
            self.spawn_refused(index, started_ms, String::from(REGISTRY_FULL_MESSAGE));
            return;
        }
        let close_fds = self.other_fds(null_fd);
        match spawn_attempt(shell, &self.tasks[index].base, null_fd, &close_fds) {
            Ok(attempt) => {
                if !self.slots[index].ever_spawned {
                    self.slots[index].ever_spawned = true;
                    self.started_tasks += 1;
                }
                supervisor::task_running(index, attempt.leader);
                self.slots[index].phase = NodePhase::Running {
                    attempt,
                    started_ms,
                };
            }
            Err(message) => self.spawn_refused(index, started_ms, message),
        }
    }

    fn spawn_refused(&mut self, index: usize, started_ms: i64, message: String) {
        let result = spawn_error(&self.tasks[index].base, &self.session_cwd, message);
        supervisor::spawn_failed(index);
        let budget = self.budget;
        self.conclude_attempt(index, result, false, budget, started_ms, None, None);
    }

    fn drain(&mut self) -> Result<bool, String> {
        let mut reaped = false;
        for index in 0..self.slots.len() {
            let leader = match &mut self.slots[index].phase {
                NodePhase::Running { attempt, .. } => {
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
                Some(
                    WaitStatus::StillAlive | WaitStatus::Stopped(..) | WaitStatus::Continued(..),
                ) => {}
                Some(_) | None => {
                    self.finish(index, leader, None)?;
                    reaped = true;
                }
            }
        }
        Ok(reaped)
    }

    fn finish(
        &mut self,
        index: usize,
        leader: i32,
        status: Option<WaitStatus>,
    ) -> Result<(), String> {
        let stop = supervisor::finish_task(index, leader);
        let (attempt, started_ms) = {
            let slot = &mut self.slots[index];
            match std::mem::replace(&mut slot.phase, NodePhase::Done) {
                NodePhase::Running {
                    attempt,
                    started_ms,
                } => (attempt, started_ms),
                other => {
                    slot.phase = other;
                    return Ok(());
                }
            }
        };

        let mut scratch_budget = self.budget;
        let outcome = attempt.finish(
            &self.tasks[index].base,
            &self.session_cwd,
            status,
            stop,
            self.limits,
            &mut scratch_budget,
        )?;
        self.conclude_attempt(
            index,
            outcome.result,
            outcome.truncated,
            scratch_budget,
            started_ms,
            stop,
            Some((outcome.raw_stdout, outcome.raw_stderr)),
        );
        Ok(())
    }

    /// Records the finished attempt in `attempts_detail`, either reschedules
    /// for another attempt after backoff without committing superseded
    /// streams/budget or finalizes the node and enforces `policy` /
    /// `failure_policy`.
    #[allow(clippy::too_many_arguments)]
    fn conclude_attempt(
        &mut self,
        index: usize,
        mut result: TaskResult,
        truncated: bool,
        next_budget: usize,
        started_ms: i64,
        stop: Option<supervisor::TaskStop>,
        raw_streams: Option<(StoredStream, StoredStream)>,
    ) {
        let slot = &mut self.slots[index];
        slot.attempts += 1;
        slot.total_duration_us += result.duration_us;
        if result.pgid.is_some() {
            slot.last_pgid = result.pgid;
        }
        slot.attempts_detail.push(AttemptDetail {
            n: slot.attempts,
            started_ms,
            duration_ms: result.duration_ms,
            status: result.status,
            termination: result.termination,
            stdout_bytes: result.stdout.bytes,
            stderr_bytes: result.stderr.bytes,
        });

        let retry_delay = (supervisor::parallel_stopped().is_none()
            && !matches!(stop, Some(stop) if stop.outcome == supervisor::Outcome::Cancelled))
        .then(|| {
            let retry = self.tasks[index].retry.as_ref()?;
            if slot.attempts >= retry.max_attempts {
                return None;
            }
            let class = retry_outcome_class(&result)?;
            if !retry.on.contains(&class) {
                return None;
            }
            Some(retry_delay_ms(
                retry,
                slot.attempts,
                self.limits.max_workflow_backoff_ms,
            ))
        })
        .flatten();

        if let Some(delay_ms) = retry_delay
            && let Some(prev_pgid) = supervisor::task_retry(index)
        {
            self.slots[index].phase = NodePhase::Ready {
                ready_at: (delay_ms > 0)
                    .then(|| Instant::now() + Duration::from_millis(delay_ms as u64)),
                previous_pgid: prev_pgid,
            };
            return;
        }

        self.budget = next_budget;
        if truncated {
            self.output_truncated = true;
        }
        if let Some((raw_stdout, raw_stderr)) = raw_streams {
            result.capture_id = self.captures.record(raw_stdout, raw_stderr);
        }
        let slot = &mut self.slots[index];
        result.duration_us = slot.total_duration_us;
        result.duration_ms = slot.total_duration_us / 1000;
        result.pgid = result.pgid.or(slot.last_pgid);
        slot.result = Some(result);
        slot.phase = NodePhase::Done;
        self.release_lock(index);
        self.apply_final_policy(index);
    }

    fn apply_final_policy(&self, index: usize) {
        let Some(result) = self.slots[index].result.as_ref() else {
            return;
        };
        if self.policy == ParallelPolicy::FailFast && triggers_fail_fast(result) {
            supervisor::record_stop(StopReason::FailFast);
        } else if self.failure_policy == FailurePolicy::Fail
            && result.reason.is_none()
            && !is_success(Some(result))
            && self.has_unfinished_dependent(index)
        {
            supervisor::record_stop(StopReason::FailurePolicy);
        }
    }

    fn fill_unscheduled(&mut self) {
        for &index in self.order {
            if !matches!(
                self.slots[index].phase,
                NodePhase::Pending | NodePhase::Ready { .. }
            ) {
                continue;
            }
            if let Some(stop) = supervisor::take_task_stop(index) {
                self.record_stop(index, stop);
            }
        }
    }

    fn cancel_unfinished(&mut self) {
        for index in 0..self.slots.len() {
            if self.slots[index].result.is_some() {
                continue;
            }
            self.record_stop(
                index,
                supervisor::TaskStop {
                    outcome: supervisor::Outcome::Cancelled,
                    reason: Some(TaskReason::GroupCancel),
                },
            );
        }
    }

    fn into_result(mut self, started: Instant, stopped: Option<StopReason>) -> WorkflowResult {
        let mut result = WorkflowResult::new(self.id, self.policy, self.failure_policy);
        let elapsed_us = started.elapsed().as_micros().min(i64::MAX as u128) as i64;
        result.duration_us = elapsed_us;
        result.duration_ms = elapsed_us / 1000;
        result.max_concurrency = self.window;
        result.started_tasks = self.started_tasks;
        result.output_truncated = self.output_truncated;
        result.order = self
            .order
            .iter()
            .map(|&index| self.tasks[index].base.id.clone())
            .collect();

        let mut attempts_total = 0usize;
        let mut skipped_tasks = 0usize;
        let mut blocked_tasks = 0usize;

        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            let task_spec = &self.tasks[index];
            let base = slot.result.take().unwrap_or_else(|| {
                unknown_result(
                    task_spec.base.id.clone(),
                    &effective_cwd(&task_spec.base, &self.session_cwd),
                )
            });
            attempts_total += slot.attempts;
            match base.state {
                TaskState::Skipped => skipped_tasks += 1,
                TaskState::Blocked => blocked_tasks += 1,
                _ => {}
            }
            let detail = std::mem::take(&mut slot.attempts_detail);
            result.tasks.push(WorkflowTaskResult {
                base,
                depends_on: dep_ids(task_spec, self.tasks),
                lock: task_spec.lock.clone(),
                condition: task_spec
                    .condition
                    .as_ref()
                    .map(|condition| condition_echo(condition, slot.condition_outcome)),
                attempts: slot.attempts,
                attempts_detail: detail,
            });
        }

        result.attempts_total = attempts_total;
        result.skipped_tasks = skipped_tasks;
        result.blocked_tasks = blocked_tasks;
        result.status = aggregate_status(self.order, &result.tasks);
        result.state = match stopped {
            Some(StopReason::CancelGroup) => GroupState::Cancelled,
            Some(StopReason::FailFast | StopReason::FailurePolicy) => GroupState::Failed,
            _ => GroupState::Completed,
        };
        result
    }
}

// -------------------------------------------------------------------- helpers

fn is_success(result: Option<&TaskResult>) -> bool {
    result.is_some_and(|result| {
        result.state == TaskState::Completed
            && result.termination == Termination::Exited
            && result.status == 0
    })
}

fn retry_outcome_class(result: &TaskResult) -> Option<RetryOn> {
    match (result.state, result.termination, result.status) {
        (TaskState::Completed, Termination::Exited, status) if status != 0 => {
            Some(RetryOn::Failure)
        }
        (TaskState::Completed, Termination::Signaled, _) => Some(RetryOn::Failure),
        (TaskState::Timeout, _, _) => Some(RetryOn::Timeout),
        (TaskState::SpawnError, _, _) => Some(RetryOn::SpawnError),
        _ => None,
    }
}

fn retry_delay_ms(policy: &RetryPolicy, completed_attempts: usize, max_backoff_ms: usize) -> i64 {
    let cap = i64::try_from(max_backoff_ms).unwrap_or(i64::MAX);
    let raw = match policy.backoff {
        Backoff::Fixed => policy.backoff_ms,
        Backoff::Exponential => {
            let shift = u32::try_from(completed_attempts.saturating_sub(1)).unwrap_or(62);
            let factor = 1i64.checked_shl(shift.min(62)).unwrap_or(i64::MAX);
            policy.backoff_ms.saturating_mul(factor)
        }
    };
    raw.clamp(0, cap)
}

fn evaluate_condition(condition: &Condition, dep: Option<&TaskResult>) -> bool {
    let Some(dep) = dep else {
        return false;
    };
    if let Some(matcher) = &condition.exit_code {
        let matched = match matcher {
            ExitCodeMatcher::Eq(expected) => dep.status == *expected,
            ExitCodeMatcher::Ne(unexpected) => dep.status != *unexpected,
            ExitCodeMatcher::In(codes) => codes.contains(&dep.status),
        };
        if !matched {
            return false;
        }
    }
    if let Some(needle) = &condition.stdout_contains {
        let StreamData::Utf8(text) = &dep.stdout.data else {
            return false;
        };
        if !text.contains(needle) {
            return false;
        }
    }
    if let Some(expected) = &condition.stdout_equals {
        let StreamData::Utf8(text) = &dep.stdout.data else {
            return false;
        };
        if text != expected {
            return false;
        }
    }
    if let Some(needle) = &condition.stderr_contains {
        let StreamData::Utf8(text) = &dep.stderr.data else {
            return false;
        };
        if !text.contains(needle) {
            return false;
        }
    }
    true
}

fn dep_ids(task: &WorkflowTask, all: &[WorkflowTask]) -> Vec<String> {
    task.depends_on
        .iter()
        .map(|&dep| all[dep].base.id.clone())
        .collect()
}

fn condition_echo(condition: &Condition, outcome: Option<bool>) -> ConditionEcho {
    ConditionEcho {
        task: condition.task.clone(),
        exit_code: condition.exit_code.clone(),
        stdout_contains: condition.stdout_contains.clone(),
        stdout_equals: condition.stdout_equals.clone(),
        stderr_contains: condition.stderr_contains.clone(),
        outcome,
    }
}

/// The aggregate status rule (`workflow-executor-design.md` §2.4):
/// real failures in `order` outrank left-out (`skipped`/`blocked`) nodes so a
/// consequence never masks its upstream cause.
fn aggregate_status(order: &[usize], tasks: &[WorkflowTaskResult]) -> i64 {
    if let Some(failed) = order.iter().map(|&idx| &tasks[idx].base).find(|task| {
        matches!(
            task.state,
            TaskState::Timeout | TaskState::Cancelled | TaskState::SpawnError
        ) || (task.state == TaskState::Completed
            && (task.termination != Termination::Exited || task.status != 0))
    }) {
        return failed.status;
    }
    if let Some(leftout) = order
        .iter()
        .map(|&idx| &tasks[idx].base)
        .find(|task| matches!(task.state, TaskState::Skipped | TaskState::Blocked))
    {
        return leftout.status;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, state: TaskState, termination: Termination, status: i64) -> WorkflowTaskResult {
        let mut base = TaskResult::new(id.to_string(), "/");
        base.state = state;
        base.termination = termination;
        base.status = status;
        WorkflowTaskResult {
            base,
            depends_on: Vec::new(),
            lock: None,
            condition: None,
            attempts: 0,
            attempts_detail: Vec::new(),
        }
    }

    #[test]
    fn aggregate_status_prefers_failures_in_topological_order_before_leftouts() {
        let tasks = vec![
            node("dep_skipped", TaskState::Skipped, Termination::Skipped, 125),
            node("leaf_fail", TaskState::Completed, Termination::Exited, 7),
            node("root_ok", TaskState::Completed, Termination::Exited, 0),
        ];
        // Request order puts `dep_skipped` first, while topological order puts `root_ok`, `leaf_fail`, `dep_skipped`.
        assert_eq!(aggregate_status(&[2, 1, 0], &tasks), 7);
    }

    #[test]
    fn aggregate_status_falls_back_to_leftouts_then_zero() {
        let blocked = vec![
            node("b", TaskState::Blocked, Termination::Cancelled, 130),
            node("a", TaskState::Skipped, Termination::Skipped, 125),
        ];
        assert_eq!(aggregate_status(&[1, 0], &blocked), 125);

        let clean = vec![
            node("a", TaskState::Completed, Termination::Exited, 0),
            node("b", TaskState::Completed, Termination::Exited, 0),
        ];
        assert_eq!(aggregate_status(&[0, 1], &clean), 0);
    }

    #[test]
    fn exponential_retry_delay_grows_by_power_of_two_and_is_capped() {
        let policy = RetryPolicy {
            max_attempts: 5,
            on: vec![RetryOn::Timeout],
            backoff_ms: 1000,
            backoff: Backoff::Exponential,
        };
        assert_eq!(retry_delay_ms(&policy, 1, 60_000), 1000);
        assert_eq!(retry_delay_ms(&policy, 2, 60_000), 2000);
        assert_eq!(retry_delay_ms(&policy, 3, 60_000), 4000);
        assert_eq!(retry_delay_ms(&policy, 7, 60_000), 60_000);
    }
}
