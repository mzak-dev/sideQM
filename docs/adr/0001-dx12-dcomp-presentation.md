# DX12 with DirectComposition presentation instead of wgpu defaults

The Menu needs per-pixel window transparency. wgpu 30's default backend
selection picks Vulkan, which on this machine's AMD driver dies with
STATUS_ACCESS_VIOLATION ~2 seconds after the first present to a transparent
winit window — a silent process kill that also tears down the global mouse
hook. Plain hwnd swapchains (Vulkan or DX12) additionally only offer
`CompositeAlphaMode::Opaque` here, so transparency is impossible on them
anyway. We therefore pin `Backends::DX12` with
`Dx12SwapchainKind::DxgiFromVisual` (wgpu-managed DirectComposition visual)
plus winit's `with_no_redirection_bitmap(true)` — without the latter the GDI
redirection bitmap covers the DComp visual and the window renders invisible.
This is the only combination on this machine that is both stable and
delivers `PreMultiplied` alpha. `SIDEQM_BACKEND=vulkan|gl` env override
exists for re-testing after driver/wgpu updates.
