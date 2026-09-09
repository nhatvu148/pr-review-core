<!--
Claude Code reads CLAUDE.md, not AGENTS.md, so this file exists only to point at
the one that holds the content. Everything worth saying goes in AGENTS.md, where
Codex, Cursor and the rest read it too — including the pre-push hook that reviews
this repo with `--backend codex`.

An import rather than `ln -s AGENTS.md CLAUDE.md`: a symlink needs Administrator
or Developer Mode to create on Windows, and this project builds and ships a
Windows binary, so contributors are on it.

Keep this file a pointer. A second copy of the build commands is a second place
for them to go stale.
-->

@AGENTS.md
