# The file picker and icon decoding run off the event loop

Both used to run inline in the winit handler. The file picker had a bug that
looks impossible until you know the mechanism, so it is worth writing down.

## The picker

Opening the picker made the Popover vanish and lost whatever the user had typed.
The code looked correct: `in_dialog = true` before `pick_file`, `false` after,
and the `Focused(false)` handler skips its discard while `in_dialog` is set.

The flag can never work there. `pick_file` is called from inside
`window_event`, and winit **buffers** events raised while a user callback is
running (`should_buffer()` is true whenever the handler has been taken out of
its cell) and dispatches them after the callback returns. So the `Focused(false)`
caused by the dialog stealing focus was delivered *after* `in_dialog` had been
cleared — the guard's lifetime and the event's arrival could not overlap. Both
browse buttons were affected.

The picker now runs on a one-shot STA thread and reports back as an
`AppEvent::FilePicked` through the `EventLoopProxy`. Nothing blocks the handler,
so nothing is buffered, and `in_dialog` finally spans the dialog's real
lifetime. The Menu also keeps animating behind the dialog, which it never did.

Rejected: holding `in_dialog` until the next `Focused(true)` (still blocks the
loop, and a user who dismisses the dialog by clicking a third window may never
send that event); checking `GetForegroundWindow` when the focus is lost (treats
the symptom, and the loop still blocks). Neither removes the reentrancy, which
is the actual defect.

## Icon decoding

The same delivery path carries decoded icons. Rasterizing an SVG or pulling a
256px shell icon is slow enough to stutter the Menu's entrance, and the Popover
re-extracted an icon on every keystroke. A single worker thread now does the
decoding; the event loop only builds cache keys and converts finished pixels
into the renderer's own format. That last step was once a hard constraint (GPU
resources belonged to the thread owning the Device and Queue); since ADR-0007
made the renderer CPU-side it is merely where the code happens to sit.

Results are matched to Tiles by key, not by remembering which request asked for
them — a config reload mid-decode leaves no Tile holding that key, so the result
simply warms the cache instead of landing on the wrong Tile. Queued preview jobs
collapse to the newest one, which is the keystroke debounce for free.

The consequence to know: a Tile shows its fallback letter until its icon
arrives. Cached icons are applied synchronously, so this is visible only on the
first open after a config change.
