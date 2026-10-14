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

The interval is an engine setting inherited by every store. Zero preserves the
ordinary instrumentation, version-zero engine metadata bytes, and engine hash
input. Nonzero intervals use version-one metadata with an explicit interval;
an older runtime rejects that format even with module version-string validation
disabled. A consumer must use the same interval as its producer. Source-keyed
build caches must still invalidate changed compilation code; artifact
compatibility does not establish that an older artifact includes a newer fix.

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
Run it with both null and copying collector configurations. Cross-version
acceptance also requires executing legacy artifacts in the new zero-interval
runtime, executing new zero-interval artifacts in the legacy runtime, comparing
their engine metadata and hash inputs, and checking rejection of polling
artifacts by the legacy runtime.
