# Maintained Wasmtime patch history

This orphan branch records the downstream `fuel-epoch-polling` patch train
based on `bytecodealliance/wasmtime` main. `UPSTREAM`, `SOURCE`, `SERIES`,
`RESULT_TREE`, and `patches/` are generated snapshots. Each snapshot is checked
by applying the series to a temporary index and comparing its result tree.

Use the canonical patch-history updater to regenerate the snapshot from the
clean source worktree. Preserve patch boundaries and record a semantic summary
when upstream changes require adaptation. Run `scripts/apply.sh` against a
clean checkout at `UPSTREAM` to reconstruct the recorded source tree.
