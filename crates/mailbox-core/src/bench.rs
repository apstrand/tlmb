//! The §10 performance budgets, measured (§14.5).
//!
//! > Builds fail on regression against the last known-good measurement
//! > (default: 10%), not on the absolute budget, so the numbers stay honest
//! > instead of being retuned whenever they hurt.
//!
//! Two consequences of that sentence shape this module. Results are *recorded*
//! rather than merely asserted, so there is a last known-good to compare
//! against. And every measurement reports a median over repeated runs rather
//! than a single sample, because a single sample on a shared machine measures
//! the machine.
//!
//! What is *not* here is as deliberate: the cold-start budget cannot be
//! measured from inside the process that is starting, so it belongs to the
//! harness that spawns it, and the TUI, socket, and idle-CPU budgets belong to
//! milestones that have not built those things yet. A benchmark that reported a
//! number for something it cannot see would be worse than one that says it
//! cannot see it.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::store::read::{ListOptions, Sort};
use crate::store::Store;

/// One measured operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    /// The §10 row this measures, e.g. `structured_search`.
    pub name: String,
    /// Median wall time in milliseconds.
    pub median_ms: f64,
    /// Slowest of the samples, which is what a user actually notices.
    pub worst_ms: f64,
    pub samples: u32,
    /// The §10 budget, where §10 states one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_ms: Option<f64>,
    /// Rows the operation returned, so a measurement that quietly matched
    /// nothing is visible rather than fast.
    pub rows: u64,
}

impl Measurement {
    pub fn within_budget(&self) -> bool {
        self.budget_ms.is_none_or(|b| self.median_ms <= b)
    }
}

/// A whole run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    /// Messages in the store the run measured.
    pub messages: u64,
    pub measurements: Vec<Measurement>,
}

impl BenchReport {
    pub fn get(&self, name: &str) -> Option<&Measurement> {
        self.measurements.iter().find(|m| m.name == name)
    }

    /// Compares against a recorded baseline, returning the regressions.
    ///
    /// `tolerance` is a fraction: 0.10 is §14.5's default ten percent. A
    /// measurement missing from the baseline is not a regression — a new
    /// benchmark should not fail the build that introduces it.
    pub fn regressions(&self, baseline: &BenchReport, tolerance: f64) -> Vec<Regression> {
        let mut out = Vec::new();
        for current in &self.measurements {
            let Some(previous) = baseline.get(&current.name) else {
                continue;
            };
            // A floor, because a sub-millisecond operation swings by more than
            // ten percent from scheduling noise alone and gating on that trains
            // everyone to ignore the suite.
            let allowed = (previous.median_ms * (1.0 + tolerance)).max(previous.median_ms + 1.0);
            if current.median_ms > allowed {
                out.push(Regression {
                    name: current.name.clone(),
                    baseline_ms: previous.median_ms,
                    current_ms: current.median_ms,
                    allowed_ms: allowed,
                });
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct Regression {
    pub name: String,
    pub baseline_ms: f64,
    pub current_ms: f64,
    pub allowed_ms: f64,
}

impl std::fmt::Display for Regression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {:.2} ms against a baseline of {:.2} ms (allowed {:.2} ms)",
            self.name, self.current_ms, self.baseline_ms, self.allowed_ms
        )
    }
}

/// Runs the budgets this milestone can measure.
pub fn run(store: &Store, samples: u32) -> Result<BenchReport> {
    let messages = store.message_count()?;
    let clock = store.clock().clone();
    let mut measurements = Vec::new();

    // §10: structured search, < 30 ms.
    measurements.push(measure("structured_search", Some(30.0), samples, || {
        let node = crate::query::parse("is:unread from:alice", &clock)?;
        let page = store.search(&node, &ListOptions::default())?;
        Ok(page.messages.len() as u64)
    })?);

    // §10: full-text search, first page, < 100 ms.
    //
    // Against a term about one message in a hundred carries, which is what an
    // ordinary search looks like. The generated corpus plants it deliberately:
    // a measurement against a word that appears in most of the mailbox
    // measures sorting, not search.
    measurements.push(measure("full_text_search", Some(100.0), samples, || {
        let node = crate::query::parse(crate::store::corpus::NEEDLE_TERM, &clock)?;
        let page = store.search(
            &node,
            &ListOptions {
                sort: Sort::DateDesc,
                ..Default::default()
            },
        )?;
        Ok(page.messages.len() as u64)
    })?);

    // The worst case, reported without a budget rather than hidden: a term a
    // quarter of the mailbox carries. §10's budget is about search; this number
    // is about what happens when a search is really a scan, and it is the one
    // that would send §16 back to its "reopen if" clause.
    measurements.push(measure("full_text_common_term", None, samples, || {
        let node = crate::query::parse(crate::store::corpus::FREQUENT_TERM, &clock)?;
        let page = store.search(
            &node,
            &ListOptions {
                sort: Sort::DateDesc,
                ..Default::default()
            },
        )?;
        Ok(page.messages.len() as u64)
    })?);

    // The default listing, which is what `ls` runs once the process is up. Its
    // §10 budget is a cold *process* start, measured by the harness, so this is
    // the query half of it and carries no budget of its own.
    measurements.push(measure("inbox_listing", None, samples, || {
        let node = crate::query::parse("in:inbox", &clock)?;
        let page = store.search(
            &node,
            &ListOptions {
                sort: Sort::UnreadFirst,
                ..Default::default()
            },
        )?;
        Ok(page.messages.len() as u64)
    })?);

    // §10: open a cached thread, < 50 ms.
    let sample_thread = store
        .search(
            &crate::query::parse("", &clock)?,
            &ListOptions {
                limit: 1,
                ..Default::default()
            },
        )?
        .messages
        .first()
        .map(|m| m.thread_id.clone());
    if let Some(thread_id) = sample_thread {
        measurements.push(measure("open_thread", Some(50.0), samples, || {
            let thread = store.thread(&thread_id)?;
            Ok(u64::from(thread.message_count))
        })?);
    }

    // Paging deep into a large result set. Not a §10 row, but the operation a
    // cursor exists for, and the one that degrades first if an index is lost.
    measurements.push(measure("deep_page", None, samples, || {
        let node = crate::query::parse("", &clock)?;
        let mut opts = ListOptions {
            limit: 50,
            sort: Sort::DateDesc,
            ..Default::default()
        };
        let mut rows = 0u64;
        for _ in 0..5 {
            let page = store.search(&node, &opts)?;
            rows += page.messages.len() as u64;
            match page.cursor {
                Some(c) => opts.cursor = Some(c),
                None => break,
            }
        }
        Ok(rows)
    })?);

    Ok(BenchReport {
        messages,
        measurements,
    })
}

fn measure(
    name: &str,
    budget_ms: Option<f64>,
    samples: u32,
    mut op: impl FnMut() -> Result<u64>,
) -> Result<Measurement> {
    // One untimed run first: the first query of a session pays for SQLite's
    // page cache and statement preparation, which is a real cost but a
    // different one from the budget being measured.
    let rows = op()?;

    let mut times = Vec::with_capacity(samples as usize);
    for _ in 0..samples.max(1) {
        let started = std::time::Instant::now();
        op()?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    Ok(Measurement {
        name: name.to_string(),
        median_ms: times[times.len() / 2],
        worst_ms: *times.last().unwrap_or(&0.0),
        samples: times.len() as u32,
        budget_ms,
        rows,
    })
}
