# Transport buttons respond to a left click, not the Trigger

Every existing Menu control (Slots, Gear zone, Done button, Dodaj toggle) fires on releasing
the Trigger, and `MouseInput` handling has so far only run while Pinned — the held-Trigger
Menu was assumed to hold the user's whole hand, leaving nothing free to click with.

Transport buttons (prev / play-pause / next, part of Now Playing) break both assumptions on
purpose: they fire on an explicit left-button click, and that click works during the ordinary
held-Trigger Menu — not while Pinned, the opposite of every other `MouseInput` handler. The
point is skipping a track without letting go of the Trigger; this is safe because the Trigger
is a side button worked by the thumb, leaving the index finger free for the left button. Held-
only also isn't optional: Now Playing never draws while Pinned (the Hub belongs to the Dodaj
toggle there), and the PlayPause button's screen position overlaps that toggle exactly, so
letting the click fire while Pinned steals the toggle's clicks instead of reaching it. This is
the first Menu control not driven by hover-then-release, so the reason is worth recording
before someone "fixes" the inconsistency.
