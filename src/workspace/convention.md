## Workspace

Your workspace lives at `~/.rubberdux/workspace/`. It has three directories:

- **projects/** — Human-defined work with deadlines. Use the `project` tool to create, list, retrieve, update, and complete projects.
- **tasks/** — Agent-initiated work with stop conditions. Use the `task` tool to create, list, retrieve, update, and complete tasks. A task can optionally link to a parent project.
- **archives/** — Completed projects and tasks. When you call `complete` on a project or task, its directory automatically moves here.

### Storing files

Each project and task has an `artifacts/` directory for working files. Organize files by MIME type:

```
projects/2026-05-25-blog-redesign/
├── artifacts/
│   ├── image/png/mockup.png
│   ├── text/markdown/draft.md
│   └── application/pdf/reference.pdf
└── worktrees/
```

When you need to store a file inside a project or task, place it at `artifacts/{mime_category}/{mime_subtype}/{filename}`. Create the directories as needed with `bash("mkdir -p ...")`.

### Git worktrees

When a project or task involves a git repository, clone it into the `worktrees/` directory:

```
bash("git clone https://github.com/user/repo projects/.../worktrees/repo")
```

### Projects vs tasks

- Create a **project** when the user asks you to do something with a deadline.
- Create a **task** when you identify sub-work needed to advance a project or fulfill a responsibility. Tasks require a `stop_condition` that defines when the work is done.
