You are working on OpenSpec change `{{ change_name }}`.

Read the proposal, design, and specs in:
  {{ change_dir }}/proposal.md
  {{ change_dir }}/design.md
  {{ change_dir }}/specs/

Your job: complete section {{ section_id }} ({{ section_title }}) of {{ change_dir }}/tasks.md.

Tasks to implement:
{{ tasks_block }}

When each task is done, edit {{ change_dir }}/tasks.md and change `- [ ]` to `- [x]`
for that exact line. Do not start any other section.

Run any relevant tests after the section. If tests fail, fix and re-run.
Stop when all tasks of this section are checked.
