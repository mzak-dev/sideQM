# Never resize the swapchain at runtime; the Popover draws inside the fixed window

> **Superseded by [ADR-0007](0007-cpu-raster-layered-window.md).** There is no
> swapchain any more, so resizing is safe. The Popover still draws inside the
> fixed window, but now because that layout works rather than because growing
> the window would reset the driver.

Opening the Popover used to grow the window (`request_inner_size`) so the panel
could sit radially outside the Scrim — the only runtime resize in the app, and
this machine's AMD driver resets on it (`ResizeBuffers` on a live
DirectComposition swapchain, the same driver ADR-0001 already works around).
We therefore keep the window at its startup size for its whole life and draw
the Popover centered over the Menu, inside the existing bounds; `window_size`
is floored so the panel always fits.

Rejected: pre-sizing the window to the radial layout's worst case (~1240px —
doesn't fit a 1080p screen vertically), a native Win32 form window (loses the
Menu's visual style), and a second wgpu window (a second swapchain on the same
fragile driver).

The one `request_inner_size` left is the config-reload path, which only runs
while the window is hidden and the present loop is idle; it predates the
Popover and has never produced a reset. If it ever does, the fallback is
recreating the surface at the new size instead of reconfiguring it.
