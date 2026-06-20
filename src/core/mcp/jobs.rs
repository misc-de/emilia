//! Background-job registry for long-running MCP tools (downloads).
//!
//! Download tools answer immediately with a `job_id` and run the actual work on
//! a detached thread; `list_jobs` reports progress. The registry lives in the
//! [`McpContext`](super::McpContext) (held by the UI across server restarts).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// State of one background job.
#[derive(Clone, Copy, PartialEq, Eq)]
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
}

impl Jobs {
    /// Registers a new running job and returns its id.
    pub fn start(&self, kind: &str, label: &str) -> u64 {
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
        id
    }

    /// Marks a job finished: `Ok(detail)` → Done, `Err(msg)` → Error.
    pub fn finish(&self, id: u64, result: Result<String, String>) {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            match result {
                Ok(detail) => {
                    job.state = JobState::Done;
                    job.detail = Some(detail);
                }
                Err(msg) => {
                    job.state = JobState::Error;
                    job.detail = Some(msg);
                }
            }
        }
    }

    /// Snapshot of all jobs, newest first.
    pub fn snapshot(&self) -> Vec<Job> {
        let list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.iter().rev().cloned().collect()
    }
}
