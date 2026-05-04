//! Parse an OpenSpec `tasks.md` file into a structured [`TaskGraph`].
//!
//! The grammar is intentionally narrow:
//!
//! ```markdown
//! ## 1. Section title              <-- top-level section, id = "1"
//! - [ ] 1.1 First task              <-- task item, id = "1.1"
//! - [x] 1.2 Done task               <-- checked task
//!
//! ### 1.3 Sub-section               <-- sub-section, id = "1.3", parent = "1"
//! - [ ] 1.3.1 Nested task           <-- nested item
//! ```
//!
//! Lines inside fenced code blocks (` ``` ` or `~~~ `) are ignored so that
//! task-like lines in code samples don't accidentally become items. Indented
//! code blocks are treated as prose — we only fence-track on ASCII fences.
//!
//! Annotations (`<!-- clhorde: ... -->`) are *not* parsed here; the
//! [`super::annotations`] module reads `Section::source_line` and
//! `TaskItem::source_line` afterwards. Splitting the two passes keeps each
//! piece testable on its own.

/// Parsed tree of an OpenSpec `tasks.md`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskGraph {
    pub sections: Vec<Section>,
}

/// One `## N.` or `### N.M` heading and its task items.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// Decimal id parsed from the heading text (e.g. `"1"` or `"2.3"`).
    pub id: String,
    /// Human title with the leading id stripped.
    pub title: String,
    /// Heading nesting level — 2 for `##`, 3 for `###`, etc.
    pub level: u8,
    /// Parent section id derived from the dotted form (e.g. `"2.3"` → `Some("2")`).
    pub parent: Option<String>,
    /// Tasks declared directly under this section.
    pub items: Vec<TaskItem>,
    /// 1-based line number of the heading line.
    pub line: usize,
    /// Verbatim heading line, kept for the annotations pass.
    pub source_line: String,
}

/// One `- [ ]` or `- [x]` task line.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskItem {
    /// Decimal id parsed from the leading text (e.g. `"1.2"`). Empty when the
    /// line had no leading id (still legal — the scheduler can synthesize).
    pub id: String,
    /// Task text with the leading id stripped.
    pub text: String,
    /// Checkbox state — `[x]`/`[X]` is `true`, `[ ]` is `false`.
    pub done: bool,
    /// 1-based `(start, end)` line range, currently always a single line.
    pub line_range: (usize, usize),
    /// Verbatim source line, kept for the annotations pass.
    pub source_line: String,
}

/// Parse a tasks.md document. Never fails — malformed lines are simply
/// dropped, so the caller gets a best-effort tree.
pub fn parse(input: &str) -> TaskGraph {
    let mut sections: Vec<Section> = Vec::new();
    let mut in_code_fence = false;

    for (idx, line) in input.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim_start();

        if is_code_fence(trimmed) {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }

        if let Some(section) = parse_heading(line, line_no) {
            sections.push(section);
            continue;
        }

        if let Some(task) = parse_task(line, line_no) {
            if let Some(last) = sections.last_mut() {
                last.items.push(task);
            }
            // Tasks before any heading are dropped on purpose — OpenSpec
            // tasks.md always opens with a section header.
        }
    }

    TaskGraph { sections }
}

fn is_code_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Recognize `## 1. Title` / `### 2.3 Title`. Returns `None` when the line
/// is not a heading or has no decimal id.
fn parse_heading(line: &str, line_no: usize) -> Option<Section> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if !(2..=6).contains(&level) {
        return None;
    }
    let after_hashes = &trimmed[level..];
    if !after_hashes.starts_with(' ') {
        return None;
    }
    let body = after_hashes.trim_start();
    let (id, rest) = split_leading_id(body)?;
    let parent = parent_id(&id);
    Some(Section {
        id,
        title: strip_html_comments(rest).trim().to_string(),
        level: level as u8,
        parent,
        items: Vec::new(),
        line: line_no,
        source_line: line.to_string(),
    })
}

/// Recognize `- [ ] 1.2 Description` / `* [x] 1.2 Description`.
fn parse_task(line: &str, line_no: usize) -> Option<TaskItem> {
    let trimmed = line.trim_start();
    let after_marker = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;

    let (done, after_box) = parse_checkbox(after_marker)?;
    let body = after_box.trim_start();
    let (id, rest_raw) = match split_leading_id(body) {
        Some((i, r)) => (i, r),
        None => (String::new(), body),
    };
    let text = strip_html_comments(rest_raw).trim().to_string();
    Some(TaskItem {
        id,
        text,
        done,
        line_range: (line_no, line_no),
        source_line: line.to_string(),
    })
}

/// Strip a `[ ]`/`[x]`/`[X]` checkbox prefix. Returns `(done, rest)`.
fn parse_checkbox(s: &str) -> Option<(bool, &str)> {
    let bytes = s.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'[' || bytes[2] != b']' {
        return None;
    }
    let done = match bytes[1] {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    Some((done, &s[3..]))
}

/// Split a leading dotted decimal id (e.g. `"1.2.3"`) from the rest of a
/// string. Optional trailing `.` after the id is consumed.
fn split_leading_id(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let mut end = 0;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_digit() || b == b'.' {
            end += 1;
        } else {
            break;
        }
    }
    let mut id = &s[..end];
    // Trim a trailing dot ("1." → "1") and reject standalone "."
    id = id.trim_end_matches('.');
    if id.is_empty() || id.contains("..") {
        return None;
    }
    let after = &s[end..];
    // Require either end of string or a separator character; otherwise the
    // "1" was just a leading numeral inside a word like "1stThing".
    if !after.is_empty() && !after.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    Some((id.to_string(), after))
}

fn parent_id(id: &str) -> Option<String> {
    let dot = id.rfind('.')?;
    Some(id[..dot].to_string())
}

/// Strip every `<!-- ... -->` substring from `s`. Used to keep HTML comments
/// out of the rendered title/text — annotations are still re-extracted from
/// `source_line` later. Unterminated comments are left as-is so we don't
/// accidentally swallow real content.
fn strip_html_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("<!--") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 4..];
        match after_open.find("-->") {
            Some(close) => rest = &after_open[close + 3..],
            None => {
                // Unterminated — keep the leftover verbatim.
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_section_with_two_tasks() {
        let input = "\
## 1. Theme Infrastructure
- [ ] 1.1 Create ThemeContext
- [ ] 1.2 Add CSS variables
";
        let g = parse(input);
        assert_eq!(g.sections.len(), 1);
        let s = &g.sections[0];
        assert_eq!(s.id, "1");
        assert_eq!(s.title, "Theme Infrastructure");
        assert_eq!(s.level, 2);
        assert_eq!(s.parent, None);
        assert_eq!(s.line, 1);

        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items[0].id, "1.1");
        assert_eq!(s.items[0].text, "Create ThemeContext");
        assert!(!s.items[0].done);
        assert_eq!(s.items[0].line_range, (2, 2));
        assert_eq!(s.items[1].id, "1.2");
    }

    #[test]
    fn checked_task_is_marked_done() {
        let input = "\
## 1. X
- [x] 1.1 done
- [X] 1.2 also done
- [ ] 1.3 pending
";
        let g = parse(input);
        let items = &g.sections[0].items;
        assert!(items[0].done);
        assert!(items[1].done);
        assert!(!items[2].done);
    }

    #[test]
    fn subsection_records_parent() {
        let input = "\
## 1. Top
### 1.2 Sub
- [ ] 1.2.1 Nested
";
        let g = parse(input);
        assert_eq!(g.sections.len(), 2);
        assert_eq!(g.sections[1].id, "1.2");
        assert_eq!(g.sections[1].parent.as_deref(), Some("1"));
        assert_eq!(g.sections[1].level, 3);
        assert_eq!(g.sections[1].items.len(), 1);
        assert_eq!(g.sections[1].items[0].id, "1.2.1");
        assert_eq!(g.sections[1].items[0].line_range, (3, 3));
    }

    #[test]
    fn dotted_heading_form_works() {
        // OpenSpec style: "## 1." with the dot baked in.
        let input = "\
## 1. Foo
- [ ] 1.1 Bar
";
        let g = parse(input);
        assert_eq!(g.sections[0].id, "1");
        assert_eq!(g.sections[0].title, "Foo");
    }

    #[test]
    fn prose_between_items_is_tolerated() {
        let input = "\
## 1. Section

Some prose explaining the section.

- [ ] 1.1 First

More prose between items.

- [ ] 1.2 Second
";
        let g = parse(input);
        assert_eq!(g.sections.len(), 1);
        assert_eq!(g.sections[0].items.len(), 2);
        assert_eq!(g.sections[0].items[0].line_range.0, 5);
        assert_eq!(g.sections[0].items[1].line_range.0, 9);
    }

    #[test]
    fn fenced_code_blocks_are_skipped() {
        let input = "\
## 1. X
```markdown
- [ ] 9.9 Fake task in code
## 99. Fake heading
```
- [ ] 1.1 Real task
";
        let g = parse(input);
        assert_eq!(g.sections.len(), 1);
        assert_eq!(g.sections[0].id, "1");
        assert_eq!(g.sections[0].items.len(), 1);
        assert_eq!(g.sections[0].items[0].id, "1.1");
        assert_eq!(g.sections[0].items[0].text, "Real task");
    }

    #[test]
    fn tilde_fenced_code_blocks_are_skipped() {
        let input = "\
## 1. X
~~~
- [ ] 9.9 Fake task
~~~
- [ ] 1.1 Real
";
        let g = parse(input);
        assert_eq!(g.sections[0].items.len(), 1);
        assert_eq!(g.sections[0].items[0].id, "1.1");
    }

    #[test]
    fn empty_input_returns_empty_graph() {
        assert!(parse("").sections.is_empty());
        assert!(parse("\n\n   \n").sections.is_empty());
    }

    #[test]
    fn task_before_any_heading_is_dropped() {
        let input = "\
- [ ] 0.0 orphan
## 1. Real
- [ ] 1.1 First
";
        let g = parse(input);
        assert_eq!(g.sections.len(), 1);
        assert_eq!(g.sections[0].items.len(), 1);
    }

    #[test]
    fn task_without_id_is_kept_with_empty_id() {
        // The plan accepts tasks without an explicit dotted id; the scheduler
        // can synthesize one. Empty id is a legal sentinel.
        let input = "\
## 1. X
- [ ] Free-form task
";
        let g = parse(input);
        let task = &g.sections[0].items[0];
        assert_eq!(task.id, "");
        assert_eq!(task.text, "Free-form task");
    }

    #[test]
    fn heading_without_id_is_dropped() {
        let input = "\
## Just a heading
- [ ] 1.1 stranded
";
        let g = parse(input);
        assert!(g.sections.is_empty());
    }

    #[test]
    fn h1_is_ignored() {
        // OpenSpec headers always use ##/### — a stray `# Title` is
        // documentation, not structure.
        let input = "\
# tasks.md
## 1. Real
- [ ] 1.1 Real
";
        let g = parse(input);
        assert_eq!(g.sections.len(), 1);
        assert_eq!(g.sections[0].id, "1");
    }

    #[test]
    fn source_line_is_preserved_verbatim_for_annotations_pass() {
        let input = "\
## 1. Foo <!-- clhorde: depends 0 -->
- [ ] 1.1 Bar <!-- clhorde: needs 0.1 -->
";
        let g = parse(input);
        assert!(g.sections[0].source_line.contains("<!-- clhorde:"));
        assert!(g.sections[0].items[0].source_line.contains("<!-- clhorde:"));
    }

    #[test]
    fn star_and_plus_list_markers_work() {
        let input = "\
## 1. X
* [ ] 1.1 star
+ [ ] 1.2 plus
- [ ] 1.3 dash
";
        let g = parse(input);
        assert_eq!(g.sections[0].items.len(), 3);
        assert_eq!(g.sections[0].items[0].id, "1.1");
        assert_eq!(g.sections[0].items[1].id, "1.2");
        assert_eq!(g.sections[0].items[2].id, "1.3");
    }

    // ── split_leading_id ──

    #[test]
    fn split_leading_id_simple() {
        assert_eq!(
            split_leading_id("1 rest"),
            Some(("1".to_string(), " rest"))
        );
    }

    #[test]
    fn split_leading_id_dotted() {
        assert_eq!(
            split_leading_id("1.2.3 rest"),
            Some(("1.2.3".to_string(), " rest"))
        );
    }

    #[test]
    fn split_leading_id_trailing_dot_consumed() {
        assert_eq!(
            split_leading_id("1. rest"),
            Some(("1".to_string(), " rest"))
        );
    }

    #[test]
    fn split_leading_id_rejects_word_with_leading_digit() {
        assert!(split_leading_id("1stThing").is_none());
    }

    #[test]
    fn split_leading_id_rejects_non_digit() {
        assert!(split_leading_id("foo").is_none());
        assert!(split_leading_id(".").is_none());
    }
}
