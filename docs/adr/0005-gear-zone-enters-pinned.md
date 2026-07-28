# The Gear zone enters Pinned instead of opening config.json

Releasing the Trigger in the Gear zone used to open config.json in the default
editor — the same thing the tray menu's "Edit config" already did, from the same
line of code. Two ways in to Notepad, and no way at all to touch the Menu with a
free mouse.

Rearranging Items, removing them, and editing them all need a free mouse, and
the press-and-hold Menu has none: the Trigger is held, and releasing it launches.
The only state where the mouse is free is Pinned, which until now existed solely
to host the add-item Popover. So the Gear zone now enters Pinned with no Popover
open, and Pinned stops meaning "the Popover is up" — it means the Menu holds
itself and the mouse and keyboard work. Dragging, removing, the Dodaj slot's
toggle, and opening a Popover on an existing Item all live inside it. Opening
config.json by hand stays available, from the tray only.

Rejected: a modifier chord (Shift+Trigger and friends — invisible, undiscoverable,
and the low-level hook would have to track a second button's state across the
whole session); a tray entry that summons the Menu in edit mode (the Menu's
entire premise is that it appears under the cursor, and a tray click puts the
cursor at the tray); a second mouse button held during press-and-hold (two
buttons down at once in a low-level hook, with no way to show what the second
one does).

The cost is that a gear glyph now opens something that is not a settings screen.
It is the closest thing the Menu has to "configure me", and the alternative was
inventing a second piece of Hub chrome for the same job.
