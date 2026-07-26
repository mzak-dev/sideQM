# Icons chosen in the Popover are copied into a library, on commit

An Item's `icon` used to be whatever path the user picked. That path is usually
somewhere transient — a Downloads folder, an extracted archive, a file the user
renames a week later — and when it moved, the Item silently fell back to the
launch target's own icon. Nothing said why, and the config still named a file
that no longer existed.

Picking an icon now copies the file into `icons\` next to config.json and stores
the copy's path. The name is the content's FNV-1a hash plus the original
extension, so the same image added twice is stored once. This is a library, not
a cache: it cannot be regenerated, and deleting it breaks Items.

The copy happens at **commit**, not at pick time. Picking an icon and then
cancelling the Popover — or picking three icons before settling on one — leaves
nothing behind. The cost is that the file is read twice (once to preview it,
once to copy it); at icon sizes that is not worth a reference count.

Rejected: storing the original path (the failure mode above, which is what
prompted this); copying at pick time (orphans on cancel); a content-addressed
store shared with the decoded-pixel cache (the two have opposite lifetimes —
decoded pixels are disposable, source files are not, and naming the folder
`cached_icons` would invite exactly the deletion this prevents).

Config paths written by the app are serialized by serde, so they escape their
backslashes correctly. Hand-written `"icon"` paths are still the user's problem,
and remain the most common way to get a working config with a broken icon —
which is why every icon failure is now logged with its path and reason.
