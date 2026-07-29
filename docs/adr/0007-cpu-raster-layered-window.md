# CPU rasterization onto a layered window instead of a GPU swapchain

Supersedes ADR-0001 and ADR-0002.

Every rendering crash this project collected came from the same place: an
application-owned GPU surface composited with per-pixel alpha. Vulkan
access-violated on the first present (ADR-0001), `ResizeBuffers` on a live
DirectComposition swapchain reset the driver (ADR-0002), and a diverged spring
that filled the screen with overdraw could reset it too (`anim.rs`). None of
these were bugs in what we drew — they were the presentation path reacting
badly to this machine's AMD driver.

So the Menu is now rasterized on the CPU into a `tiny_skia::Pixmap` and handed
to DWM through `UpdateLayeredWindow`. The application allocates no GPU
resources at all, which removes that entire class of failure rather than
working around another instance of it. `tiny-skia` draws the shapes that
`shader.wgsl` used to (rounded boxes, an arc stroke, a circle segment) and
`cosmic-text` shapes and rasterizes the glyphs that glyphon used to; both were
already in the dependency tree via resvg and glyphon, so the swap removed four
direct dependencies (wgpu, glyphon, pollster, bytemuck) and added two.

The window is roughly 460px square and redraws only while animating, so CPU
rasterization is comfortably affordable — a frame costs well under a
millisecond, and `DwmFlush` paces the loop to the compositor instead of letting
it spin.

Consequences worth knowing:

- **Runtime resize is legal again.** ADR-0002's ban existed only because of
  `ResizeBuffers`; `UpdateLayeredWindow` takes the size with every frame. The
  Popover still draws inside fixed bounds because that layout works, not
  because growing the window is dangerous.
- **Overdraw is now merely slow, not fatal.** The clamps in `config.rs` and
  `anim.rs` stay, because a diverged spring is still a bug worth containing.
- **The icon worker could hand over finished pixmaps.** ADR-0004's constraint
  that uploads happen on the event-loop thread was about GPU resource
  ownership, which no longer applies. Not worth changing until it costs
  something.
- **`WS_EX_LAYERED` must be re-asserted per frame.** winit's Windows backend
  rewrites the whole ex-style from its own flag model whenever anything
  changes it, wiping bits set behind its back; `present::ensure_layered` puts
  it back. Losing it makes every present fail with ERROR_INVALID_PARAMETER.
- **winit's `with_transparent(true)` is gone.** It asked DWM for blur-behind,
  which the DirectComposition path needed; a layered window carries its own
  per-pixel alpha.

Rejected: `skia-safe` on its GPU backend (Ganesh over D3D or Vulkan) — the
same presentation path and therefore the same risk, bought with a heavy C++
build. `skia-safe` on its CPU backend — the right shape, but a large native
dependency to draw three primitive kinds and about fifteen short strings.
Drawing into an `IDCompositionSurface` — pulls a D3D11 device back in, which
is what we set out to remove.
