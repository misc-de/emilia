//! Background-job registry for long-running MCP tools (downloads).
//!
//! Download tools answer immediately with a `job_id` and run the actual work on
//! a detached thread; `list_jobs` reports progress. The registry lives in the
//! [`McpContext`](super::McpContext) (held by the UI across server restarts).
//! At most [`MAX_RUNNING`] jobs run at once, so a remote client cannot start
//! an unbounded number of worker threads.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

/// Upper bound on concurrently running jobs.
pub const MAX_RUNNING: usize = 3;

/// State of one background job.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobState {
    Running,
    Done,
    Error,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Error => "error",
        }
    }
}

/// One tracked background job.
#[derive(Clone)]
pub struct Job {
    pub id: u64,
    /// Job kind, e.g. `"youtube_download"` | `"episode_download"`.
    pub kind: String,
    /// Human label — what is being downloaded.
    pub label: String,
    pub state: JobState,
    /// On completion: a result path/message, or the error string.
    pub detail: Option<String>,
}

/// Thread-safe registry of background jobs.
#[derive(Default)]
pub struct Jobs {
    seq: AtomicU64,
    list: Mutex<Vec<Job>>,
    /// Jobs started but not yet finished (one slot per live [`JobHandle`]).
    running: AtomicUsize,
}

impl Jobs {
    /// Registers a new running job, or fails when [`MAX_RUNNING`] jobs are
    /// already running. The returned handle owns the slot: dropping it (also
    /// while unwinding from a panic) frees the slot and, without an explicit
    /// [`JobHandle::finish`], marks the job as failed.
    pub fn try_start(self: &Arc<Self>, kind: &str, label: &str) -> Result<JobHandle> {
        let mut n = self.running.load(Ordering::Acquire);
        loop {
            if n >= MAX_RUNNING {
                return Err(anyhow!(
                    "too many jobs running (max {MAX_RUNNING}); try again once one has finished (see list_jobs)"
                ));
            }
            match self
                .running
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(current) => n = current,
            }
        }
        let id = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.push(Job {
            id,
            kind: kind.to_string(),
            label: label.to_string(),
            state: JobState::Running,
            detail: None,
        });
        // Keep the registry bounded — drop the oldest entry past the cap.
        if list.len() > 100 {
            list.remove(0);
        }
        Ok(JobHandle {
            jobs: self.clone(),
            id,
            result: None,
        })
    }

    /// Records a job's outcome: `Ok(detail)` → Done, `Err(msg)` → Error.
    fn record(&self, id: u64, result: Result<String, String>) {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            let (state, detail) = match result {
                Ok(detail) => (JobState::Done, detail),
                Err(msg) => (JobState::Error, msg),
            };
            job.state = state;
            job.detail = Some(detail);
        }
    }

    /// Number of jobs currently running.
    #[cfg(test)]
    fn running(&self) -> usize {
        self.running.load(Ordering::Acquire)
    }

    /// Snapshot of all jobs, newest first.
    pub fn snapshot(&self) -> Vec<Job> {
        let list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.iter().rev().cloned().collect()
    }
}

/// A running job's slot. The outcome is recorded and the slot freed on drop,
/// so a panicking worker still leaves the job as "error".
pub struct JobHandle {
    jobs: Arc<Jobs>,
    id: u64,
    result: Option<Result<String, String>>,
}

impl JobHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Marks the job finished with `result` and frees its slot.
    pub fn finish(mut self, result: Result<String, String>) {
        self.result = Some(result);
    }

    /// Runs `work` on a detached thread and records its result. A panic in
    /// `work` marks the job as failed.
    pub fn spawn(
        self,
        work: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) -> Result<()> {
        std::thread::Builder::new()
            .name("mcp-job".into())
            .spawn(move || {
                let result = crate::core::panic_guard::catch_or("MCP job", work, || {
                    Err("job failed: internal error".to_string())
                });
                self.finish(result);
            })
            .map(drop)
            .map_err(|e| anyhow!("could not start the job: {e}"))
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err("job failed: aborted unexpectedly".to_string()));
        self.jobs.record(self.id, result);
        self.jobs.running.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_of(jobs: &Jobs, id: u64) -> (JobState, Option<String>) {
        let j = jobs.snapshot().into_iter().find(|j| j.id == id).unwrap();
        (j.state, j.detail)
    }

    /// Waits (bounded) until no job is running any more.
    fn wait_idle(jobs: &Jobs) {
        for _ in 0..500 {
            if jobs.running() == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn limit_rejects_beyond_max_and_frees_on_finish() {
        let jobs = Arc::new(Jobs::default());
        let mut handles: Vec<JobHandle> = (0..MAX_RUNNING)
            .map(|i| jobs.try_start("test", &i.to_string()).unwrap())
            .collect();
        assert_eq!(jobs.running(), MAX_RUNNING);
        let err = jobs.try_start("test", "one too many").err().unwrap();
        assert!(err.to_string().contains("too many jobs running"));

        let h = handles.pop().unwrap();
        let id = h.id();
        h.finish(Ok("fine".into()));
        assert_eq!(jobs.running(), MAX_RUNNING - 1);
        assert_eq!(state_of(&jobs, id), (JobState::Done, Some("fine".into())));
        assert!(jobs.try_start("test", "fits again").is_ok());
    }

    #[test]
    fn panicking_job_is_marked_failed_and_frees_its_slot() {
        let jobs = Arc::new(Jobs::default());
        let h = jobs.try_start("test", "boom").unwrap();
        let id = h.id();
        h.spawn(|| panic!("worker exploded")).unwrap();
        wait_idle(&jobs);
        assert_eq!(jobs.running(), 0);
        assert_eq!(state_of(&jobs, id).0, JobState::Error);
    }

    #[test]
    fn dropped_handle_without_result_is_marked_failed() {
        let jobs = Arc::new(Jobs::default());
        let id = jobs.try_start("test", "dropped").unwrap().id();
        assert_eq!(jobs.running(), 0);
        assert_eq!(state_of(&jobs, id).0, JobState::Error);
    }

    #[test]
    fn spawned_job_records_success() {
        let jobs = Arc::new(Jobs::default());
        let h = jobs.try_start("test", "ok").unwrap();
        let id = h.id();
        h.spawn(|| Ok("done".into())).unwrap();
        wait_idle(&jobs);
        assert_eq!(state_of(&jobs, id), (JobState::Done, Some("done".into())));
    }
}
