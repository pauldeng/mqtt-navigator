# ADR 0003: Own flat-row topic tree

Status: accepted (2026-11-11). Advice: Codex (gpt-6.1-sol).

## Problem

The gpui-kit `Tree` rebuilds all entries on each expand and on `set_items`. Each rebuild deep-clones every visible item with its children. `set_items` also resets the selection, and there is no incremental insert. A live MQTT tree changes many times each second.

## Decision

- `navigator-core` keeps the tree as an arena of nodes. Expansion and selection use `NodeId`.
- The store keeps a flat list of visible rows. It rebuilds the list only when the structure, the expansion or the filter changes. Counts are read at render time.
- The app renders the rows with GPUI `uniform_list` and its own row layout and keyboard handling.

## Consequences

- We write the tree keyboard handling ourselves (arrows, Left, Right, Enter).
- P1 measures this list. We do not benchmark the gpui-kit `Tree`.
