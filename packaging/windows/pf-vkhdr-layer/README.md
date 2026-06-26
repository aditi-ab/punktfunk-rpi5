# pf-vkhdr-layer — HDR Vulkan layer for the virtual display

A tiny Vulkan **implicit layer** (`VK_LAYER_PUNKTFUNK_hdr_inject`) that lets **Vulkan games enable
HDR while streaming over the punktfunk virtual display**.

## The problem it solves

On Windows, NVIDIA/AMD Vulkan ICDs do **not** advertise any HDR color space
(`VK_COLOR_SPACE_HDR10_ST2084_EXT`, `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`) for a surface that
lives on an **IddCx indirect / virtual display** — even when Windows "Use HDR" is on and the desktop
is composited at 10-bit. So Vulkan games (Doom: The Dark Ages and the rest of id Tech, Indiana Jones
and the Great Circle, …) query `vkGetPhysicalDeviceSurfaceFormatsKHR`, find no HDR color space, and
refuse HDR ("This device does not support HDR"). D3D11/D3D12 HDR works on the very same display
because the OS compositor drives it — only the **Vulkan WSI enumeration** is gated.

This was long believed unfixable from outside the GPU driver (the Apollo/Sunshine/Virtual-Display-
Driver communities all concluded "it's in Windows's kernel"). It isn't: an on-box experiment proved
the ICD happily **accepts and presents a *forced* HDR swapchain** on that exact virtual-display
surface (`vkCreateSwapchainKHR` + `vkSetHdrMetadataEXT` + present all succeed) — it simply won't
*advertise* the format. So the whole fix is to add the HDR surface formats to the enumeration the
game queries; once the game requests that swapchain, the ICD honors it. **Validated live: Doom: The
Dark Ages enables HDR over the virtual display with this layer.**

## What it does

- Intercepts `vkGetPhysicalDeviceSurfaceFormatsKHR` / `...2KHR`, calls down to the ICD, and appends
  `{A2B10G10R10_UNORM_PACK32, HDR10_ST2084_EXT}` + `{R16G16B16A16_SFLOAT, EXTENDED_SRGB_LINEAR_EXT}`
  (deduped — a no-op on real HDR monitors that already list them).
- **Self-gates**: it only injects when the surface's monitor actually has Windows advanced-color
  (HDR) *enabled* right now (checked via `DisplayConfigGetDeviceInfo` / `GET_ADVANCED_COLOR_INFO`).
  So it does **nothing** on SDR sessions/displays — no washed-out "SDR-in-HDR". It tracks
  `VkSurfaceKHR → HWND` by intercepting `vkCreateWin32SurfaceKHR`.
- Everything else is pass-through dispatch chaining (instance + device).

It is shipped as an **always-on** implicit layer (loads via the registry, so it works regardless of
how a game is launched — including via an already-running Steam, which env-based scoping can't
guarantee). Because of the self-gate it is inert outside HDR streaming.

## Controls

| Variable | Effect |
|---|---|
| `DISABLE_PF_VKHDR=1` | Loader-standard off-switch — disables the whole layer for that process. |
| `PF_VKHDR_EXCLUDE=foo.exe,bar.exe` | Extra exe basenames to skip (in addition to a small built-in kernel-anti-cheat default list: `cs2.exe`, `rainbowsix.exe`, …). |
| `PF_VKHDR_LOG=1` | Write a debug log to `%TEMP%\pf_vkhdr_layer.log`. |

## Build / install

Standalone crate (own `[workspace]`), Windows-only `cdylib`:

```sh
cargo build --release           # -> target/release/pf_vkhdr_layer.dll
```

The host installer (`packaging/windows/pack-host-installer.ps1` → `punktfunk-host.iss`) builds it,
lays `pf_vkhdr_layer.dll` + `pf_vkhdr_layer.json` into `{app}\vklayer`, and registers it under
`HKLM64\SOFTWARE\Khronos\Vulkan\ImplicitLayers` (opt-out task "Install the HDR Vulkan layer").

Manual dev install: drop the DLL + JSON in one directory and add a `REG_DWORD` value named after the
JSON's full path (data `0`) under `HKLM\SOFTWARE\Khronos\Vulkan\ImplicitLayers` (or `HKCU\...`).
Confirm with `vulkaninfo` (Surface section) — `HDR10_ST2084_EXT` should appear when the display has
HDR enabled.

## Notes

- x64 only (the Windows host is x64 only).
- Anti-cheat: the layer is benign (it only *adds* surface formats; it never touches rendering,
  memory, or input) and is signed, but because it's always-on it is *present* in every Vulkan
  process. The built-in exclude list + `PF_VKHDR_EXCLUDE` + `DISABLE_PF_VKHDR` cover kernel-anti-
  cheat titles you'd rather it stay out of.
