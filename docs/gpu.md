# nativ64-gpu: the graphics side

`crates/nativ64-gpu` holds the graphics modules a title imports (Xbox Direct3D 12, XMem
graphics memory), apart from `crates/nativ64`, which stays host APIs only. It has no Vulkan
dependency yet.

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
  `XMemIsToolsMemAvailable`.
- `xcrdapi`: DirectStorage (`DStorageGetFactory`; factory, file, queue, status array as COM
  objects). Uncompressed memory/file to memory requests, run synchronously at `Submit`; errors
  in the queue's error record and event. TODO: zlib/BCPack, page and GPU destinations, fence
  signalling (recorded only). Details in the module header.

## Plan

1. Finish `xmem` (physical pages, working set) and `d3d12_xs` up to device creation.
2. Xbox D3D12 objects as COM vtables in Rust (device, queues, command lists, descriptor heaps).
3. Translate the console's shader binaries to SPIR-V and run the commands on Vulkan, once the
   object model above is exercised by the title.
