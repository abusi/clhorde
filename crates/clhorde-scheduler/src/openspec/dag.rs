//! Build an executable DAG from an annotated `TaskGraph`.
//!
//! The graph node is the unit the scheduler dispatches to a worker. The unit
//! depends on the chosen [`Granularity`]:
//!
//! - [`Granularity::Section`] (default): one node per leaf section.
//! - [`Granularity::Task`]: one node per `- [ ]` task. High prompt overhead;
//!   only useful when explicitly asked for.
//! - [`Granularity::Phase`]: one node for the entire `apply` phase. Falls
//!   back to OpenSpec's native sequential flow.
//!
//! Granularity is picked from the first section whose annotation sets it; if
//! none do, [`Granularity::Section`] is used. We deliberately avoid mixed
//! granularity — the scheduler is simpler if every node in a workflow has
//! the same shape.
//!
//! Edge derivation:
//! 1. Sequential default: section `N+1` depends on section `N` (in source
//!    order, regardless of dotted ids).
//! 2. `<!-- clhorde: depends X,Y -->` overrides the default and *replaces*
//!    the predecessor edge for the annotated section.
//! 3. `<!-- clhorde: parallel-with X -->` is informational only at this
//!    stage — it tells later phases that two sections may run concurrently.
//!    It does NOT remove sequential edges; the user should also annotate
//!    `depends` on both sides if they want a real fork-join.
//! 4. For task granularity, `<!-- clhorde: needs A.B -->` adds an edge from
//!    the named task to this one. The default within a section is sequential
//!    on declaration order.
//!
//! Cycles are rejected with [`DagError::Cycle`]. Unknown reference ids are
//! rejected with [`DagError::UnknownRef`].

use std::collections::HashMap;

use super::annotations::{AnnotatedSection, Granularity};

#[derive(Debug, Clone, PartialEq)]
pub struct Dag {
    pub granularity: Granularity,
    pub nodes: Vec<DagNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DagNode {
    /// Stable id derived from the source. For section granularity, this is
    /// the section id (e.g. `"1"`, `"2.3"`). For task granularity, the dotted
    /// task id (e.g. `"1.2"`).
    pub id: String,
    /// Human-readable label (heading title or task text).
    pub label: String,
    /// Indices of predecessor nodes in `Dag::nodes`.
    pub deps: Vec<usize>,
    /// Indices of nodes the user marked as parallelizable with this one.
    /// Informational; the scheduler may use it as a hint to relax ordering.
    pub parallel_with: Vec<usize>,
    /// Optional override of the prompt template name for this node.
    pub prompt_template: Option<String>,
    /// 1-based source line of the heading or task line. Useful for error
    /// reporting and for the scheduler to re-locate the source.
    pub source_line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DagError {
    /// One annotation pointed at a section/task id that doesn't exist.
    UnknownRef {
        from: String,
        to: String,
        kind: &'static str, // "depends" | "parallel-with" | "needs"
    },
    /// The annotations would form a directed cycle through these node ids.
    Cycle { involved: Vec<String> },
    /// A workflow with no nodes — nothing to schedule.
    Empty,
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::UnknownRef { from, to, kind } => write!(
                f,
                "{kind} annotation on {from} references unknown id {to}"
            ),
            DagError::Cycle { involved } => {
                write!(f, "cycle detected through nodes: {}", involved.join(" -> "))
            }
            DagError::Empty => write!(f, "workflow has no schedulable nodes"),
        }
    }
}

impl std::error::Error for DagError {}

/// Build a DAG from annotated sections. The chosen granularity is the first
/// non-default value found in the section annotations, or
/// [`Granularity::Section`] when all sections leave it implicit.
pub fn build(sections: &[AnnotatedSection]) -> Result<Dag, DagError> {
    let granularity = pick_granularity(sections);
    match granularity {
        Granularity::Section => build_section_dag(sections),
        Granularity::Task => build_task_dag(sections),
        Granularity::Phase => build_phase_dag(sections),
    }
}

fn pick_granularity(sections: &[AnnotatedSection]) -> Granularity {
    sections
        .iter()
        .find_map(|s| s.annotations.granularity)
        .unwrap_or_default()
}

fn build_section_dag(sections: &[AnnotatedSection]) -> Result<Dag, DagError> {
    if sections.is_empty() {
        return Err(DagError::Empty);
    }
    let id_to_idx: HashMap<&str, usize> = sections
        .iter()
        .enumerate()
        .map(|(i, s)| (s.section.id.as_str(), i))
        .collect();

    let mut nodes: Vec<DagNode> = Vec::with_capacity(sections.len());
    for (i, sec) in sections.iter().enumerate() {
        let mut deps: Vec<usize> = Vec::new();
        if !sec.annotations.depends.is_empty() {
            for dep in &sec.annotations.depends {
                let &idx = id_to_idx.get(dep.as_str()).ok_or_else(|| {
                    DagError::UnknownRef {
                        from: sec.section.id.clone(),
                        to: dep.clone(),
                        kind: "depends",
                    }
                })?;
                deps.push(idx);
            }
        } else if i > 0 {
            // Default sequential: depend on the previous section in source order.
            deps.push(i - 1);
        }

        let mut parallel_with: Vec<usize> = Vec::new();
        for p in &sec.annotations.parallel_with {
            let &idx = id_to_idx.get(p.as_str()).ok_or_else(|| {
                DagError::UnknownRef {
                    from: sec.section.id.clone(),
                    to: p.clone(),
                    kind: "parallel-with",
                }
            })?;
            parallel_with.push(idx);
        }

        nodes.push(DagNode {
            id: sec.section.id.clone(),
            label: sec.section.title.clone(),
            deps,
            parallel_with,
            prompt_template: sec.annotations.prompt_template.clone(),
            source_line: sec.section.line,
        });
    }

    detect_cycle(&nodes)?;
    Ok(Dag {
        granularity: Granularity::Section,
        nodes,
    })
}

fn build_task_dag(sections: &[AnnotatedSection]) -> Result<Dag, DagError> {
    // Flatten every task across every section into a single node list.
    struct Flat<'a> {
        id: String,
        label: String,
        line: usize,
        needs: &'a [String],
        section_id: String,
    }

    let mut flats: Vec<Flat> = Vec::new();
    for sec in sections {
        for it in &sec.items {
            // A task without an id is unaddressable in DAG terms — synthesize
            // one from `<section_id>.<n>` so it still flows through.
            let id = if it.task.id.is_empty() {
                format!("{}.{}", sec.section.id, flats.len() + 1)
            } else {
                it.task.id.clone()
            };
            flats.push(Flat {
                id,
                label: it.task.text.clone(),
                line: it.task.line_range.0,
                needs: &it.annotations.needs,
                section_id: sec.section.id.clone(),
            });
        }
    }
    if flats.is_empty() {
        return Err(DagError::Empty);
    }

    let id_to_idx: HashMap<&str, usize> = flats
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id.as_str(), i))
        .collect();

    // Track section-internal sequential edges by stamping each flat's section
    // index — the previous task in the same section is a default predecessor.
    let mut last_in_section: HashMap<&str, usize> = HashMap::new();
    let mut nodes: Vec<DagNode> = Vec::with_capacity(flats.len());
    for (i, f) in flats.iter().enumerate() {
        let mut deps: Vec<usize> = Vec::new();
        if !f.needs.is_empty() {
            for dep in f.needs {
                let &idx = id_to_idx.get(dep.as_str()).ok_or_else(|| {
                    DagError::UnknownRef {
                        from: f.id.clone(),
                        to: dep.clone(),
                        kind: "needs",
                    }
                })?;
                deps.push(idx);
            }
        } else if let Some(&prev) = last_in_section.get(f.section_id.as_str()) {
            deps.push(prev);
        }
        last_in_section.insert(f.section_id.as_str(), i);

        nodes.push(DagNode {
            id: f.id.clone(),
            label: f.label.clone(),
            deps,
            parallel_with: Vec::new(),
            prompt_template: None,
            source_line: f.line,
        });
    }

    detect_cycle(&nodes)?;
    Ok(Dag {
        granularity: Granularity::Task,
        nodes,
    })
}

fn build_phase_dag(sections: &[AnnotatedSection]) -> Result<Dag, DagError> {
    if sections.is_empty() {
        return Err(DagError::Empty);
    }
    // One node representing the whole apply phase.
    Ok(Dag {
        granularity: Granularity::Phase,
        nodes: vec![DagNode {
            id: "apply".to_string(),
            label: "Apply (whole change)".to_string(),
            deps: Vec::new(),
            parallel_with: Vec::new(),
            prompt_template: None,
            source_line: sections[0].section.line,
        }],
    })
}

/// Three-color DFS. Returns `Err(Cycle)` with the back-edge path on the first
/// cycle found.
fn detect_cycle(nodes: &[DagNode]) -> Result<(), DagError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color = vec![Color::White; nodes.len()];
    let mut stack: Vec<usize> = Vec::new();

    fn visit(
        node: usize,
        nodes: &[DagNode],
        color: &mut [Color],
        stack: &mut Vec<usize>,
    ) -> Result<(), DagError> {
        color[node] = Color::Gray;
        stack.push(node);
        for &dep in &nodes[node].deps {
            match color[dep] {
                Color::Gray => {
                    // Back edge — extract the cycle.
                    let start = stack.iter().position(|&n| n == dep).unwrap_or(0);
                    let involved = stack[start..]
                        .iter()
                        .map(|&i| nodes[i].id.clone())
                        .chain(std::iter::once(nodes[dep].id.clone()))
                        .collect();
                    return Err(DagError::Cycle { involved });
                }
                Color::White => visit(dep, nodes, color, stack)?,
                Color::Black => {}
            }
        }
        stack.pop();
        color[node] = Color::Black;
        Ok(())
    }

    for i in 0..nodes.len() {
        if color[i] == Color::White {
            visit(i, nodes, &mut color, &mut stack)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openspec::annotations::annotate;
    use crate::openspec::tasks_parser;

    fn build_from_md(input: &str) -> Result<Dag, DagError> {
        let graph = tasks_parser::parse(input);
        let annotated = annotate(graph);
        build(&annotated)
    }

    // ── section granularity ──

    #[test]
    fn empty_input_is_error() {
        let err = build_from_md("").unwrap_err();
        assert_eq!(err, DagError::Empty);
    }

    #[test]
    fn default_sequential_chains_sections() {
        let dag = build_from_md(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C
- [ ] 3.1 c
",
        )
        .unwrap();

        assert_eq!(dag.granularity, Granularity::Section);
        assert_eq!(dag.nodes.len(), 3);
        assert_eq!(dag.nodes[0].deps, Vec::<usize>::new());
        assert_eq!(dag.nodes[1].deps, vec![0]);
        assert_eq!(dag.nodes[2].deps, vec![1]);
    }

    #[test]
    fn explicit_depends_replaces_sequential() {
        let dag = build_from_md(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C <!-- clhorde: depends 1 -->
- [ ] 3.1 c
",
        )
        .unwrap();
        // Default sequential would give {1}. Explicit depends should give {0}.
        assert_eq!(dag.nodes[2].deps, vec![0]);
    }

    #[test]
    fn explicit_depends_multi_value() {
        let dag = build_from_md(
            "\
## 1. A
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C <!-- clhorde: depends 1,2 -->
- [ ] 3.1 c
",
        )
        .unwrap();
        assert_eq!(dag.nodes[2].deps, vec![0, 1]);
    }

    #[test]
    fn parallel_with_is_recorded() {
        let dag = build_from_md(
            "\
## 1. A
- [ ] 1.1 a
## 2. B <!-- clhorde: parallel-with 1 -->
- [ ] 2.1 b
",
        )
        .unwrap();
        assert_eq!(dag.nodes[1].parallel_with, vec![0]);
    }

    #[test]
    fn unknown_depends_ref_is_error() {
        let err = build_from_md(
            "\
## 1. A
- [ ] 1.1 a
## 2. B <!-- clhorde: depends 99 -->
- [ ] 2.1 b
",
        )
        .unwrap_err();
        match err {
            DagError::UnknownRef { from, to, kind } => {
                assert_eq!(from, "2");
                assert_eq!(to, "99");
                assert_eq!(kind, "depends");
            }
            other => panic!("expected UnknownRef, got {other:?}"),
        }
    }

    #[test]
    fn unknown_parallel_with_ref_is_error() {
        let err = build_from_md(
            "\
## 1. A
- [ ] 1.1 a
## 2. B <!-- clhorde: parallel-with 99 -->
- [ ] 2.1 b
",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DagError::UnknownRef { kind: "parallel-with", .. }
        ));
    }

    #[test]
    fn cycle_is_rejected() {
        // Force a cycle: section 1 depends on 2, section 2 default-depends on 1.
        // Wait — section 2 default-depends on 1, so adding `depends 2` on 1
        // creates 1↔2.
        let err = build_from_md(
            "\
## 1. A <!-- clhorde: depends 2 -->
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
",
        )
        .unwrap_err();
        match err {
            DagError::Cycle { involved } => {
                assert!(involved.contains(&"1".to_string()));
                assert!(involved.contains(&"2".to_string()));
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn three_section_cycle_is_rejected() {
        let err = build_from_md(
            "\
## 1. A <!-- clhorde: depends 3 -->
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
## 3. C
- [ ] 3.1 c
",
        )
        .unwrap_err();
        assert!(matches!(err, DagError::Cycle { .. }));
    }

    #[test]
    fn prompt_template_propagates() {
        let dag = build_from_md(
            "\
## 1. A <!-- clhorde: prompt-template strict -->
- [ ] 1.1 a
",
        )
        .unwrap();
        assert_eq!(dag.nodes[0].prompt_template.as_deref(), Some("strict"));
    }

    #[test]
    fn source_line_records_heading_line() {
        // Concrete line numbers, not relative — easier to reason about.
        let input = "\n## 1. A\n- [ ] 1.1 a\n\n## 2. B\n- [ ] 2.1 b\n";
        let dag = build_from_md(input).unwrap();
        assert_eq!(dag.nodes[0].source_line, 2);
        assert_eq!(dag.nodes[1].source_line, 5);
    }

    // ── task granularity ──

    #[test]
    fn task_granularity_emits_one_node_per_task() {
        let dag = build_from_md(
            "\
## 1. A <!-- clhorde: granularity task -->
- [ ] 1.1 a
- [ ] 1.2 b
## 2. B
- [ ] 2.1 c
",
        )
        .unwrap();
        assert_eq!(dag.granularity, Granularity::Task);
        assert_eq!(dag.nodes.len(), 3);
        assert_eq!(dag.nodes[0].id, "1.1");
        assert_eq!(dag.nodes[1].id, "1.2");
        assert_eq!(dag.nodes[2].id, "2.1");
    }

    #[test]
    fn task_default_sequential_within_section_resets_across_sections() {
        let dag = build_from_md(
            "\
## 1. A <!-- clhorde: granularity task -->
- [ ] 1.1 a
- [ ] 1.2 b
## 2. B
- [ ] 2.1 c
",
        )
        .unwrap();
        // 1.1 has no deps; 1.2 chains to 1.1; 2.1 chains to no one (new section).
        assert_eq!(dag.nodes[0].deps, Vec::<usize>::new());
        assert_eq!(dag.nodes[1].deps, vec![0]);
        assert_eq!(dag.nodes[2].deps, Vec::<usize>::new());
    }

    #[test]
    fn task_needs_overrides_default() {
        let dag = build_from_md(
            "\
## 1. A <!-- clhorde: granularity task -->
- [ ] 1.1 a
- [ ] 1.2 b
- [ ] 1.3 c <!-- clhorde: needs 1.1 -->
",
        )
        .unwrap();
        // 1.3 should depend on 1.1 only, not the default 1.2.
        assert_eq!(dag.nodes[2].deps, vec![0]);
    }

    #[test]
    fn task_needs_unknown_ref_is_error() {
        let err = build_from_md(
            "\
## 1. A <!-- clhorde: granularity task -->
- [ ] 1.1 a <!-- clhorde: needs 9.9 -->
",
        )
        .unwrap_err();
        assert!(matches!(err, DagError::UnknownRef { kind: "needs", .. }));
    }

    // ── phase granularity ──

    #[test]
    fn phase_granularity_collapses_to_single_node() {
        let dag = build_from_md(
            "\
## 1. A <!-- clhorde: granularity phase -->
- [ ] 1.1 a
## 2. B
- [ ] 2.1 b
",
        )
        .unwrap();
        assert_eq!(dag.granularity, Granularity::Phase);
        assert_eq!(dag.nodes.len(), 1);
        assert_eq!(dag.nodes[0].id, "apply");
    }
}
