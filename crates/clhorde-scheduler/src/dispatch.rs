//! Pure decision helpers for the apply phase.
//!
//! Everything in this module is side-effect free — input is a `Dag` and
//! some bookkeeping sets, output is a `Vec<usize>` of indices the caller
//! should fire next. The orchestrator is the only thing that mutates state
//! and talks to the daemon; pulling these decisions out makes them trivial
//! to unit-test without spinning up an Orchestrator at all.

use std::collections::HashSet;

use crate::openspec::annotations::{AnnotatedSection, Granularity};
use crate::openspec::dag::Dag;

/// Indices of DAG nodes whose dependencies are all in `completed` and
/// which are not themselves in `completed` or `dispatched`. Sorted by
/// node index for stable test output.
pub fn next_runnable_nodes(
    dag: &Dag,
    completed: &HashSet<usize>,
    dispatched: &HashSet<usize>,
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for (i, node) in dag.nodes.iter().enumerate() {
        if completed.contains(&i) || dispatched.contains(&i) {
            continue;
        }
        if node.deps.iter().all(|d| completed.contains(d)) {
            out.push(i);
        }
    }
    out
}

/// True iff every leaf task of the given section id is checked.
///
/// "Leaf" means: when the section has subsections, only the lowest-level
/// items count. Here we keep it simple: every `TaskItem` in
/// `section.items` must be checked. Subsection traversal can be added when
/// the parser starts treating subsections as their own sections.
pub fn is_section_done(section_id: &str, sections: &[AnnotatedSection]) -> bool {
    let Some(section) = sections.iter().find(|s| s.section.id == section_id) else {
        return false;
    };
    !section.items.is_empty() && section.items.iter().all(|t| t.task.done)
}

/// True iff the task with the given dotted id is checked. Searches every
/// section because tasks live inside their parent sections in the parser
/// output.
pub fn is_task_done(task_id: &str, sections: &[AnnotatedSection]) -> bool {
    sections
        .iter()
        .flat_map(|s| s.items.iter())
        .any(|t| t.task.id == task_id && t.task.done)
}

/// True iff a DAG node is "done" against the latest parsed `tasks.md`.
/// The check varies with granularity so callers don't have to branch on it.
pub fn is_node_done(
    dag: &Dag,
    node_idx: usize,
    sections: &[AnnotatedSection],
) -> bool {
    let Some(node) = dag.nodes.get(node_idx) else {
        return false;
    };
    match dag.granularity {
        Granularity::Section => is_section_done(&node.id, sections),
        Granularity::Task => is_task_done(&node.id, sections),
        // Phase granularity is one node representing the whole apply phase —
        // it's done when every task across every section is checked.
        Granularity::Phase => {
            !sections.is_empty()
                && sections
                    .iter()
                    .flat_map(|s| s.items.iter())
                    .all(|t| t.task.done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openspec::annotations::annotate;
    use crate::openspec::dag;
    use crate::openspec::tasks_parser;

    fn dag_from(md: &str) -> Dag {
        let graph = tasks_parser::parse(md);
        let annotated = annotate(graph);
        dag::build(&annotated).unwrap()
    }

    fn sections_from(md: &str) -> Vec<AnnotatedSection> {
        let graph = tasks_parser::parse(md);
        annotate(graph)
    }

    fn s(items: &[usize]) -> HashSet<usize> {
        items.iter().copied().collect()
    }

    // ── next_runnable_nodes ──

    #[test]
    fn initial_runnable_set_is_only_root_nodes() {
        let dag = dag_from(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C
- [ ] 3.1 c
",
        );
        let runnable = next_runnable_nodes(&dag, &s(&[]), &s(&[]));
        // Default sequential: only node 0 has no deps.
        assert_eq!(runnable, vec![0]);
    }

    #[test]
    fn runnable_advances_as_each_predecessor_completes() {
        let dag = dag_from(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C
- [ ] 3.1 c
",
        );
        assert_eq!(next_runnable_nodes(&dag, &s(&[0]), &s(&[])), vec![1]);
        assert_eq!(next_runnable_nodes(&dag, &s(&[0, 1]), &s(&[])), vec![2]);
        assert_eq!(next_runnable_nodes(&dag, &s(&[0, 1, 2]), &s(&[])), Vec::<usize>::new());
    }

    #[test]
    fn dispatched_nodes_are_skipped() {
        let dag = dag_from(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
",
        );
        // Already dispatched node 0; runnable should now be empty until it
        // completes.
        let runnable = next_runnable_nodes(&dag, &s(&[]), &s(&[0]));
        assert!(runnable.is_empty());
    }

    #[test]
    fn parallel_independents_run_concurrently() {
        let dag = dag_from(
            "\
## 1. A
- [ ] 1.1 a
## 2. B <!-- clhorde: depends 1 -->
- [ ] 2.1 b
## 3. C <!-- clhorde: depends 1 -->
- [ ] 3.1 c
",
        );
        // After section 1 completes, both 2 and 3 are runnable in one wave.
        let runnable = next_runnable_nodes(&dag, &s(&[0]), &s(&[]));
        assert_eq!(runnable, vec![1, 2]);
    }

    #[test]
    fn already_completed_node_is_not_re_listed() {
        let dag = dag_from(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
",
        );
        // node 0 marked completed. Even with no dispatch state, node 0 must
        // not surface as runnable.
        let runnable = next_runnable_nodes(&dag, &s(&[0]), &s(&[]));
        assert_eq!(runnable, vec![1]);
    }

    #[test]
    fn fan_in_waits_for_every_predecessor() {
        let dag = dag_from(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C <!-- clhorde: depends 1, 2 -->
- [ ] 3.1 c
",
        );
        // Only one predecessor done — node 2 (C) still blocked.
        assert!(next_runnable_nodes(&dag, &s(&[0]), &s(&[1])).is_empty());
        // Both done — C runs.
        assert_eq!(next_runnable_nodes(&dag, &s(&[0, 1]), &s(&[])), vec![2]);
    }

    // ── is_section_done / is_task_done ──

    #[test]
    fn section_with_all_tasks_checked_is_done() {
        let sections = sections_from("## 1. A\n- [x] 1.1 a\n- [x] 1.2 b\n");
        assert!(is_section_done("1", &sections));
    }

    #[test]
    fn section_with_one_unchecked_task_is_not_done() {
        let sections = sections_from("## 1. A\n- [x] 1.1 a\n- [ ] 1.2 b\n");
        assert!(!is_section_done("1", &sections));
    }

    #[test]
    fn empty_section_is_not_done() {
        // A section header with no checkable items should not appear "done"
        // — we'd have nothing to verify.
        let sections = sections_from("## 1. A\n");
        assert!(!is_section_done("1", &sections));
    }

    #[test]
    fn missing_section_id_is_not_done() {
        let sections = sections_from("## 1. A\n- [x] 1.1 a\n");
        assert!(!is_section_done("99", &sections));
    }

    #[test]
    fn task_done_lookup_works_across_sections() {
        let sections = sections_from(
            "\
## 1. A
- [x] 1.1 done
- [ ] 1.2 pending
## 2. B
- [x] 2.1 also done
",
        );
        assert!(is_task_done("1.1", &sections));
        assert!(!is_task_done("1.2", &sections));
        assert!(is_task_done("2.1", &sections));
        assert!(!is_task_done("nope", &sections));
    }

    // ── is_node_done across granularities ──

    #[test]
    fn node_done_section_granularity() {
        let dag = dag_from("## 1. A\n- [ ] 1.1 a\n## 2. B\n- [ ] 2.1 b\n");

        let unchecked = sections_from("## 1. A\n- [ ] 1.1 a\n## 2. B\n- [ ] 2.1 b\n");
        assert!(!is_node_done(&dag, 0, &unchecked));

        let one_done = sections_from("## 1. A\n- [x] 1.1 a\n## 2. B\n- [ ] 2.1 b\n");
        assert!(is_node_done(&dag, 0, &one_done));
        assert!(!is_node_done(&dag, 1, &one_done));
    }

    #[test]
    fn node_done_task_granularity() {
        let dag = dag_from(
            "\
## 1. A <!-- clhorde: granularity task -->
- [ ] 1.1 a
- [ ] 1.2 b
",
        );
        assert_eq!(dag.granularity, Granularity::Task);

        let one_done = sections_from(
            "\
## 1. A
- [x] 1.1 a
- [ ] 1.2 b
",
        );
        assert!(is_node_done(&dag, 0, &one_done));
        assert!(!is_node_done(&dag, 1, &one_done));
    }

    #[test]
    fn node_done_phase_granularity() {
        let dag = dag_from(
            "\
## 1. A <!-- clhorde: granularity phase -->
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
",
        );
        assert_eq!(dag.granularity, Granularity::Phase);

        let half = sections_from(
            "\
## 1. A
- [x] 1.1 a
## 2. B
- [ ] 2.1 b
",
        );
        assert!(!is_node_done(&dag, 0, &half));

        let all = sections_from(
            "\
## 1. A
- [x] 1.1 a
## 2. B
- [x] 2.1 b
",
        );
        assert!(is_node_done(&dag, 0, &all));
    }
}
