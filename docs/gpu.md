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
- `d3d12_xs`: `D3D12XboxCreateDevice` (`d3d12/device.rs`): version 0x00020004 only
  (else DXGI_ERROR_UNSUPPORTED), IID of ID3D12Device or any level, E_NOINTERFACE otherwise. One
  device per process (later calls AddRef the same one). Object 0x58 bytes with the out-of-line
  flag at +0x18 = 1; table of 160 slots, only IUnknown filled, every other slot a trap.
- `d3d12/rootsig.rs`, `d3d12/blob.rs`: `D3D12SerializeVersionedRootSignature` (1.0, 1.1, 1.2) and
  `D3D12SerializeRootSignature` (1.0 desc, serialized as 1.0 or 1.1) return an `ID3DBlob`
  (plain COM object) holding a DXBC container with one `RTS0` chunk; `deserialize` reads it back
  to a `RootSignature` model for `CreateRootSignature`. The DXBC checksum is written (modified
  MD5), not verified on read. Null desc is E_INVALIDARG, null blob out E_POINTER, bad version or
  parameter type E_INVALIDARG plus a text error blob.
- `d3d12/rootobj.rs`: `CreateRootSignature` (device slot 16): the blob is parsed with `deserialize`
  (DXBC with an `RTS0` chunk, other chunks skipped); the model is kept in the object (`+0x10`) for
  later `Set*RootSignature`. Null or empty blob and non-DXBC garbage are E_INVALIDARG; no methods
  of its own beyond `ID3D12DeviceChild`'s.
- `d3d12/pipeline.rs`: `CreateGraphicsPipelineState` (10), `CreateComputePipelineState` (11),
  `CreateComputePipelineStateX` (62, extensions ignored) and `CreatePipelineState` from a stream (86;
  console numbering, mesh 24 and amplification 23). The object keeps copies of each stage's
  bytecode, a clone of the root signature model, and for graphics the target/depth formats,
  topology type, sample description, deep-copied input layout and the raw description or stream.
  Nothing is compiled (G4). Null description, compute without code, unknown or overrunning stream
  subobject (logged) are E_INVALIDARG; null `out` is S_FALSE.
- `d3d12/pipeline.rs` (console extras): `DeserializeGraphicsPipelineStateX` / `DeserializeComputePipelineStateX` (57, 67: `*out` cleared, E_NOTIMPL, warns once), `SerializePipelineStateX` (56, 66: E_ACCESSDENIED), `SetDebugFlagsX` (52: S_OK).
- `d3d12/query.rs`: `CreateQueryHeap` (slot 39): keeps the description only; no results yet.
- `d3d12/descriptor.rs`: `CreateSamplerX` (slot 61, `+0x1e8`) records the console's 27-word sampler as
  `View::ConsoleSampler`.
- `d3d12/resource.rs`: `RegisterPagePoolX` / `UnregisterPagePoolX` (slots 74, 75): pools kept as
  `(address, pages)` behind a handle (index + 1); unaligned address, zero pages or null out are
  E_INVALIDARG; nothing reads them yet.
- `xg_xs.rs`, `texture/tile.rs`: `XGSuggestSwizzleMode` and `XGCreateTextureComputer` with the
  computer object (slots 0-12, no QI; 7, 11, 12 answer E_NOTIMPL), plus `tile`/`detile` of one
  subresource (round-trip tested over all modes); next stop: device slot 67
  (`DeserializeComputePipelineStateX`).
- `d3d12/signature.rs`: `CreateCommandSignature` (slot 41): stride and argument descriptions copied;
  the root signature is not kept.
- `d3d12/resource.rs`: `CreateCommittedResource` (slot 27) and `CreateCommittedResource3` (slot 127,
  `+0x3f8`): zeroed memory sized and aligned by the console layout, taken from the host
  reservation table (2^32..2^40) so its address is the GPU address; freed with the resource.
  Heap properties and clear value are not kept; a size error is E_INVALIDARG.
- `backend/vk` (no D3D12 yet): `Gpu::open()` loads Vulkan at run time (ash, no link-time
  dependency), picks discrete > integrated > CPU (`NATIV64_GPU=<index|name part>` overrides;
  needs 1.2, `VK_EXT_external_memory_host`, device address, timeline semaphores), one queue
  behind a mutex, `submit_once`. `import_host(ptr, len)` binds a buffer to caller pages (zero
  copy, alignment checked); `clear_and_read` proves the read-back. `NATIV64_VK_VALIDATION=1`
  enables the validation layer. Tests skip without a device.
- `wsi/wayland.rs`: `WaylandBackend` (a `WindowBackend`): one `xdg_toplevel` per HWND, connected
  at `new()` (an error without a compositor), globals bound at the first `on_create`.
  `poll_events` is non-blocking and maps configure to `WM_SIZE`, close to `WM_CLOSE`, the
  activated state to `WM_ACTIVATE`; no input yet. `nativ64::windowing` gains `RawSurface`,
  `WindowBackend::surface` / `client_size` and `window_surface` / `window_client_size`.
  `nativ64-run` installs it when `WAYLAND_DISPLAY` is set (`NATIV64_WINDOW=headless` opts out).
- `backend/vk/swapchain.rs`: `Gpu::create_swapchain(RawSurface, size)` (instance gets
  `VK_KHR_surface` + `VK_KHR_wayland_surface`, device `VK_KHR_swapchain`, when offered; headless
  `Gpu::open` is unchanged). FIFO, two frames in flight, rebuilt on resize / out-of-date /
  suboptimal; `present_clear(rgba)`. `nativ64-vkdemo [--frames N]` runs it through the windowing
  API with a cycling colour.
- `xmem`: `XMemVirtualAlloc` (placement by XMem flags: 32-bit below 4 GiB, else 4 GiB up to
  2^40..2^46; 2 MiB alignment; reserve then commit), `XMemAllocDefault`, `XMemFreeDefault`,
  `XMemAlloc`, `XMemFree` (call the title's `__XMemAllocationHooks__` export when it has one;
  the default allocator puts the GPU class 0x8000 in graphics memory, 64 KiB aligned; committed
  resources allocate through it, as on console, so the title's GPU pools see them),
  `XMemIsToolsMemAvailable`, `XMemAllocatePhysicalPages` (`(attributes, SIZE_T *count, ULONG_PTR
  *pages) -> BOOL`, 64 KiB pages: page numbers are slots of a sparse memfd pool, all or none,
  `count` reset to 0 on failure; mapping them into an address is not done yet). Model in the
  module header.
- `xcrdapi`: DirectStorage (`DStorageGetFactory`; factory, file, queue, status array as COM
  objects). Uncompressed memory/file to memory requests, run synchronously at `Submit`; errors
  in the queue's error record and event. TODO: zlib/BCPack, page and GPU destinations, fence
  signalling (recorded only). Details in the module header.
- Free paths to the host (what returns memory, what keeps it): `XMemFreePhysicalPages` punches the
  memfd range out (`FALLOC_FL_PUNCH_HOLE`), remaps the freed slot's views as private anonymous zeros
  (a view left on the file would fault the page back in) and records a change; unmapping or
  `MEM_RELEASE` of a view never frees the slot (only the title's `XMemFreePhysicalPages` does, as on
  the console). `MEM_DECOMMIT`/`MEM_RELEASE` are `mmap(PROT_NONE)`/`munmap`. The CRT heap and the
  default `XMemAlloc` are glibc: thresholds are fixed (`xmem::return_big_blocks_to_the_host`) so
  blocks of 1 MiB or more are `munmap`ped by `free`. The GPU keeps host-pointer imports
  (`gpumem.rs`: dropped on a recorded change, after 300 idle submissions, or LRU past `IMPORT_CAP`, 2 GiB: a lower cap churned dozens of imports and view records per frame) and
  placed images (dropped when memory under them changes). The memory line (every 600 frames) ends
  with `host: anon .. (trim freed ..), shmem .., xmem pages .., xmem file ..`: `xmem file` above
  `xmem pages` means freed pages stayed in the file.
- Lifetime of images (`d3d12/resource.rs`, `backend/vk/textures.rs`, `draw.rs`): releasing a
  resource clears its vtable word (`desc_of` / `info_of` refuse it) and queues its address
  (`take_released`). Each `execute` drains the queue first: the image and its cached views go,
  the registry entries that named it are classified again (a hole, or a texture when a new
  resource sits at the same address; `TexState::by_res` gives the indices) and their descriptors
  are written again. When `vkAllocateMemory` fails for an image, host-owned images (not GPU
  written) not used by the current submission are destroyed, least recently used first (placed
  images included), until it succeeds; they are made and uploaded again at next use. The sync
  visits every registered texture in each submission, so a working set above device memory
  evicts in a cycle. The root block arena is kept across submissions, made in `DrawState::begin`
  before any image, and doubled (or sized to the need if larger) when a submission has more draws.
  `ReplayStats` counts images and placed images alive with their bytes, released and evictions,
  printed every 600 frames and with `NATIV64_PROFILE`.
  `NATIV64_GPU_BACKEND=vulkan` runs Vulkan headless with no dump directory, to measure memory
  without the dump readbacks.

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
  (one shifted by 2 slots). We only trust slot numbers that our own run confirms. Confirmed
  on our title: the device follows VIctory's layout (slot 48 = `CreateCommandQueueX`); the
  command list uses the "newer" layout (every slot from 59 on shifted by +2; syncs at +0x418
  and +0x420); the queue uses the same "newer" layout (every slot from 14 on is two later than the older SDK's; confirmed by slot 18 = `GetTimestampFrequency`, which only fits there).
- **Methods returning a struct take the hidden return pointer first** (`rcx`), `this` second
  (`rdx`), unlike MSVC on PC: seen with gdb on `GetCustomHeapProperties`. Check every other
  struct-returning method (`GetCPUDescriptorHandleForHeapStart`, `GetDesc`...) at a call site
  before writing it. `ID3D12Resource::GetDesc` is the exception seen so far: `this` first (`rcx`),
  buffer second (`rdx`), as its call site loads them.
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

### Deviations decided during G1

- **View descriptors are ours, not RDNA2.** A CBV/SRV/UAV/sampler written through the device
  holds a tag and an index into our view registry (32 or 16 bytes, RTV/DSV in 128), so the
  title's byte copies of descriptors keep working. RDNA2 decoding is only needed if the
  title writes raw descriptors itself: the inline CBV path does, but our device's flag at
  +0x18 turns it off. Translated shaders (G4) will read the index.
- **Texture sizes need the real GFX10 layout.** Texture data in the archives is stored in
  the console's swizzled layout, so `GetResourceAllocationInfo`, `xg_xs` and the upload
  detiler must agree with the hardware's addressing (swizzle modes, mip tail, pitch). Own
  spec: `docs/textures.md`.

### PresentX (end of G1, input of G3)

- **Call:** queue slot 32 (+0x100), `HRESULT PresentX(this, u32 plane_count, const Plane *planes,
  const u8 *params)`. Confirmed on our title: it is the first call after the first frame's
  lists (clear RTV, timestamps, 64-bit wait).
- **Plane** (`D3D12XBOX_PRESENT_PLANE_PARAMETERS`): `+0x00 u64 token` (the frame token
  `WaitFrameEventX` handed out), `+0x08 u32 resource_count`, `+0x10 ID3D12Resource **resources`,
  `+0x18 const RECT *source_rect` (may be NULL), rest unread (destination, flags, colour space:
  to identify from the logged words). Stride 0x48: xcl's reading of another title, unverified
  on ours (only matters with two planes or more). `params` is unread.
- **Now (null backend), `d3d12/present.rs`:** never fails for the title (S_OK, as on the
  console and in both references), also for 0 planes or NULL pointers (logged once).
  1. On the first call and whenever the plane count changes, log the raw words of every
     plane (`0x48 / 8` u64 each) and the first 0x40 bytes of `params` when not NULL.
  2. Count the frame; keep the last 16 flips `(token, Instant)` in a ring, for
     `GetFrameStatisticsX` when the title shows it needs them (unchanged for now: count 0).
  3. Plane 0, resource 0: when it is one of our resources (vtable pointer is the resource
     table), read its description. Log frame 1 and every power of two: frame number, token,
     width x height, format, and the lists and commands submitted since the last line.
     A resource that is not ours is logged once with its address.
  4. Return S_OK. All earlier work is complete (null backend), so the flip is immediate.
- **G3:** `PresentX` becomes a queue operation after the submissions before it: the backend
  copies (or blits, with `source_rect`) plane 0's first resource to the swapchain image of
  the title's window and, with `NATIV64_DUMP_FRAMES=<dir>`, writes it as a PNG. Extra planes
  (a UI plane over the game's) are composed in order once the run shows any.

### CopyPageMappingsBatchX (queue slot 31, +0xf8)

- **Call** (VIctory, older layout slot 29; ours reached it after 64 frames):
  `HRESULT CopyPageMappingsBatchX(this, u32 count, const Batch *batches, u64 pool, u32
  pool_pages, u32 flags)`. `Batch { u64 destination; u32 count; const Range *ranges }` (16+8
  bytes, pointer at +0x10), `Range { u32 kind; u32 start; u32 count }`, pages of 64 KiB. From
  `destination` on, each range covers `count` pages, then the next range follows:
  kind 0 = pool pages `start..start+count` one to one; kind 1 = pool page `start` repeated
  `count` times; kind 2 = unmap; kind 3 = leave as is. Any other kind or a range past
  `pool_pages` fails that range (logged once), the call still returns S_OK.
- **`pool`**: VIctory reads it as the pool's address. Our `RegisterPagePoolX` hands out
  index + 1 handles: on the first call, log the raw arguments and accept both (a value that
  is a live handle maps to its pool's address; otherwise it is an address).
- **Mapping = aliasing, never a copy**: the title keeps writing the pool and expects the
  destination to see it. A pool page whose memory is one of our physical pages (memfd slots
  mapped by `XMemMapPhysicalPages[Scatter]`) is `mmap`ped `MAP_SHARED | MAP_FIXED` from that
  slot at the destination page. xmem keeps a reverse table (host page address → slot) filled
  by its map calls and cleared on remap/free, and exposes `alias_pages(dst, src, pages) ->
  bool` and `unmap_pages(dst, pages)` (unmap = fresh zero anonymous pages, as unmapped tiles
  read zero). A pool page that is not physical memory fails (logged once with the address).
- Done at once (null backend: nothing in flight). In G3, after the queue's earlier work.

### Validation

- Unit tests per object (slots, refcounts, recorded commands) with the null backend; Vulkan
  tests (clear + readback, host-pointer import, present to a headless surface) on lavapipe.
- `NATIV64_DUMP_FRAMES=<dir>` writes each presented image as a PNG: the real check of each
  milestone is the title's own frames, compared with what the title shows on a console
  (captures from the user or public footage of the same screen).
- `NATIV64_DUMP_TARGETS=<dir>` (with `NATIV64_DUMP_FRAMES`, same frames: 1, 2, 4, 8, ... and every
  60th) describes what the replay did in those frames: one `inspect: cmd` line per command, one
  `inspect: frame N draw` line per replayed draw (pipeline, targets, viewport, scissor, cull,
  depth and blend state, raw pipeline description, root arguments, the first descriptors of each
  table, the bytes of constant buffers, vertex buffers and root views), and after the present
  every image the replay holds as `<dir>/frame_NNNNN_res<id>_<w>x<h>.png` with its owner (gpu or
  host) and its count of non-zero bytes (formats the PNG dump does not convert are named only).
  It reads every image back, so a run is slow (use `--timeout` of 60 or more); pipeline serials
  are not stable from one run to the next (threads), read them within one run.
- `NATIV64_DUMP_EVERY=<n>` also keeps every `n`th frame (dense dumps around a moment of interest).
- `NATIV64_DUMP_FROM=<n>` skips the kept frames before `n` (to reach a late frame with
  `NATIV64_DUMP_TARGETS` in reasonable time); `NATIV64_DRAW_ONLY=<a>-<b>` records only draws `a`
  to `b` of each described frame (bisect which draw produces or breaks a pixel).
- `NATIV64_IMPORT_CAP_MB=<n>` overrides the host-pointer import cap (default 2048).
- `NATIV64_PROFILE=1` prints, every 60 presented frames, the seconds spent in each phase of a
  submission (texture sync, write watch, hashing, uploads, recording, dump) and their counters.

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
descriptor tracing, pipelines; draws, mesh draws and dispatches start replaying (compute and
mesh are half the title's programs). First target: the title's first splash/loading screen.
Design and chunk plan: [`docs/shaders.md`](shaders.md).

Later: indirect execution, queries, tiled resources (page pools), compressed
DirectStorage, the BVH builder, async overlap of CPU and GPU.
