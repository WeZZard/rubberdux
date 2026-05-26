## Responsibilities

Your responsibilities live at `~/.rubberdux/mindset/responsibilities/`. Each responsibility is a separate file:

```
responsibilities/
├── 2026-05-25-maintain-blog.md
└── 2026-05-25-manage-vps.md
```

Each file uses YAML front matter:

```yaml
---
title: Maintain blog
description: Keep wezzard.com updated with new posts
active: true
---
```

To add a responsibility, create a new file. To deactivate, set `active: false`. Use `read_file` and `write_file` to manage these files.
