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
One angular position on the Menu; shows either an Item or the meta "Dodaj" slot (always last, so its clock position shifts with the Item count).
_Avoid_: sector, position

**Tile**:
The visual square representing a Slot (icon or fallback letter), plus its always-visible caption underneath.
_Avoid_: button, icon box

**Scrim**:
The Menu's neutral backdrop circle (a faint fill, a barely-there border) that everything else sits on top of.
_Avoid_: ring, border, circle edge

**Hub**:
The circle at the Menu's center. Idle, it shows a dot; while a Slot is Hovered, it shows that Item's name and a "release to launch" subtitle.
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
The Menu state entered by releasing the Trigger over the "Dodaj" Slot: the Menu stays up without the Trigger held, the window takes keyboard focus, and the Popover is open. Clicking away, Escape, commit, or a fresh Trigger press leaves it.
_Avoid_: sticky mode, edit mode

**Popover**:
The inline add-item form (name, target, browse, icon picker, commit/cancel) that expands out of the "Dodaj" Tile while Pinned.
_Avoid_: popup, dialog, modal, panel

**Gear zone**:
The Hub's bottom segment; releasing the Trigger there opens config.json in the default editor — the secondary, manual editing path.
_Avoid_: settings button

## Relationships

- A **Slot** shows exactly one **Item**, except the meta "Dodaj" slot
- **Hover** selects at most one **Slot**; release over it launches its **Item**
- The **Arc** and **Hub** both track **Hover**; neither decides what launches, they only preview it
- Losing **Hover** (cursor back in the **Dead zone**) simply fades the **Arc** out and the **Hub** back to its idle dot — no separate "cancelled" state exists
- Launching is never delayed by animation: the close animation plays while the **Item** is already starting

## Example dialogue

> **Dev:** "When the cursor sweeps between two **Tiles**, which one is **Hovered**?"
> **Domain expert:** "Whichever **Slot**'s sector the cursor angle is in — sector, not proximity to the Tile itself, and it keeps working even past the **Scrim**'s edge."
> **Dev:** "And if I release right there?"
> **Domain expert:** "The **Hovered** Slot's **Item** launches immediately; the Menu animates out on its own time. Release in the **Dead zone** instead and nothing launches — the Menu just closes."
