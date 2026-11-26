# Audio: `AcpHal.dll`, `xapu.dll` and `chad.dll`

Code: `crates/nativ64/src/acphal.rs` and `xapu.rs`. Both are console DLLs, not API sets.

## Role

The console has an audio processor (ACP, with the SHAPE mixer and XMA decoder) and a separate
decoder queue (xapu, Opus). A title's audio engine talks to both at startup and then polls them.
The goal for now is that the engine starts and never waits forever on hardware that is absent.

## Facts

Two reference projects document these interfaces; they agree on most of it. Facts only, no code
shared. "VI" and "XCL" below name them.

- Imports used by the title: AcpHal `AcpHalAllocateShapeContexts`, `AcpHalCreate`,
  `AcpHalReleaseShapeContexts`, `ApuAlloc`, `ApuCreateHeap`, `ApuFree`,
  `ApuIsVirtualAddressValid`, `ApuMapApuAddress`, `ApuMapVirtualAddress`; xapu `XApuConnect`,
  `XApuDequeueResult`, `XApuDisconnect`, `XApuEnqueueCommand`, `XApuGetResultCount`.
- APU heap: host memory with 32-bit device addresses. `ApuCreateHeap(a, b)` (both: first size
  non-zero); `ApuAlloc(&va, &apu, size, align, flags)` returns zeroed memory; `ApuFree(va)`;
  `ApuMapApuAddress(apu) -> va`. Observed allocations: 0x5780 (align 0x100), 0x222E00 (0x800,
  flags 2), 0x1913AFA0 (0x800, flags 1).
- SHAPE contexts: a request of counts, then host pointers from +24 (both references), then (VI)
  device addresses from +72. Kinds: rate conversion, EQ/compression, filter/volume, DMA, XMA, PCM.
- HAL object from `AcpHalCreate(&hal)`: no IUnknown, vtable = Connect(commands, messages),
  Disconnect, SubmitCommand(type, id, frame, data, last), PopMessage(&msg) -> bool,
  GetNumMessages, Release. Messages: 2 audio frame (128 samples at 48 kHz), 0x10 command done
  (carries the id), 0x80 disconnected. One command (type 2, id 0xF0F0F0F0) follows `Connect`.
- xapu: `XApuConnect(&input{process, streams, commands_per_stream, data_bytes, frames_per_output,
  options}, &output{data, ready}, &handle)`; options bit 8 creates the result event. Commands start
  with a 24-byte id (kind, stream, sequence, options, context u64); kinds 1 Activate (channels at
  +24), 2 Deactivate, 3 Process (ramp, pitch, first_frame, frames, input bytes/ptr, output
  bytes/ptr), 4 Notify, 5 Reset. Results are 80 bytes (id, state, state_data, time, error, frames,
  output bytes/ptr, pitch, input bytes/ptr); state bit 0 output ready, bit 1 more input wanted.
  Option bit 1 on a command signals the event. Error codes: 0x8AC81000 queue full, 0x8AC81001 no
  result yet, 0x8AC81002 invalid command, 0x8AC81003 invalid stream.

## Where the references disagree, and the choice

- Message layout: VI is 28 bytes (kind, dropped, usec, then a 16-byte payload: command, id, frame);
  XCL is 24 (kind, reserved, value u64, id u64). Both put the id at +16. Chosen: 24 bytes, VI's
  fields minus the frame, so a title with the smaller buffer is never overrun.
- Kind 2: XCL calls it an audio frame, VI a flowgraph completion. Chosen: audio frame (task and
  XCL's observation). VI's message mask (command 2 registers kinds) is ignored: all are delivered.
- `ApuMapVirtualAddress`: XCL `(&apu, va, size) -> HRESULT`, VI `(ptr) -> u32`. Chosen XCL, which
  says it was observed on retail titles; the VI form cannot report errors.
- `ApuFree`: void in VI, HRESULT in XCL. Chosen HRESULT (a void caller ignores it).
- `ApuIsVirtualAddressValid(va, x)`: XCL always true, VI checks the heap and treats x as alignment.
  Chosen: true if a region holds the address, x ignored.
- `ApuCreateHeap`: VI fails a second call (0x8ACA0001); XCL always succeeds. Chosen: idempotent,
  because a failure made one title drop all its audio and nothing shows a retail check.
- SHAPE structure: five counts and a reserved word (XCL) or six counts (VI, one more kind and the
  device addresses). Chosen: VI's, but the sixth pointer and the addresses are written only if the
  sixth count is non-zero. A second request replaces the first; release without one succeeds.
- `SubmitCommand`'s last argument: VI a device address of a notification word (set to 1), XCL a
  size. Chosen: store 1 only when it falls in a region (regions start at 256 MB, sizes do not).
- `XApuGetResultCount` and `XApuEnqueueCommand` `total`: counts so far (VI) or results waiting
  (XCL). Chosen: VI's (commands queued, results dequeued); the return value is results waiting.
- Process on an inactive stream: VI reports an invalid-stream error in the result, XCL answers
  silence. Chosen VI's.

## Design

- Heap: a map from device address to region (host pointer, size, owned or mapped title memory);
  first fit from 0x10000000 in the 32-bit space; owned blocks come from zeroed host allocation.
- HAL channel: every command completes at once; with an id it queues a "done" message, bounded by
  the queue size given to `Connect` (drops are counted in the next message). Audio frames are not
  queued: when the title polls, one is due if 2.667 ms have passed since the last. A title that
  stopped polling gets at most about 9 frames back. No thread.
- xapu: commands complete when queued and results come in order; each stream's queue is bounded
  by `commands_per_stream`. The result event is a handle-table event.

## Shortcuts

- No SHAPE mixer: contexts are zeroed memory nothing reads. Commands are not interpreted.
- No XMA or Opus decoding: a Process command writes silence (frames from the command, else the
  connection's, else 960; limited by the output size) and reports it ready.
- No sound output.

## Next chunks

- XMA and Opus decoding into the title's buffers.
- Output through the host (see "`mmdevapi.rs`" below for the null endpoint that stands in).

## `chad.dll` (the processor's DSP)

Imports used by the title: `XDspConnectWithMaximumStreamLimit`, `XDspActivate`,
`XDspSubmitCommand`, `XDspDeactivate`, `XDspDisconnect`. Both references agree: the DSP gets no
commands in observed runs, and a retail title stops at a breakpoint when connecting fails.
`XDspConnectWithMaximumStreamLimit(a, b, c, max streams, flags, &handle)` returns a handle; the
other four take the handle first. Here (`chad.rs`) connect succeeds with a heap-allocated handle
(null `&handle` is refused), the other calls accept a live handle and do nothing, and a null,
unknown or already disconnected handle gets `E_INVALIDARG`. Shortcut: the arguments after the
handle are not read; nothing is run.

## `mmdevapi.rs` (the audio endpoint, over COM)

The title's audio engine calls `CoCreateInstance(MMDeviceEnumerator {BCDE0395-E52F-467C-8E3D-
C4579291692E})`; a refusal left its mix queue unallocated and later wedged the audio thread, which
stopped the command recycler and ended in an assert after ~512 frames. `CoCreateInstance` now
returns a null endpoint (`mmdevapi.rs`): nothing is played, but every call answers like a working
device. All objects share one layout (vtable, count, kind) and one `QueryInterface`; a slot not
implemented is a named trap (`unimplemented IAudioClient slot N (+0xOFF)`, then abort).

- `IMMDeviceEnumerator`: `EnumAudioEndpoints` (collection of one), `GetDefaultAudioEndpoint`
  (render and capture flows, roles 0-2), `Register`/`UnregisterEndpointNotificationCallback`
  (S_OK, the callback is never stored). `GetDevice` traps.
- `IMMDevice`: `Activate` (`IAudioClient`, `ISpatialAudioClient`; other IIDs `E_NOINTERFACE`,
  `*out` NULL), `OpenPropertyStore`, `GetId` (CoTaskMem string), `GetState` = ACTIVE.
- `IPropertyStore::GetValue`: the one key the title reads is `{1DA5D803-D492-4EDD-8C23-
  E0C0FFEE7F0E}` pid 0 = `PKEY_AudioEndpoint_FormFactor` (read from its key table). It compares the
  value with 3 (Headphones) and 5 (Headset) to choose a downmix; we answer VT_UI4 1 (Speakers).
  Other keys: VT_EMPTY, S_OK.
- `IAudioClient`: only `GetMixFormat` (float32, 48 kHz, stereo, mask 3, as WAVEFORMATEXTENSIBLE).
- `ISpatialAudioClient`: static position (guessed unit vectors per speaker bit), static mask 3,
  16 dynamic objects, format enumerator (one format, the mix format), `GetMaxFrameCount` 480,
  `IsAudioObjectFormatSupported` (float32 48 kHz only, else `AUDCLNT_E_UNSUPPORTED_FORMAT`),
  `ActivateSpatialAudioStream` (the params PROPVARIANT is a VT_BLOB whose block gives max objects
  at +16 and the event handle at +24; IIDs of the render stream and its base).
- Render stream: `Start` spawns a clock thread that sets the stream's event every 10 ms until
  `Stop` or the last release; `BeginUpdatingAudioObjects` answers (max objects, 480 frames);
  `ActivateSpatialAudioObject` returns an object whose `GetBuffer` is a 480-frame scratch buffer
  that is never read. Slot orders are from the public headers as remembered; unverified against
  a real device.
- Shortcuts: no real clock accuracy (a plain sleep), no object count limit, no call-order errors
  (Begin without End, buffer after End), `IAudioClient` beyond `GetMixFormat` untested.
