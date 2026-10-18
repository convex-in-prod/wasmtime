//! Unix-specific extension for the `wasmtime` crate.
//!
//! This module is only available on Unix targets with the `std` feature, for
//! example Linux and macOS. Individual methods require native signal support.
//! The import path for this module is `wasmtime::unix::...`, which is intended
//! to emphasize that it is platform-specific.
//!
//! The traits contained in this module are intended to extend various types
//! throughout the `wasmtime` crate with extra functionality that's only
//! available on Unix.

#[cfg(has_native_signals)]
use crate::AsContextMut;
use crate::Store;
#[cfg(has_native_signals)]
use crate::prelude::*;

/// Extensions for the [`Store`] type only available on Unix.
pub trait StoreExt {
    // TODO: needs more docs?
    /// The signal handler must be
    /// [async-signal-safe](http://man7.org/linux/man-pages/man7/signal-safety.7.html).
    #[cfg(has_native_signals)]
    unsafe fn set_signal_handler<H>(&mut self, handler: H)
    where
        H: 'static
            + Fn(libc::c_int, *const libc::siginfo_t, *const libc::c_void) -> bool
            + Send
            + Sync;

    /// Installs a signal handler which can reject an embedder-managed memory
    /// fault with a WebAssembly trap. `Ok(true)` resumes the faulting instruction;
    /// `Ok(false)` delegates to the ordinary trap handler. `Err(trap)` unwinds
    /// only when the faulting PC belongs to code registered in this Store.
    /// The trap is returned through the innermost active call into the Store.
    /// To keep rejection allocation-free and lock-free in the signal handler,
    /// these traps do not capture a Wasm backtrace or coredump. Protected-page
    /// rejection is maintained on Linux x86-64 with native execution and
    /// signals-based traps enabled; Pulley and macOS Mach exceptions do not use
    /// this signal rejection interface.
    ///
    /// # Errors
    ///
    /// Returns an allocation error if the Store's code ranges cannot be prepared
    /// for signal-safe lookup. The existing handler remains installed on error.
    ///
    /// # Safety
    ///
    /// The handler must be async-signal-safe and must only reject faults caused
    /// by the embedder's deliberate memory protection. It must not reenter the
    /// Store, invalidate its memory, or access mutably borrowed Store data.
    /// Native host frames cannot be unwound this way: host and bulk-memory
    /// writes need a writable-range check before they write. The handler must
    /// not allocate, lock, or panic.
    #[cfg(has_native_signals)]
    unsafe fn set_signal_handler_with_trap<H>(&mut self, handler: H) -> crate::Result<()>
    where
        H: 'static
            + Fn(
                libc::c_int,
                *const libc::siginfo_t,
                *const libc::c_void,
            ) -> core::result::Result<bool, crate::Trap>
            + Send
            + Sync;

    /// Checks destination ranges before host APIs or bulk-memory libcalls write.
    /// Returning false rejects the write before its contents are changed.
    /// Scalar guest stores still require a signal handler when memory is protected.
    /// This hook covers the safe mutable `Memory` APIs and native bulk-memory
    /// libcalls. It is not a memory-growth, initialization, reset, or component
    /// canonical-ABI write hook.
    ///
    /// # Safety
    ///
    /// The callback receives a valid destination address and length. On success,
    /// that range must remain writable for the entire operation, including the
    /// lifetime of any mutable slice returned by `Memory::data_mut_range`,
    /// `Memory::data_mut`, or `Memory::data_and_store_mut`. Returning from this
    /// callback, or from an API returning a slice, does not end that permission.
    /// Protection may only be restored after all such borrows have ended.
    ///
    /// Raw pointer writes must obtain the same permission separately. The hook
    /// must not reenter the Store, access its borrowed data, grow or relocate
    /// memory, or invalidate any outstanding borrow. A hook used to enforce a
    /// copy budget must cover every managed memory in this Store. Empty ranges
    /// do not invoke it. Bulk libcalls may also operate on tables and GC arrays;
    /// ranges outside the managed linear memories must be accepted unchanged.
    ///
    /// The embedder must keep other native writers, including memory allocation,
    /// growth/relocation and component canonical-ABI lowering, away from protected
    /// destinations or obtain permission for their full destination first. Shared
    /// memory requires coordination with every writer and Store using it; this
    /// per-Store hook does not provide that coordination.
    #[cfg(has_native_signals)]
    unsafe fn set_memory_write_handler<H>(&mut self, handler: H)
    where
        H: 'static + Fn(usize, usize) -> bool + Send + Sync;

    /// Checks unshared linear-memory growth before exposing its new bytes.
    /// Receives the current base address, current byte length and requested byte
    /// length after ordinary resource and type limits have accepted the growth.
    /// Returning false traps with [`crate::Trap::AllocationTooLarge`]; guest code
    /// does not receive a recoverable `memory.grow` failure. Host growth returns
    /// that error normally.
    ///
    /// This hook covers host and guest growth of unshared linear memories. It
    /// does not cover initial allocation, shared memory or the internal GC heap.
    /// Lengths are logical byte lengths, not rounded to host page boundaries.
    /// Zero growth does not invoke it. Acceptance reserves permission; an OS or
    /// allocator error may still prevent growth. Such failures retain any charge
    /// made by this hook until the embedder reconciles or disposes the memory.
    ///
    /// # Safety
    ///
    /// The handler must not reenter the Store, access borrowed Store data, or
    /// access the requested tail, which is not yet accessible. When used with
    /// page protection, the embedder must guarantee a stable memory reservation:
    /// this callback does not make relocating or shared allocations safe. All
    /// other native writers still require the write hook or separate permission.
    #[cfg(has_native_signals)]
    unsafe fn set_memory_grow_handler<H>(&mut self, handler: H)
    where
        H: 'static + Fn(usize, usize, usize) -> bool + Send + Sync;
}

impl<T> StoreExt for Store<T> {
    #[cfg(has_native_signals)]
    unsafe fn set_signal_handler<H>(&mut self, handler: H)
    where
        H: 'static
            + Fn(libc::c_int, *const libc::siginfo_t, *const libc::c_void) -> bool
            + Send
            + Sync,
    {
        self.as_context_mut()
            .0
            .set_signal_handler(Some(Box::new(move |signal, info, context| {
                Ok(handler(signal, info, context))
            })));
    }

    #[cfg(has_native_signals)]
    unsafe fn set_signal_handler_with_trap<H>(&mut self, handler: H) -> crate::Result<()>
    where
        H: 'static
            + Fn(
                libc::c_int,
                *const libc::siginfo_t,
                *const libc::c_void,
            ) -> core::result::Result<bool, crate::Trap>
            + Send
            + Sync,
    {
        self.as_context_mut()
            .0
            .set_signal_handler_with_trap(Box::new(handler))
    }

    #[cfg(has_native_signals)]
    unsafe fn set_memory_write_handler<H>(&mut self, handler: H)
    where
        H: 'static + Fn(usize, usize) -> bool + Send + Sync,
    {
        self.as_context_mut()
            .0
            .set_memory_write_handler(Box::new(handler));
    }

    #[cfg(has_native_signals)]
    unsafe fn set_memory_grow_handler<H>(&mut self, handler: H)
    where
        H: 'static + Fn(usize, usize, usize) -> bool + Send + Sync,
    {
        self.as_context_mut().0.memory_grow_handler = Some(alloc::sync::Arc::new(handler));
    }
}
