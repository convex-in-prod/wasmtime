# Downstream metering changes

This source starts at upstream revision
`5afd63588b1f5520391c6a37fc38dfd3ccd607c2`. It owns the shared compilation and
runtime changes below so AOT producers and runtime consumers can pin the same
implementation without maintaining separate source copies.

- Fuel and epoch slow handlers preserve live registers. Stack-map construction
  supports the values that remain live across these calls.
- Exception catch entry reloads authoritative fuel, including charges incurred
  by callees before unwinding.
- `Config::fuel_epoch_poll_interval` optionally schedules internal epoch checks
  at bounded fuel refills. Native and escaping entries, startup, successful host
  returns, and rooted exception continuations retain checks. Deadline callbacks,
  asynchronous yields, interruptions, and exact fuel accounting remain active.

The interval is an engine setting inherited by every store. Zero preserves
ordinary instrumentation. Native artifacts use metadata version six, or seven
with an explicit nonzero polling interval. These versions include the upstream
tunables and builtin layout and the downstream deferred table-initialization
contract. Older native artifacts are rejected before module decoding even when
module version-string validation is disabled. Producer and consumer must agree
on the interval and native ABI; portable Wasm modules do not require a specific
Wasmtime revision. Source-keyed build caches still invalidate changed compiler
code even when native compatibility happens to be unchanged.

Polling supports core modules with default operator costs, fuel consumption, and
epoch interruption. Components have separate host-call trampolines and are not
supported by this mode. Stack switching, shared-memory threads, guest debugging,
recording, component concurrency, and separately configured asynchronous fuel
yields are also rejected. An interval bounds fuel slices, not wall time: native
host work and individual guest operations have varying duration, and a straight
line block may overshoot the active slice before its next checkpoint.

`tests/all/metering_polling.rs` covers metering values and accounting, exceptions,
entry and continuation boundaries, interruption and yield behavior, reference
rooting through collection, configuration rejection, and artifact identities.
Run it with both null and copying collector configurations. Native compatibility
checks reject obsolete metadata versions, mismatched intervals, and tampered
polling metadata before deserialization. Portable modules remain usable by other
compatible WebAssembly runtimes without these native extensions.
