# nativ64-gpu: the graphics side

`crates/nativ64-gpu` holds the graphics modules a title imports (Xbox Direct3D 12, XMem
graphics memory, DirectStorage, later `xg_xs`), apart from `crates/nativ64`, which stays host
APIs only (libc as its only dependency).

## Role

- Same registry type as the library: exports are `nativ64::Export` values declared with
  `nativ64::win32!` (one `module "name";` per file), listed in `TABLES`, found with
  `nativ64_gpu::lookup(module, name)`.
- The loader binds each import through `nativ64::lookup` first, then `nativ64_gpu::lookup`;
  what neither knows gets a trap stub.

## What exists

- `d3d12_xs`: `D3D12ConfigureVirtualMemoryX` (E_INVALIDARG on null, else the out range is all
  of user space, `0x10000..0x7fff_fffe_ffff`: GPU addresses are CPU addresses).
- `xmem`: `XMemVirtualAlloc` (placement by XMem flags: 32-bit below 4 GiB, else 4 GiB up to
  2^40..2^46; 2 MiB alignment; reserve then commit), `XMemAllocDefault`, `XMemFreeDefault`,
  `XMemAlloc`, `XMemFree` (shortcut: the title's allocation hooks are not consulted),
  `XMemIsToolsMemAvailable`, `XMemAllocatePhysicalPages` (`(attributes, SIZE_T *count, ULONG_PTR
  *pages) -> BOOL`, 64 KiB pages: page numbers are slots of a sparse memfd pool, all or none,
  `count` reset to 0 on failure; mapping them into an address is not done yet). Model in the
  module header.
- `xcrdapi`: DirectStorage (`DStorageGetFactory`; factory, file, queue, status array as COM
  objects). Uncompressed memory/file to memory requests, run synchronously at `Submit`; errors
  in the queue's error record and event. TODO: zlib/BCPack, page and GPU destinations, fence
  signalling (recorded only). Details in the module header.

## What the title expects (facts from reading VIctory, to confirm on our run)

The executable imports from `d3d12_xs` only `D3D12XboxCreateDevice` and
`D3D12SerializeVersionedRootSignature` (plus `D3D12ConfigureVirtualMemoryX`); from `xg_xs`
`XGCreateTextureComputer` and `XGSuggestSwizzleMode`; `XboxBvhBuilder.dll` imports one more
`d3d12_xs` function. Everything else goes through objects.

- **The runtime is mostly inlined in the title.** The Xbox D3D12 headers compile most methods
  into the title: they write GPU packets straight into the command list's memory while its
  write pointer is below a limit, and read fields of the device object at fixed offsets. A
  device whose "out of line" flag is set, and command lists whose write pointer and limits are
  0, make every method fall back to a vtable call. That is how we see each command without
  parsing packets.
- **Objects are not plain COM.** Each is a `#[repr(C)]` struct whose first field is the vtable,
  followed by fields the inlined code reads (device: 0x58 bytes, with the out-of-line flags).
  The vtables are long (160 slots for the device and the command list) and follow the PC
  interfaces (IUnknown, ID3D12Object, ID3D12Device..Device8), then console-only slots: queue
  `PresentX` and `ExecuteCommandListsX`, device `CreateCommandQueueX`/`CreateCommandListX`,
  placed views from a GPU address, page pools, frame pacing (`SetFrameIntervalX`,
  `ScheduleFrameEventX`, `WaitFrameEventX`, statistics).
- **Slot layouts depend on the title's SDK.** VIctory carries two queue and two list layouts
  (one shifted by 2 slots). We only trust slot numbers that our own run confirms.
- **`D3D12XboxCreateDevice(adapter, params, iid, out)`**: `params[0]` is the interface version
  (0x00020004 for the title VIctory follows), the rest (ring sizes, opt-outs) can be ignored at
  first; `iid` is one of ID3D12Device, Device1..Device8, Device11.
- **Memory is unified.** A resource's GPU address is its CPU address in memory the title got
  from `XMemAlloc`; descriptor heaps are title memory holding raw RDNA2 descriptors (32-byte
  views, 16-byte samplers) that the title may write or copy by itself; descriptor handles are
  addresses.
- **Shaders are DXBC containers** whose DXIL part carries Xbox additions: an `XDAT` table,
  options, and a zlib-compressed GFX10 (RDNA2 ISA) program. The DXIL bitcode may use
  console-only intrinsics, so the ISA is the input to translate. AMD publishes the RDNA2 ISA
  reference; it is our specification for the decoder.
- **Presentation** is `PresentX(planes, Plane *, params)` on a queue: a frame token and a list
  of resources per plane. Pacing is the title's frame-interval calls, the token being the frame
  number.

## Design

### Principles

- Own code, clean licences: Vulkan through `ash` (MIT/Apache, bindings only), Wayland through
  `wayland-client` + `wayland-protocols` (MIT), zlib through `miniz_oxide`. No vkd3d,
  dxil-spirv, DXVK, Mesa code. VIctory is read for facts (offsets, slot meanings, formats),
  never copied (GPL-3).
- **Unknown slot = stop, like an unresolved import.** Every slot of every table starts as a
  trap that prints `nativ64: unimplemented ID3D12Device slot 12 (+0x60)` and aborts. The loop
  stays run → implement the slot that stopped → rerun, one Sonnet agent per round.
- **Record, then replay.** Object methods record into our own command stream (a Rust enum per
  command, with resolved resources); a backend replays it at `ExecuteCommandLists[X]`. The
  object model is then testable without a GPU, and the Vulkan part is a consumer of plain data.

### Layers and modules (in `crates/nativ64-gpu/src`)

| Module | Content |
|---|---|
| `xobj.rs` | Object header (vtable pointer, refcount, then the fields the inlined code reads), `SlotTable<N>` built at compile time with one trap per slot (each knows its interface name and index), `QueryInterface` against an IID list. Not `nativ64::com::ComObject`, whose layout is ours, not the console's. |
| `d3d12/device.rs` | `D3D12XboxCreateDevice` (version check, IID check, one device per process), the device table. |
| `d3d12/queue.rs`, `list.rs`, `allocator.rs` | Queues, command lists (write pointer and limits 0, methods push `Command`s), allocators. |
| `d3d12/resource.rs`, `heap.rs` | Committed/placed resources: description, GPU address (title memory), state. |
| `d3d12/descriptor.rs` | Descriptor heaps in title memory; RTV/DSV in our own 128-byte format; RDNA2 view and sampler descriptors decoded at replay into `ViewKey`s. |
| `d3d12/fence.rs` | Fences: value in title-visible memory if the inlined code reads it (to check), else in the object; events signalled by the backend. |
| `d3d12/pipeline.rs`, `signature.rs` | Pipelines and root signatures keep their blobs; translation happens at first use. `D3D12SerializeVersionedRootSignature` is our own serializer (format is the public D3D12 one). |
| `d3d12/present.rs` | `PresentX` and frame pacing (software clock on the frame interval). |
| `backend/mod.rs` | `trait Backend`: `execute(&[CommandStream])`, `present(resource, hwnd)`, `signal(fence, value)`. `NullBackend` (records, completes at once) is the default until Vulkan works, and stays for tests. |
| `backend/vk/` | Vulkan backend: instance and device choice, memory, images, replay, swapchain. |
| `wsi/wayland.rs` | `WindowBackend` (from `nativ64::windowing`) over Wayland: `xdg_toplevel` per HWND, input/resize/close turned into `Msg`s in `poll_events`. |

`nativ64::windowing::WindowBackend` gains one method, `surface(hwnd) -> Option<RawSurface>`
(`RawSurface::Wayland { display, surface }`, raw pointers), so the Vulkan backend can create a
`VkSurfaceKHR` without `nativ64` knowing Wayland. The loader installs the Wayland backend when
`WAYLAND_DISPLAY` is set, headless otherwise (`NATIV64_WINDOW=headless` forces it).

### Choices made

- **Window: Wayland directly** (`wayland-client` with the `client_system` feature, which gives
  the raw `wl_display *` Vulkan needs). The session here is Wayland and all three GPUs offer
  `VK_KHR_wayland_surface`. Our window model already pumps events on the title's thread inside
  `PeekMessage`, which suits a non-blocking `dispatch_pending` + `prepare_read`. winit is ruled
  out: it wants to own the event loop. X11 (`x11rb`) only if someone needs it. No server-side
  decorations are required; the window can be borderless or fullscreen.
- **GPU choice:** the discrete GPU first (here an RTX 3060 Laptop, besides an Intel UHD and
  llvmpipe), `NATIV64_GPU=<index or name part>` to override. CI runs the Vulkan tests on
  lavapipe (Mesa's CPU driver).
- **Buffers without copies.** All three drivers here have `VK_EXT_external_memory_host` and
  `VK_KHR_buffer_device_address`: title memory is imported as `VkDeviceMemory`, buffers are
  bound to it, and a GPU address stays the title's address. No shadow copies and no page
  watching, unlike VIctory (which needs them on D3D12). Cost: on the discrete GPU these
  buffers live in system memory over PCIe; acceptable until measured.
- **Textures and render targets are device-local `VkImage`s.** The title lays textures out with
  RDNA swizzle modes (`xg_xs`); uploads detile on the CPU first (compute shader later). A
  texture the title writes with the CPU after creation is re-uploaded when its pages change
  (checked at submit by a cheap hash first, write-protect later if needed).
- **Descriptors:** at replay, RDNA2 descriptors read from title memory are decoded to
  `ViewKey`s (address, format, dimensions, swizzle) and cached as `VkImageView`/samplers in a
  bindless array (`VK_EXT_descriptor_indexing`, core in 1.2). Translated shaders look
  descriptors up by index in that array (see Shaders).
- **Synchronisation:** first, every `ExecuteCommandLists` waits for idle (CPU and GPU never
  overlap, as in VIctory). Then D3D12 fences map to Vulkan timeline semaphores (core 1.2), one
  per fence, and queue `Wait`/`Signal` become semaphore waits/signals. One mutex per `VkQueue`.
- **Shaders: GFX10 ISA → our IR → SPIR-V.** Steps: unpack the container (DXBC parts, `XDAT`,
  zlib) → decode GFX10 (from the AMD RDNA2 ISA reference) → SSA IR → structurise (EXEC masks
  become per-lane booleans; SPIR-V needs structured control flow) → resource tracing (which
  descriptor a load comes from: root parameter, heap index) → SPIR-V emitter (own code over the
  `spirv` enums crate, Apache-2.0) → validate with `spirv-val` in tests when present. Cached on
  disk by content hash under `$XDG_CACHE_HOME/nativ64/shaders`. VIctory's version of this is
  about 23k lines (decoder 3.6k, IR + structuring 3.3k, resources 3.6k, DXIL emitter 19k): it
  is the long part, with its own spec before any code.

### Validation

- Unit tests per object (slots, refcounts, recorded commands) with the null backend; Vulkan
  tests (clear + readback, host-pointer import, present to a headless surface) on lavapipe.
- `NATIV64_DUMP_FRAMES=<dir>` writes each presented image as a PNG: the real check of each
  milestone is the title's own frames, compared with what the title shows on a console
  (captures from the user or public footage of the same screen).

## Milestones and chunks

Each chunk is one Sonnet agent with this spec and tests; Opus reviews after each.

**G1 — object model on the null backend (no Vulkan).** Goal: the title runs past device
creation up to its first `PresentX`, every call recorded.
1. `xobj.rs`: header, `SlotTable` with per-slot traps, refcount, `QueryInterface`. Tests:
   trap message names interface and slot, QI on listed/unlisted IIDs, release frees.
2. `D3D12XboxCreateDevice` + device table (version and IID checks, out-of-line flags at the
   offsets the inlined code reads: confirm them with `objdump -d` around the import's call
   site before writing them). `D3D12SerializeVersionedRootSignature`.
3. Then the trap loop: each round implements the slot that stopped (queues, allocators,
   lists, resources, heaps, views, fences, pipelines stored as blobs, frame pacing, `PresentX`
   recorded) until the title presents frames. `xg_xs` exports as they come.
4. Remaining `xmem`: Free/Map/MapScatter of physical pages (mmap of the memfd slots over a
   reserved range), GetWorkingSetStatistics, TransferMemory.

**G2 — Vulkan and the window (no title).**
1. `backend/vk`: instance, device choice, queue, host-pointer import, a clear replayed from a
   `CommandStream` and read back. Tests on lavapipe; CI installs Mesa's Vulkan drivers.
2. `wsi/wayland.rs` + `RawSurface` + swapchain; a small `nativ64-vkdemo` binary that opens a
   window and presents a changing clear colour (checked by running it).

**G3 — first image.** The Vulkan backend replays the title's real streams: render targets as
images, clears, copies, barriers, `PresentX` to the window, `NATIV64_DUMP_FRAMES`. Draws are
counted and skipped. Done when the title's cleared frames reach the window.

**G4 — shaders.** Spec first (`docs/shaders.md`), then decoder, IR, structuring, SPIR-V,
descriptor tracing, pipelines; draws start replaying. First target: the title's first
splash/loading screen.

Later: compute, indirect execution, queries, tiled resources (page pools), compressed
DirectStorage, the BVH builder, async overlap of CPU and GPU.
