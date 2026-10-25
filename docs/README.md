# Git worktree disk usage: guides and comparisons

Git worktrees share a repository's history, but each checkout normally has its own working files. If you run several branches or coding agents at once, those files, dependencies and build outputs can add up quickly.

These guides explain where that space goes, how copy-on-write helps and when a different worktree tool might suit you better.

| Guide | Start here if... |
| --- | --- |
| [How to reduce Git worktree disk usage](reduce-git-worktree-disk-usage.md) | Your worktrees are filling your disk and you want practical fixes. |
| [Parhel vs. native Git, Worktrunk, git-cow and simgit](git-worktree-tools-comparison.md) | You're choosing a tool for worktrees or parallel coding agents. |
| [Copy-on-write Git worktrees on ext4](copy-on-write-git-worktrees-ext4.md) | You're on Ubuntu or another Linux system using ext4. |
| [Git worktree disk space FAQ](git-worktree-disk-space-faq.md) | You have a specific question about dependencies, isolation, measurement or cleanup. |

For installation, all command options, support status and release-specific caveats, see the [Parhel README](../README.md). For the checks behind its recovery and safety behavior, see the [test catalog](test-catalog.md).
