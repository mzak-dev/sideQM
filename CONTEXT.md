# sideQM

A Windows background utility: hold a mouse side button to summon a translucent
circular launcher at the cursor; release over an entry to launch it.

## Language

**Menu**:
The circular overlay summoned by holding the Trigger.
_Avoid_: wheel, radial, circle menu, overlay

**Trigger**:
The mouse side button that owns the Menu — press shows, release acts.
_Avoid_: hotkey, activation button

**Item**:
A user-configured launch target (exe, URL, folder, document).
_Avoid_: app, shortcut, entry

**Slot**:
One angular position on the Menu; shows either an Item or the meta "Dodaj" slot. Slots divide the circle evenly, so every Slot's clock position shifts with the Slot count.
_Avoid_: sector, position

**Dodaj slot**:
The meta Slot that opens the Popover on an empty form. Two independent things are true of it: where it sits (first or last among the Slots) and whether it shows at all. Hiding it never forgets where it sat, so showing it again puts it back. Hidden, the Menu is Items only, and the Hub's toggle is the way to get it back.
_Avoid_: add button, plus tile, new-item slot

**Tile**:
The visual square representing a Slot (icon or fallback letter), plus its always-visible caption underneath. While Pinned it gains two affordances an Item Tile has nowhere else: it can be dragged to another Slot, and it carries a remove control in its corner. The Dodaj slot's Tile has neither.
_Avoid_: button, icon box

**Icon**:
The image on a Tile. It has two possible sources, in precedence order: the Item's own icon file, else the one extracted from whatever the Item launches. A Tile whose Icon is missing, still loading, or unreadable shows its fallback letter instead — the Menu never waits for one and never breaks over one.
_Avoid_: image, thumbnail, glyph, bitmap

**Icon Library**:
The folder of icon files the app owns, alongside config.json. Choosing an Icon in the Popover copies the file in and points the Item at the copy, so moving or deleting the original leaves the Item intact. It holds the only copy of nothing else, but it is a library, not a cache: regenerating it is impossible, and deleting it loses Icons.
_Avoid_: icon cache, cached_icons, thumbnail store

**Scrim**:
The Menu's neutral backdrop circle (a faint fill, a barely-there border) that everything else sits on top of.
_Avoid_: ring, border, circle edge

**Hub**:
The circle at the Menu's center, and the Menu's one piece of chrome. Idle, it shows a dot; while a Slot is Hovered, it shows that Item's name and a "release to launch" subtitle; while Pinned with no Popover open, it shows the Dodaj slot's toggle — and there it is never drawn smaller than that toggle needs, however small the configured Hub is. A Popover, when open, covers it entirely.
_Avoid_: center dot, puck

**Arc**:
The accent-colored indicator riding just outside the Scrim's edge. Springs (shortest angular path) to point at the Hovered Slot; hidden entirely when Hover is none.
_Avoid_: indicator ring, pointer, highlight arc

**Rest radius**:
The fixed radius where Tiles sit.
_Avoid_: base position, default radius

**Dead zone**:
The central region of the Menu, sized to the Hub's radius, where releasing the Trigger dismisses without launching — and without any distinct "cancelled" visual.
_Avoid_: center area, cancel zone

**Hover**:
The single Slot currently selected for launch, decided by angular sector — or none. The whole wedge is the target: there's no outer edge past which Hover stops working.
_Avoid_: focus, highlight

**Stagger**:
The per-Slot delay applied during the open and close animations.
_Avoid_: cascade, sequence delay

**Pinned**:
The Menu state where it stays up without the Trigger held: the window takes keyboard focus and the mouse is free. Entered by releasing the Trigger over the "Dodaj" Slot or in the Gear zone. Left by the Done button, clicking outside the Menu, Escape, or a fresh Trigger press — committing a Popover does not. Pinned is a state in its own right; a Popover may or may not be open over it.
_Avoid_: sticky mode, edit mode

**Popover**:
The inline item form (name, target, browse, icon picker, commit/cancel) that expands out of a Tile while Pinned. It either adds a new Item or edits an existing one; nothing else opens it. It is modal within Pinned: while it is open the Tiles are inert — no dragging, no removing — and closing it, either way, returns to Pinned rather than dismissing the Menu.
_Avoid_: popup, dialog, modal, panel

**Gear zone**:
The Hub's bottom segment; releasing the Trigger there enters Pinned with no Popover open — the way in to rearranging and editing. Opening config.json by hand is the tray's job, not the Menu's.
_Avoid_: settings button

**Done button**:
What the Gear zone becomes while Pinned: the same bottom segment of the Hub, now the way out. The way in and the way out share one place, so leaving is where the user last clicked to arrive.
_Avoid_: close button, exit, OK

## Relationships

- A **Slot** shows exactly one **Item**, except the meta "Dodaj" slot
- A **Tile** shows its **Item**'s **Icon** when there is one, and its fallback letter otherwise — including while the Icon is still being read
- **Hover** selects at most one **Slot**; release over it launches its **Item**
- The **Arc** and **Hub** both track **Hover**; neither decides what launches, they only preview it
- Losing **Hover** (cursor back in the **Dead zone**) simply fades the **Arc** out and the **Hub** back to its idle dot — no separate "cancelled" state exists
- Launching is never delayed by animation: the close animation plays while the **Item** is already starting
- Dragging a **Tile** reorders the **Items**: the dragged one takes the target **Slot**'s place and the ones between it and its old place shift over — nothing is swapped
- Adding, removing, or reordering an **Item** changes the **Slot** count or order, so every **Tile** moves to a new angle — the **Menu** is always evenly divided
- Every change made while **Pinned** is saved the moment it happens, and what gets saved is what the **Menu** shows — the config file is a recording of the visible state, not a separate truth to reconcile with it
- Removing an **Item** never touches the **Icon Library**: two Items can share one stored file, so the file outlives the Item that referenced it
- A **Menu** with no **Slots** at all — no Items, **Dodaj slot** hidden — is a legal state, not a broken one: the **Scrim** and **Hub** still draw, and the **Gear zone** still works, so **Pinned** is always reachable and the user can never lock themselves out

## Example dialogue

> **Dev:** "When the cursor sweeps between two **Tiles**, which one is **Hovered**?"
> **Domain expert:** "Whichever **Slot**'s sector the cursor angle is in — sector, not proximity to the Tile itself, and it keeps working even past the **Scrim**'s edge."
> **Dev:** "And if I release right there?"
> **Domain expert:** "The **Hovered** Slot's **Item** launches immediately; the Menu animates out on its own time. Release in the **Dead zone** instead and nothing launches — the Menu just closes."
