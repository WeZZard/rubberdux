---
name: linear-cli
description: Manage Linear issues and projects from the command line. This skill allows automating Linear project management.
---

# Linear CLI

A cross-platform CLI for Linear's GraphQL API, with unblocked issue filtering.

Install: `npm install -g @dabble/linear-cli`

## First-Time Setup

```bash
linear login
```

This will:
1. Ask where to save credentials (project or global)
2. Open Linear API settings in your browser
3. Prompt you to paste your API key
4. Show available teams and let you pick one (or create a new team)
5. Save config to the chosen location

## Configuration

Config is layered: `~/.linear` (global) is loaded first, then `./.linear` (local) overrides on top. Local values override global; unset local values inherit from global. Env vars (`LINEAR_API_KEY`, `LINEAR_TEAM`, `LINEAR_PROJECT`, `LINEAR_MILESTONE`) are used as fallbacks.

```
# .linear file format
api_key=lin_api_xxx
team=ISSUE
project=My Project
milestone=Sprint 3

[aliases]
V2=Version 2.0 Release
MVP=MVP Milestone
```

## Aliases

Create short codes for projects and milestones to use in commands:

```bash
# Create aliases
linear alias V2 "Version 2.0"           # For a project
linear alias MVP "MVP Milestone"        # For a milestone

# List all aliases
linear alias --list

# Remove an alias
linear alias --remove MVP
```

Use aliases anywhere a project or milestone name is accepted:

```bash
linear issues --project V2
linear issues --milestone MVP
linear issue create --project V2 --milestone MVP "New feature"
linear milestones --project V2
```

Aliases are shown in `linear projects` and `linear milestones` output:

```
Projects:
[V2] Version 2.0 Release  started  58%

Milestones:
[MVP] MVP Milestone [next]
```

## Quick Reference

```bash
# Auth
linear login                    # Interactive setup
linear logout                   # Remove config
linear whoami                   # Show current user/team

# Roadmap (overview)
linear roadmap                  # Projects with milestones and progress

# Default context (sets project/milestone for issues & create)
linear project open "Phase 1"    # Set default project
linear milestone open "Sprint 3" # Set default milestone
linear project close             # Clear default project
linear milestone close           # Clear default milestone

# Issues
linear issues                    # Default: backlog + todo issues (filtered by open project/milestone)
linear issues --no-project       # Bypass default project filter
linear issues --no-milestone     # Bypass default milestone filter
linear issues --unblocked       # Ready to work on (no blockers)
linear issues --open            # All non-completed issues
linear issues --status todo     # Only todo issues
linear issues --status backlog  # Only backlog issues
linear issues --status in-progress # Issues currently in progress
linear issues --status todo --status in-progress # Multiple statuses
linear issues --mine            # Only your assigned issues
linear issues --project "Name"  # Issues in a project
linear issues --milestone "M1"  # Issues in a milestone
linear issues --label bug       # Filter by label
linear issues --priority urgent # Filter by priority (urgent/high/medium/low/none)
# Flags can be combined: linear issues --status todo --mine
linear issue show ISSUE-1        # Full details with parent context
linear issue start ISSUE-1       # Assign to you + set In Progress
linear issue create --title "Fix bug" --project "Phase 1" --assign --estimate M
linear issue create --title "Urgent bug" --priority urgent --assign
linear issue create --title "Task" --milestone "Beta" --estimate S
linear issue create --title "Blocked task" --blocked-by ISSUE-1 --blocked-by ISSUE-2
linear issue create --title "Labeled" --label bug --label frontend  # Multiple labels
linear issue update ISSUE-1 --status "In Progress"
linear issue update ISSUE-1 --priority high   # Set priority
linear issue update ISSUE-1 --estimate M      # Set estimate
linear issue update ISSUE-1 --label bug --label frontend  # Set labels (repeatable)
linear issue update ISSUE-1 --assign          # Assign to yourself
linear issue update ISSUE-1 --parent ISSUE-2  # Set parent issue
linear issue update ISSUE-1 --milestone "Beta"
linear issue update ISSUE-1 --append "Notes..."
linear issue update ISSUE-1 --check "validation" # Check off a todo item
linear issue update ISSUE-1 --blocks ISSUE-2 --blocks ISSUE-3  # Repeatable
linear issue close ISSUE-1
linear issue comment ISSUE-1 "Comment text"

# Projects
linear projects                 # Active projects
linear projects --all           # Include completed
linear project show "Phase 1"   # Details with issues
linear project create "Name" --description "..."
linear project complete "Phase 1"
linear project open "Phase 1"   # Set as default project
linear project close            # Clear default project

# Milestones
linear milestones --project "P1" # Milestones in a project
linear milestone show "Beta"     # Details with issues
linear milestone create "Beta" --project "P1" --target-date 2024-03-01
linear milestone open "Beta"    # Set as default milestone
linear milestone close          # Clear default milestone

# Reordering (drag-drop equivalent)
linear projects reorder "P1" "P2" "P3"           # Set project order
linear project move "Urgent" --before "Phase 1"  # Move single project
linear milestones reorder "Alpha" "Beta" --project "P1"
linear milestone move "Beta" --after "Alpha" --project "P1"
linear issues reorder ISSUE-1 ISSUE-2 ISSUE-3    # Set issue order
linear issue move ISSUE-5 --before ISSUE-1       # Move single issue

# Labels
linear labels                   # List all labels
linear label create "bug" --color "#FF0000"

# Aliases
linear alias V2 "Version 2.0"        # Create alias for project/milestone
linear alias --list                  # List all aliases
linear alias --remove V2             # Remove alias
# Then use: linear issues --project V2

# Git
linear branch ISSUE-1            # Create branch: ISSUE-1-issue-title
```

## Estimation

Use t-shirt sizes for estimates. Always use `--estimate` (not `-e`) for clarity.

| Size | Meaning |
|------|---------|
| XS | Trivial, < 1 hour |
| S | Small, couple hours |
| M | Medium, a day or so |
| L | Large, multi-day - consider breaking down |
| XL | Very large - should definitely break down |

```bash
# Create with estimate (use long flags for clarity)
linear issue create --title "Add caching" --estimate M --assign

# L/XL issues should be broken into sub-issues
linear issue create --title "Implement auth" --estimate L
linear issue create --title "Add login endpoint" --parent ISSUE-5 --estimate S
linear issue create --title "Add JWT validation" --parent ISSUE-5 --estimate S
```

## Git Conventions

Always link git work to Linear issues:

```bash
# Create branch from issue (recommended)
linear branch ISSUE-5            # Creates: ISSUE-5-add-caching-layer

# Commit message format
git commit -m "ISSUE-5: Add cache invalidation on logout"

# Include issue ID in PR title
gh pr create --title "ISSUE-5: Add caching layer"
```

## Workflow Guidelines

### Setting context
When working on a specific project/milestone, set it as default to avoid repeating flags:
```bash
linear project open "Phase 1"    # All commands now default to Phase 1
linear milestone open "Sprint 3" # And to Sprint 3 milestone
linear issues                    # Shows Phase 1 > Sprint 3 issues only
linear issue create --title "Fix" # Created in Phase 1, Sprint 3
linear project close             # Done? Clear the context
```

### Getting oriented
```bash
linear roadmap                  # See all projects, milestones, progress
linear issues --project "P1"    # Issues in a specific project
linear issues --milestone "M1"  # Issues in a specific milestone
```

### Starting work on an issue
```bash
linear issues --unblocked       # Find what's ready
linear issue show ISSUE-2        # Review it (shows parent context)
linear issue start ISSUE-2       # Assign + set In Progress
linear branch ISSUE-2            # Create git branch
```

### When you hit a blocker
If work cannot continue due to a dependency or external factor:

```bash
# Create the blocking issue
linear issue create --title "Need API credentials" --blocks ISSUE-5

# Or mark existing issue as blocking
linear issue update ISSUE-3 --blocks ISSUE-5
```

This removes ISSUE-5 from `--unblocked` results until the blocker is resolved.

### When a task is larger than expected
If you discover an M issue is actually L/XL, break it down:

```bash
# Create sub-issues
linear issue create --title "Step 1: Research approach" --parent ISSUE-5 --estimate S
linear issue create --title "Step 2: Implement core logic" --parent ISSUE-5 --estimate M
linear issue create --title "Step 3: Add tests" --parent ISSUE-5 --estimate S

# Start working on the first sub-issue
linear issue start ISSUE-6
```

### Checklists vs. sub-issues
Use description checklists for lightweight steps within a single issue. Use sub-issues when items need their own status, assignee, or estimate.

```bash
# Checklist — quick implementation steps, a punch list, acceptance criteria
linear issue update ISSUE-5 --append "## TODO\n- [ ] Add validation\n- [ ] Update tests\n- [ ] Check edge cases"

# Check off completed items (fuzzy matches the item text)
linear issue update ISSUE-5 --check "validation"
linear issue update ISSUE-5 --check "tests"

# Uncheck if needed
linear issue update ISSUE-5 --uncheck "validation"

# Sub-issues — substantial, independently trackable work
linear issue create --title "Add login endpoint" --parent ISSUE-5 --estimate S
```

Prefer checklists when the items are small and don't need independent tracking. Prefer sub-issues when you'd want to assign, estimate, or block on them individually. Use `--check` to mark items complete as you finish them.

### Completing work
After finishing implementation, ask the developer if they want to close the issue:

```bash
# Suggest closing
linear issue close ISSUE-5
```

Do not auto-close issues. Let the developer review the work first.

### Adding notes while working
```bash
linear issue update ISSUE-2 --append "## Notes\n\nDiscovered X, trying Y approach..."
# or for quick updates
linear issue comment ISSUE-2 "Found the root cause in auth.ts:142"
```

### Organizing with milestones
Milestones group related issues within a project:

```bash
# Create milestone for a release
linear milestone create "Beta" --project "Phase 1" --target-date 2024-03-01

# Add issues to milestone
linear issue create --title "Core feature" --milestone "Beta" --estimate M
linear issue update ISSUE-5 --milestone "Beta"

# Reorder milestones to reflect priority
linear milestones reorder "Alpha" "Beta" "Stable" --project "Phase 1"
```

### Completing a phase
```bash
linear issue close ISSUE-7       # Close remaining issues
linear project complete "Phase 1"
# Then update CLAUDE.md status table
```

## Parent Context

When viewing an issue with `linear issue show`, you'll see where it fits in the larger work:

```
# ISSUE-6: Add JWT validation

State: In Progress
...

## Context

ISSUE-3: Implement authentication system
  - [Done] ISSUE-4: Add login endpoint
  → [In Progress] ISSUE-6: Add JWT validation  ← you are here
  - [Backlog] ISSUE-7: Add refresh tokens
```

This helps understand the scope and what comes before/after the current task.
