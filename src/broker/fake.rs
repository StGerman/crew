//! In-memory [`TrackerWrites`], recording every write in order.
//!
//! The broker's own tests assert on *counts* as much as content — "exactly one tracker write"
//! is the acceptance criterion for the comment path, and a fake that only remembered the last
//! call could not tell one write from three.

use std::sync::Mutex;

use super::writes::TrackerWrites;
use crate::tracker::TrackerError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    Comment { issue_id: String, body: String },
    SetState { issue_id: String, state: String },
    LinkPr { issue_id: String, url: String },
}

impl Write {
    pub fn issue_id(&self) -> &str {
        match self {
            Write::Comment { issue_id, .. }
            | Write::SetState { issue_id, .. }
            | Write::LinkPr { issue_id, .. } => issue_id,
        }
    }
}

#[derive(Default)]
pub struct FakeWrites {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    writes: Vec<Write>,
    fail: Option<TrackerError>,
}

impl FakeWrites {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn writes(&self) -> Vec<Write> {
        self.inner.lock().unwrap().writes.clone()
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().writes.len()
    }

    /// Make every subsequent write fail, for the "a tool failure is not a run failure" path.
    pub fn fail_with(&self, e: Option<TrackerError>) {
        self.inner.lock().unwrap().fail = e;
    }

    fn record(&self, w: Write) -> Result<String, TrackerError> {
        let mut g = self.inner.lock().unwrap();
        if let Some(e) = &g.fail {
            return Err(e.clone());
        }
        g.writes.push(w);
        Ok("recorded".to_string())
    }
}

impl TrackerWrites for FakeWrites {
    fn comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError> {
        self.record(Write::Comment { issue_id: issue_id.into(), body: body.into() })
    }

    fn set_state(&self, issue_id: &str, state: &str) -> Result<String, TrackerError> {
        self.record(Write::SetState { issue_id: issue_id.into(), state: state.into() })
    }

    fn link_pr(&self, issue_id: &str, url: &str) -> Result<String, TrackerError> {
        self.record(Write::LinkPr { issue_id: issue_id.into(), url: url.into() })
    }
}
