//! Parse `<!-- clhorde: ... -->` directives next to section headers and
//! task lines.
//!
//! Format:
//!
//! ```markdown
//! ## 3. Tests <!-- clhorde: depends 2; parallel-with 4 -->
//! - [ ] 3.2 E2E <!-- clhorde: needs 2.1 -->
//! ```
//!
//! Multiple directives in a single comment are separated by `;`. Each
//! directive is `<key> <value(s)>`, where multi-value lists use `,` (commas
//! without surrounding whitespace are tolerated).
//!
//! Recognized keys:
//! - `depends` — section ids this section runs *after* (overrides the default
//!   sequential policy).
//! - `parallel-with` — section ids that may run concurrently with this one.
//! - `needs` — task ids (dotted) the annotated task depends on.
//! - `granularity` — `section` (default), `task`, or `phase`.
//! - `prompt-template` — name of a template overriding the default for this
//!   section.
//!
//! Unknown keys are silently ignored. Empty values are treated as missing.
//! Parsing is total: callers always get a `SectionAnnotations` /
//! `TaskAnnotations` (defaulted when no comment is present).

use super::tasks_parser::{Section, TaskGraph, TaskItem};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Granularity {
    #[default]
    Section,
    Task,
    Phase,
}

impl Granularity {
    fn parse(s: &str) -> Option<Granularity> {
        match s.trim() {
            "section" => Some(Granularity::Section),
            "task" => Some(Granularity::Task),
            "phase" => Some(Granularity::Phase),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SectionAnnotations {
    pub depends: Vec<String>,
    pub parallel_with: Vec<String>,
    pub granularity: Option<Granularity>,
    pub prompt_template: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskAnnotations {
    pub needs: Vec<String>,
}

/// Annotated copy of a [`Section`] — original parser output plus directives
/// extracted from its source line.
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotatedSection {
    pub section: Section,
    pub annotations: SectionAnnotations,
    pub items: Vec<AnnotatedTask>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotatedTask {
    pub task: TaskItem,
    pub annotations: TaskAnnotations,
}

/// Walk a [`TaskGraph`] and attach annotations parsed from each
/// `source_line`. Always succeeds; malformed comments are ignored quietly so
/// a typo doesn't block the whole workflow.
pub fn annotate(graph: TaskGraph) -> Vec<AnnotatedSection> {
    graph
        .sections
        .into_iter()
        .map(|mut section| {
            let annotations = parse_section(&section.source_line);
            let raw_items = std::mem::take(&mut section.items);
            let items = raw_items
                .into_iter()
                .map(|task| {
                    let annotations = parse_task(&task.source_line);
                    AnnotatedTask { task, annotations }
                })
                .collect();
            AnnotatedSection {
                section,
                annotations,
                items,
            }
        })
        .collect()
}

fn parse_section(line: &str) -> SectionAnnotations {
    let mut out = SectionAnnotations::default();
    for body in extract_clhorde_bodies(line) {
        for (key, value) in split_directives(body) {
            match key {
                "depends" => out.depends.extend(split_values(value)),
                "parallel-with" => out.parallel_with.extend(split_values(value)),
                "granularity" => {
                    if let Some(g) = Granularity::parse(value) {
                        out.granularity = Some(g);
                    }
                }
                "prompt-template" => {
                    let v = value.trim();
                    if !v.is_empty() {
                        out.prompt_template = Some(v.to_string());
                    }
                }
                _ => {} // unknown key — ignore
            }
        }
    }
    out
}

fn parse_task(line: &str) -> TaskAnnotations {
    let mut out = TaskAnnotations::default();
    for body in extract_clhorde_bodies(line) {
        for (key, value) in split_directives(body) {
            if key == "needs" {
                out.needs.extend(split_values(value));
            }
        }
    }
    out
}

/// Yield the inner text of every `<!-- clhorde: ... -->` comment on `line`.
fn extract_clhorde_bodies(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find("<!--") {
        let after_open = &rest[open + 4..];
        let close = match after_open.find("-->") {
            Some(c) => c,
            None => break,
        };
        let inner = after_open[..close].trim();
        if let Some(body) = inner.strip_prefix("clhorde:") {
            out.push(body.trim());
        }
        rest = &after_open[close + 3..];
    }
    out
}

/// Split a comment body like `"depends 2; parallel-with 4"` into
/// `(key, value)` pairs.
fn split_directives(body: &str) -> Vec<(&str, &str)> {
    body.split(';')
        .filter_map(|chunk| {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                return None;
            }
            let split_at = chunk.find(char::is_whitespace).unwrap_or(chunk.len());
            let (key, value) = chunk.split_at(split_at);
            Some((key, value.trim()))
        })
        .collect()
}

/// Split a multi-value field like `"2, 3"` or `"2 3"` into its tokens.
fn split_values(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|tok| {
            let t = tok.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openspec::tasks_parser;

    fn ann_section(line: &str) -> SectionAnnotations {
        parse_section(line)
    }

    fn ann_task(line: &str) -> TaskAnnotations {
        parse_task(line)
    }

    // ── extract_clhorde_bodies ──

    #[test]
    fn extracts_single_clhorde_comment() {
        let bodies = extract_clhorde_bodies("## 3. Tests <!-- clhorde: depends 2 -->");
        assert_eq!(bodies, vec!["depends 2"]);
    }

    #[test]
    fn ignores_non_clhorde_comments() {
        let bodies = extract_clhorde_bodies("## 3. Tests <!-- TODO: refactor -->");
        assert!(bodies.is_empty());
    }

    #[test]
    fn handles_multiple_clhorde_comments_on_one_line() {
        let bodies = extract_clhorde_bodies(
            "- [ ] 1.1 X <!-- clhorde: needs 0.1 --> <!-- clhorde: needs 0.2 -->",
        );
        assert_eq!(bodies, vec!["needs 0.1", "needs 0.2"]);
    }

    #[test]
    fn unterminated_comment_is_ignored() {
        let bodies = extract_clhorde_bodies("## 3. Tests <!-- clhorde: depends 2");
        assert!(bodies.is_empty());
    }

    // ── section annotations ──

    #[test]
    fn section_depends_single_value() {
        let a = ann_section("## 3. Tests <!-- clhorde: depends 2 -->");
        assert_eq!(a.depends, vec!["2"]);
    }

    #[test]
    fn section_depends_comma_separated() {
        let a = ann_section("## 3. Tests <!-- clhorde: depends 1,2,3 -->");
        assert_eq!(a.depends, vec!["1", "2", "3"]);
    }

    #[test]
    fn section_depends_space_separated() {
        let a = ann_section("## 3. Tests <!-- clhorde: depends 1 2 3 -->");
        assert_eq!(a.depends, vec!["1", "2", "3"]);
    }

    #[test]
    fn section_multiple_directives() {
        let a = ann_section(
            "## 3. Tests <!-- clhorde: depends 2; parallel-with 4 -->",
        );
        assert_eq!(a.depends, vec!["2"]);
        assert_eq!(a.parallel_with, vec!["4"]);
    }

    #[test]
    fn section_granularity_recognized() {
        let a = ann_section("## 3. T <!-- clhorde: granularity task -->");
        assert_eq!(a.granularity, Some(Granularity::Task));

        let a = ann_section("## 3. T <!-- clhorde: granularity phase -->");
        assert_eq!(a.granularity, Some(Granularity::Phase));

        let a = ann_section("## 3. T <!-- clhorde: granularity nope -->");
        assert_eq!(a.granularity, None);
    }

    #[test]
    fn section_prompt_template_override() {
        let a = ann_section(
            "## 3. T <!-- clhorde: prompt-template apply-section-strict -->",
        );
        assert_eq!(a.prompt_template.as_deref(), Some("apply-section-strict"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let a = ann_section(
            "## 3. T <!-- clhorde: depends 2; bogus value; parallel-with 1 -->",
        );
        assert_eq!(a.depends, vec!["2"]);
        assert_eq!(a.parallel_with, vec!["1"]);
        // No panic.
    }

    #[test]
    fn empty_clhorde_comment_yields_default() {
        let a = ann_section("## 3. T <!-- clhorde: -->");
        assert_eq!(a, SectionAnnotations::default());
    }

    #[test]
    fn no_comment_yields_default() {
        let a = ann_section("## 3. Tests");
        assert_eq!(a, SectionAnnotations::default());
    }

    // ── task annotations ──

    #[test]
    fn task_needs_single_value() {
        let a = ann_task("- [ ] 3.2 E2E <!-- clhorde: needs 2.1 -->");
        assert_eq!(a.needs, vec!["2.1"]);
    }

    #[test]
    fn task_needs_multiple() {
        let a = ann_task("- [ ] 3.2 E2E <!-- clhorde: needs 2.1, 2.2 -->");
        assert_eq!(a.needs, vec!["2.1", "2.2"]);
    }

    #[test]
    fn task_ignores_section_keys() {
        let a = ann_task("- [ ] 3.2 E2E <!-- clhorde: depends 2; needs 2.1 -->");
        assert_eq!(a.needs, vec!["2.1"]);
        // `depends` on a task line is ignored — no field for it.
    }

    // ── annotate() integration ──

    #[test]
    fn annotate_walks_full_graph() {
        let input = "\
## 1. A
- [ ] 1.1 first
## 3. C <!-- clhorde: depends 1; parallel-with 2 -->
- [ ] 3.1 second <!-- clhorde: needs 1.1 -->
";
        let graph = tasks_parser::parse(input);
        let annotated = annotate(graph);
        assert_eq!(annotated.len(), 2);

        // Section 1: bare
        assert_eq!(annotated[0].annotations, SectionAnnotations::default());
        assert_eq!(annotated[0].items.len(), 1);
        assert_eq!(annotated[0].items[0].annotations, TaskAnnotations::default());

        // Section 3: annotated
        assert_eq!(annotated[1].annotations.depends, vec!["1"]);
        assert_eq!(annotated[1].annotations.parallel_with, vec!["2"]);
        assert_eq!(annotated[1].items[0].annotations.needs, vec!["1.1"]);
    }

    #[test]
    fn annotate_preserves_section_metadata() {
        let input = "\
## 2. Foo <!-- clhorde: depends 1 -->
- [ ] 2.1 bar
";
        let graph = tasks_parser::parse(input);
        let annotated = annotate(graph);
        assert_eq!(annotated[0].section.id, "2");
        assert_eq!(annotated[0].section.title, "Foo");
        assert_eq!(annotated[0].section.line, 1);
    }
}
