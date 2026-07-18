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
One angular position on the Menu; shows either an Item or the Edit slot (always at the bottom).
_Avoid_: sector, position

**Tile**:
The visual square representing a Slot (icon or fallback letter).
_Avoid_: button, icon box

**Ring**:
The Menu's accent-colored border line. A fully bulged Tile's center sits exactly on it.
_Avoid_: border, outline, circle edge

**Rest radius**:
The radius inside the Ring where Tiles sit when the cursor is elsewhere.
_Avoid_: base position, default radius

**Dead zone**:
The central region of the Menu where releasing the Trigger dismisses without launching.
_Avoid_: center area, cancel zone

**Hover**:
The single Slot currently selected for launch, decided by angular sector — or none.
_Avoid_: focus, highlight

**Bulge**:
The dock-style visual swelling of Tiles, centered on the cursor's angle, driving each Tile's scale and outward shift through the Falloff.
_Avoid_: magnification, zoom, dock effect

**Falloff**:
The weight curve mapping a Tile's angular distance from the cursor to its share of the Bulge.
_Avoid_: easing, gradient

**Stagger**:
The per-Slot delay applied during the open and close animations.
_Avoid_: cascade, sequence delay

## Relationships

- A **Slot** shows exactly one **Item**, except the Edit slot
- **Hover** selects at most one **Slot**; release over it launches its **Item**
- The **Bulge** affects every **Tile** through the **Falloff**; it never decides what launches
- The **Bulge** fades to nothing while the cursor is in the **Dead zone**
- Launching is never delayed by animation: the close animation plays while the **Item** is already starting

## Example dialogue

> **Dev:** "When the cursor sweeps between two **Tiles**, which one is **Hovered**?"
> **Domain expert:** "Whichever **Slot**'s sector the cursor angle is in — the **Bulge** meanwhile peaks between them, because it follows the cursor, not the **Hover**."
> **Dev:** "And if I release right there?"
> **Domain expert:** "The **Hovered** Slot's **Item** launches immediately; the Menu animates out on its own time."

## Flagged ambiguities

- "hover" was used for both launch selection and the visual swelling — resolved: **Hover** is selection (sector-based, discrete), **Bulge** is visual (cursor-angle-based, continuous).
