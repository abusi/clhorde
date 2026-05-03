//! Inter-workflow dependency evaluator.
//!
//! Pure function over a snapshot of workflows. The orchestrator consults it
//! before transitioning a `Queued` workflow into `Implementing`, so a workflow
//! whose `depends_on` list isn't fully archived stays held without any new
//! state-machine variant.
//!
//! [`DepEvaluation`] is a three-way result:
//! - [`DepEvaluation::Satisfied`]: every dep is `Archived` (or no deps at all).
//! - [`DepEvaluation::Pending`]: at least one dep exists but isn't `Archived`
//!   yet. Carries the sorted list of blocking dep names so callers can surface
//!   "blocked by X, Y" without recomputing.
//! - [`DepEvaluation::Failed`]: a dep is unrecoverable — missing entirely,
//!   `Cancelled`, `Failed`, or part of a cycle. Carries a human-readable
//!   reason suitable for `Workflow::fail`.
//!
//! Cycle detection only flags cycles that include the workflow being
//! evaluated. A cycle elsewhere in the graph (B → C → B, while A depends_on B)
//! leaves A in `Pending`; B and C get their own `Failed` verdicts when each
//! is evaluated in turn.

use std::collections::{BTreeMap, HashSet};

use crate::workflow::{Workflow, WorkflowStatus};

/// Verdict of evaluating a workflow's `depends_on` list against the current
/// orchestrator view.
#[derive(Debug, Clone, PartialEq)]
pub enum DepEvaluation {
    /// Every dep is `Archived`, or the workflow has no deps. Caller may
    /// proceed with `start_implementing`.
    Satisfied,
    /// At least one dep exists and is in a non-terminal, non-`Archived`
    /// state. Caller should keep the workflow in `Queued`. The `Vec` lists
    /// blocking dep names, sorted and deduplicated.
    Pending(Vec<String>),
    /// At least one dep is unrecoverable (missing, `Cancelled`, `Failed`, or
    /// part of a cycle including this workflow). Caller should fail the
    /// workflow with the carried reason.
    Failed(String),
}

/// Evaluate `me`'s `depends_on` list against `others` (the orchestrator's
/// current `workflows` map; `me` itself is *not* expected to appear in
/// `others`).
///
/// The order of checks is deliberate: cycle detection first so a graph that
/// is structurally broken reports the cycle rather than a downstream "not
/// found"/"cancelled" symptom; then per-dep status walk in
/// `me.metadata.depends_on` order so the reported reason is stable across
/// runs.
pub fn evaluate(me: &Workflow, others: &BTreeMap<String, Workflow>) -> DepEvaluation {
    if me.metadata.depends_on.is_empty() {
        return DepEvaluation::Satisfied;
    }

    if let Some(path) = detect_cycle(me, others) {
        return DepEvaluation::Failed(format!(
            "dependency cycle: {}",
            path.join(" → ")
        ));
    }

    let mut pending: Vec<String> = Vec::new();
    for dep in &me.metadata.depends_on {
        match others.get(dep) {
            None => {
                return DepEvaluation::Failed(format!(
                    "dependency '{dep}' not found"
                ));
            }
            Some(d) => match &d.status {
                WorkflowStatus::Archived => {}
                WorkflowStatus::Cancelled => {
                    return DepEvaluation::Failed(format!(
                        "dependency '{dep}' was cancelled"
                    ));
                }
                WorkflowStatus::Failed { reason } => {
                    return DepEvaluation::Failed(format!(
                        "dependency '{dep}' failed: {reason}"
                    ));
                }
                _ => pending.push(dep.clone()),
            },
        }
    }

    if pending.is_empty() {
        DepEvaluation::Satisfied
    } else {
        pending.sort();
        pending.dedup();
        DepEvaluation::Pending(pending)
    }
}

/// Returns `Some(path)` if a directed cycle in `depends_on` edges starting
/// at `me` and returning to `me.name` exists. The path is human-readable:
/// `[me.name, dep1, dep2, ..., me.name]`. Otherwise `None`.
///
/// Cycles not involving `me` are intentionally ignored — they will be
/// detected when each member is evaluated on its own. This keeps the
/// reported reason scoped to "why *I* can't proceed".
fn detect_cycle(me: &Workflow, others: &BTreeMap<String, Workflow>) -> Option<Vec<String>> {
    let mut path = vec![me.name.clone()];
    let mut on_path: HashSet<String> = [me.name.clone()].into_iter().collect();

    for dep in &me.metadata.depends_on {
        if dep == &me.name {
            path.push(dep.clone());
            return Some(path);
        }
        if !on_path.insert(dep.clone()) {
            continue;
        }
        path.push(dep.clone());
        if dfs_back_to(&me.name, dep, others, &mut on_path, &mut path) {
            return Some(path);
        }
        path.pop();
        on_path.remove(dep);
    }
    None
}

/// DFS helper: from `current`, walk its `depends_on` looking for `target`.
/// `on_path` is the set of names currently on the DFS stack; `path` is the
/// readable sequence used to render the cycle. Returns `true` (with `path`
/// extended through to `target`) if found.
fn dfs_back_to(
    target: &str,
    current: &str,
    others: &BTreeMap<String, Workflow>,
    on_path: &mut HashSet<String>,
    path: &mut Vec<String>,
) -> bool {
    let Some(wf) = others.get(current) else {
        return false;
    };
    for dep in &wf.metadata.depends_on {
        if dep == target {
            path.push(dep.to_string());
            return true;
        }
        if !on_path.insert(dep.clone()) {
            continue;
        }
        path.push(dep.clone());
        if dfs_back_to(target, dep, others, on_path, path) {
            return true;
        }
        path.pop();
        on_path.remove(dep);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openspec::discovery::MarkerMetadata;

    fn wf(name: &str, status: WorkflowStatus, deps: &[&str]) -> Workflow {
        let mut w = Workflow::drafted(name);
        w.status = status;
        w.metadata = MarkerMetadata {
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            ..MarkerMetadata::default()
        };
        w
    }

    fn map(items: Vec<Workflow>) -> BTreeMap<String, Workflow> {
        items.into_iter().map(|w| (w.name.clone(), w)).collect()
    }

    // ── Satisfied ──

    #[test]
    fn no_deps_is_satisfied() {
        let me = wf("a", WorkflowStatus::Queued, &[]);
        assert_eq!(evaluate(&me, &map(vec![])), DepEvaluation::Satisfied);
    }

    #[test]
    fn all_deps_archived_is_satisfied() {
        let me = wf("a", WorkflowStatus::Queued, &["b", "c"]);
        let others = map(vec![
            wf("b", WorkflowStatus::Archived, &[]),
            wf("c", WorkflowStatus::Archived, &[]),
        ]);
        assert_eq!(evaluate(&me, &others), DepEvaluation::Satisfied);
    }

    // ── Pending ──

    #[test]
    fn drafted_dep_is_pending() {
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![wf("b", WorkflowStatus::Drafted, &[])]);
        assert_eq!(
            evaluate(&me, &others),
            DepEvaluation::Pending(vec!["b".into()])
        );
    }

    #[test]
    fn implementing_dep_is_pending() {
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![wf("b", WorkflowStatus::Implementing, &[])]);
        assert_eq!(
            evaluate(&me, &others),
            DepEvaluation::Pending(vec!["b".into()])
        );
    }

    #[test]
    fn pending_lists_only_unsatisfied_deps_sorted() {
        let me = wf("a", WorkflowStatus::Queued, &["c", "b", "d"]);
        let others = map(vec![
            wf("b", WorkflowStatus::Queued, &[]),
            wf("c", WorkflowStatus::Archived, &[]),
            wf("d", WorkflowStatus::Verifying, &[]),
        ]);
        assert_eq!(
            evaluate(&me, &others),
            DepEvaluation::Pending(vec!["b".into(), "d".into()])
        );
    }

    // ── Failed ──

    #[test]
    fn missing_dep_fails() {
        let me = wf("a", WorkflowStatus::Queued, &["ghost"]);
        match evaluate(&me, &map(vec![])) {
            DepEvaluation::Failed(reason) => {
                assert!(reason.contains("ghost"));
                assert!(reason.contains("not found"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_dep_fails() {
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![wf("b", WorkflowStatus::Cancelled, &[])]);
        match evaluate(&me, &others) {
            DepEvaluation::Failed(reason) => {
                assert!(reason.contains("'b'"));
                assert!(reason.contains("cancelled"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn failed_dep_propagates_reason() {
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![wf(
            "b",
            WorkflowStatus::Failed {
                reason: "verify exited with code 2".into(),
            },
            &[],
        )]);
        match evaluate(&me, &others) {
            DepEvaluation::Failed(reason) => {
                assert!(reason.contains("'b'"));
                assert!(reason.contains("verify exited with code 2"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Cycles ──

    #[test]
    fn self_dep_is_a_cycle() {
        let me = wf("a", WorkflowStatus::Queued, &["a"]);
        match evaluate(&me, &map(vec![])) {
            DepEvaluation::Failed(reason) => {
                assert!(reason.contains("cycle"));
                assert!(reason.contains("a → a"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn two_workflow_cycle_detected() {
        // a → b → a
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![wf("b", WorkflowStatus::Queued, &["a"])]);
        match evaluate(&me, &others) {
            DepEvaluation::Failed(reason) => {
                assert!(reason.contains("cycle"));
                assert!(reason.contains("a → b → a"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn three_workflow_cycle_detected() {
        // a → b → c → a
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![
            wf("b", WorkflowStatus::Queued, &["c"]),
            wf("c", WorkflowStatus::Queued, &["a"]),
        ]);
        match evaluate(&me, &others) {
            DepEvaluation::Failed(reason) => {
                assert!(reason.contains("cycle"));
                assert!(reason.contains("a → b → c → a"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn cycle_not_through_me_is_pending_not_failed() {
        // me=a depends on b. b → c → b is a cycle but doesn't include a.
        // From a's POV, b is just Pending.
        let me = wf("a", WorkflowStatus::Queued, &["b"]);
        let others = map(vec![
            wf("b", WorkflowStatus::Queued, &["c"]),
            wf("c", WorkflowStatus::Queued, &["b"]),
        ]);
        assert_eq!(
            evaluate(&me, &others),
            DepEvaluation::Pending(vec!["b".into()])
        );
    }

    // ── Mixed ──

    #[test]
    fn one_failed_dep_short_circuits_even_if_others_pending() {
        let me = wf("a", WorkflowStatus::Queued, &["b", "c"]);
        let others = map(vec![
            wf("b", WorkflowStatus::Queued, &[]),
            wf("c", WorkflowStatus::Cancelled, &[]),
        ]);
        // Walk order is depends_on order: b (pending), then c (cancelled).
        // First Failed wins because the per-dep loop returns early.
        match evaluate(&me, &others) {
            DepEvaluation::Failed(reason) => assert!(reason.contains("'c'")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_deps_are_deduplicated_in_pending() {
        let me = wf("a", WorkflowStatus::Queued, &["b", "b"]);
        let others = map(vec![wf("b", WorkflowStatus::Queued, &[])]);
        assert_eq!(
            evaluate(&me, &others),
            DepEvaluation::Pending(vec!["b".into()])
        );
    }
}
