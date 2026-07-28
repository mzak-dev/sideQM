# Item mutations write the in-memory config, not a fresh read of the file

`append_item` deliberately re-read config.json before appending, so that a hand
edit made while the app was running survived the next add. That works because
appending needs no index: whatever the file holds, the new Item goes on the end.

Removing, reordering, and editing all need an index, and an index only means
anything against the list the user is actually looking at. Re-reading the file
and then removing element 2 removes whatever happens to be second on disk, which
is not necessarily what the user clicked. So every Item mutation now serializes
the in-memory config with the change applied: what the Menu shows is what the
file gets.

The race this gives up is narrower than it looks. Losing window focus leaves
Pinned (main.rs), and opening the Menu reloads the config first, so the user
cannot reach Notepad without ending the session that would have raced it. What
remains is an external process writing config.json during a Pinned session —
which the single-instance mutex already rules out for a second sideQM.

All four mutations (add, remove, reorder, edit) go through one write. That is
worth doing on its own with four callers touching one file, and it is also the
single place a future undo would hook into: keep the previous `Vec<Item>` before
the write. No undo is built now, deliberately — a removed Item costs a name, a
target, and an icon path to retype, and the stored icon file is not deleted
(ADR-0003: names are content hashes, so Items can share one file).

Rejected: matching Items by content instead of index (two identical Items become
indistinguishable and the mutation hits an arbitrary one); re-reading and
bailing out when the file disagrees with memory (protects both sides, but the
gesture silently does nothing and the user has no way to tell why).
