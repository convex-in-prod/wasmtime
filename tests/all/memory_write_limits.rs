#![cfg(all(target_os = "linux", target_arch = "x86_64", not(miri)))]

// Exercise real protected-page writes: a missing trap or preflight boundary
// otherwise terminates the process instead of rejecting one guest execution.
use object::{Object, ObjectSection};
use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, Trap, unix::StoreExt};

struct WriteBudget {
    base: usize,
    length: usize,
    pages: AtomicU32,
    limit: AtomicU32,
    preflights: AtomicUsize,
    signals: AtomicUsize,
}
impl WriteBudget {
    fn permit(&self, address: usize, length: usize) -> bool {
        if length == 0 {
            return true;
        }
        let Some(end) = address.checked_add(length) else {
            return false;
        };
        if end <= self.base || address >= self.base + self.length {
            return true;
        }
        let Some(offset) = address.checked_sub(self.base) else {
            return false;
        };
        let Some(end) = offset.checked_add(length).filter(|end| *end <= self.length) else {
            return false;
        };
        let first = offset / 4096;
        let last = (end - 1) / 4096;
        let mask = ((u32::MAX >> (31 - (last - first))) << first) as u32;
        let mut previous = self.pages.load(Ordering::Acquire);
        loop {
            let next = previous | mask;
            if next.count_ones() > self.limit.load(Ordering::Relaxed) {
                return false;
            }
            match self.pages.compare_exchange_weak(
                previous,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => previous = actual,
            }
        }
        unsafe {
            libc::mprotect(
                (self.base + first * 4096) as *mut _,
                (last - first + 1) * 4096,
                libc::PROT_READ | libc::PROT_WRITE,
            ) == 0
        }
    }
}
struct DropWitness(Arc<AtomicUsize>);
impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn protected_write_denial() -> wasmtime::Result<()> {
    #[cfg(feature = "cranelift")]
    check(wasmtime::Strategy::Cranelift)?;
    #[cfg(feature = "winch")]
    check(wasmtime::Strategy::Winch)?;
    Ok(())
}

#[test]
fn linear_growth_preflight() -> wasmtime::Result<()> {
    #[cfg(feature = "cranelift")]
    check_growth(wasmtime::Strategy::Cranelift)?;
    #[cfg(feature = "winch")]
    check_growth(wasmtime::Strategy::Winch)?;
    Ok(())
}

pub(crate) fn check_growth(strategy: wasmtime::Strategy) -> wasmtime::Result<()> {
    let mut config = Config::new();
    config.strategy(strategy).memory_reservation(1 << 32);
    let engine = Engine::new(&config)?;
    let module = Module::new(
        &engine,
        r#"(module
            (memory (export "memory") 1 3)
            (data (i32.const 0) "prepared")
            (func (export "grow") (param i32) (result i32)
                local.get 0 memory.grow)
            (func (export "grow_then_write")
                i32.const 1 memory.grow drop
                i32.const 0 i32.const 42 i32.store8))"#,
    )?;
    let mut store = Store::new(&engine, wasmtime::StoreLimitsBuilder::new().build());
    store.limiter(|limits| limits);
    let base = Arc::new(AtomicUsize::new(0));
    let accepted_end = Arc::new(AtomicUsize::new(2 * 65536));
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_base = base.clone();
    let handler_end = accepted_end.clone();
    let handler_calls = calls.clone();
    unsafe {
        store.set_memory_grow_handler(move |address, current, desired| {
            assert_eq!(address, handler_base.load(Ordering::Relaxed));
            assert!(current < desired);
            handler_calls.fetch_add(1, Ordering::Relaxed);
            desired <= handler_end.load(Ordering::Relaxed)
        });
    }
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    base.store(memory.data_ptr(&store) as usize, Ordering::Relaxed);
    let grow = instance.get_typed_func::<i32, i32>(&mut store, "grow")?;
    let grow_then_write = instance.get_typed_func::<(), ()>(&mut store, "grow_then_write")?;
    assert_eq!(grow.call(&mut store, 0)?, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(memory.grow(&mut store, 1)?, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        memory.data_ptr(&store) as usize,
        base.load(Ordering::Relaxed)
    );
    assert!(memory.data(&store)[65536..].iter().all(|byte| *byte == 0));

    // Ordinary limiter refusal retains its recoverable Wasm semantics and
    // cannot consume the separate private-exposure allowance.
    *store.data_mut() = wasmtime::StoreLimitsBuilder::new()
        .memory_size(2 * 65536)
        .build();
    assert_eq!(grow.call(&mut store, 1)?, -1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    *store.data_mut() = wasmtime::StoreLimitsBuilder::new().build();

    // The embedder's refusal must trap before subsequent guest writes even
    // without a resource limiter that itself converts failures into traps.
    let error = grow_then_write.call(&mut store, ()).unwrap_err();
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::AllocationTooLarge)
    );
    assert_eq!(memory.data_size(&store), 2 * 65536);
    assert_eq!(&memory.data(&store)[..8], b"prepared");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    accepted_end.store(3 * 65536, Ordering::Relaxed);
    assert_eq!(grow.call(&mut store, 1)?, 2);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(grow.call(&mut store, 1)?, -1);
    assert!(memory.grow(&mut store, 1).is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 3);

    // Equal capacities do not identify an allocation. Imports must reach the
    // owning memory and the same hook, including when no limiter is installed.
    let imports = Module::new(
        &engine,
        r#"(module
            (import "" "memory" (memory 1 3))
            (func (export "grow") (param i32) (result i32)
                local.get 0 memory.grow))"#,
    )?;
    let mut store = Store::new(&engine, ());
    let allowed = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, Some(3))?)?;
    let denied = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, Some(3))?)?;
    let allowed_base = allowed.data_ptr(&store) as usize;
    let denied_base = denied.data_ptr(&store) as usize;
    assert_ne!(allowed_base, denied_base);
    unsafe {
        store.set_memory_grow_handler(move |address, current, desired| {
            assert!(current < desired);
            if address == allowed_base {
                true
            } else {
                assert_eq!(address, denied_base);
                false
            }
        });
    }
    for (memory, should_grow) in [(allowed, true), (denied, false)] {
        let instance = wasmtime::Instance::new(&mut store, &imports, &[memory.into()])?;
        let result = instance
            .get_typed_func::<i32, i32>(&mut store, "grow")?
            .call(&mut store, 1);
        if should_grow {
            assert_eq!(result?, 1);
            assert_eq!(memory.size(&store), 2);
        } else {
            assert_eq!(
                result.unwrap_err().downcast_ref::<Trap>(),
                Some(&Trap::AllocationTooLarge)
            );
            assert_eq!(memory.size(&store), 1);
        }
    }
    assert_eq!(allowed.grow(&mut store, 1)?, 2);
    assert_eq!(
        denied
            .grow(&mut store, 1)
            .unwrap_err()
            .downcast_ref::<Trap>(),
        Some(&Trap::AllocationTooLarge)
    );
    Ok(())
}

pub(crate) fn check(strategy: wasmtime::Strategy) -> wasmtime::Result<()> {
    let mut config = Config::new();
    config.memory_init_cow(false).strategy(strategy);
    config.module_version(wasmtime::ModuleVersionStrategy::None)?;
    let engine = Engine::new(&config)?;
    let module = Module::new(
        &engine,
        r#"(module
      (import "host" "write" (func $host_write (param i32)))
      (import "host" "nested" (func $nested))
      (import "host" "register" (func $register))
      (type $empty (func))
      (table (export "table") 1 funcref)
      (memory (export "memory") 2 3)
      (data $bytes "abcdefgh")
      (func (export "healthy") (result i32) i32.const 7)
      (func (export "store") (param i32) local.get 0 i32.const 42 i32.store)
      ;; This constant address is within the initial memory size. An optimizing
      ;; compiler need not attach an out-of-bounds trap to this store.
      (func (export "fixed_store") i32.const 4096 i32.const 42 i32.store)
      (func (export "nested") call $nested)
      (func (export "register_then_store")
        call $register i32.const 0 call_indirect (type $empty))
      (func (export "fill") (param i32 i32 i32) local.get 0 local.get 1 local.get 2 memory.fill)
      (func (export "copy") (param i32 i32 i32) local.get 0 local.get 1 local.get 2 memory.copy)
      (func (export "init") (param i32 i32 i32) local.get 0 local.get 1 local.get 2 memory.init $bytes)
      (func (export "host_write") (param i32) local.get 0 call $host_write)
    )"#,
    )?;
    let serialized = module.serialize()?;
    let object = object::File::parse(serialized.as_slice())?;
    let offset = object
        .section_by_name(".wasmtime.engine")
        .unwrap()
        .file_range()
        .unwrap()
        .0 as usize;
    assert_eq!(serialized[offset], 6);
    for old_version in [0, 1, 2, 3, 4, 5] {
        let mut old = serialized.clone();
        old[offset] = old_version;
        let error = unsafe { Module::deserialize(&engine, old) }
            .err()
            .expect("old native metadata and builtin ABI must be rejected");
        assert!(
            error
                .to_string()
                .contains("mismatched version in engine section"),
            "{error}"
        );
    }
    let module = unsafe { Module::deserialize(&engine, serialized)? };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut store = Store::new(&engine, drops.clone());
    let mut linker = Linker::new(&engine);
    linker.func_wrap(
        "host",
        "write",
        |mut caller: Caller<'_, Arc<AtomicUsize>>, offset: i32| -> wasmtime::Result<()> {
            let _witness = DropWitness(caller.data().clone());
            let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
            memory.write(&mut caller, offset as usize, &[19])?;
            Ok(())
        },
    )?;
    linker.func_wrap(
        "host",
        "nested",
        |mut caller: Caller<'_, Arc<AtomicUsize>>| -> wasmtime::Result<()> {
            let _witness = DropWitness(caller.data().clone());
            let put = caller
                .get_export("fixed_store")
                .unwrap()
                .into_func()
                .unwrap()
                .typed::<(), ()>(&caller)?;
            let error = put.call(&mut caller, ()).unwrap_err();
            assert_eq!(
                error.downcast_ref::<Trap>(),
                Some(&Trap::AllocationTooLarge)
            );
            // Catching the inner trap must let the outer Wasm activation resume.
            Ok(())
        },
    )?;
    linker.func_wrap(
        "host",
        "register",
        |mut caller: Caller<'_, Arc<AtomicUsize>>| -> wasmtime::Result<()> {
            let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
            let table = caller.get_export("table").unwrap().into_table().unwrap();
            let module = Module::new(
                caller.engine(),
                r#"(module
                    (import "" "" (memory 2 3))
                    (func (export "put") i32.const 4096 i32.const 1 i32.store))"#,
            )?;
            let instance = wasmtime::Instance::new(&mut caller, &module, &[memory.into()])?;
            let put = instance.get_func(&mut caller, "put").unwrap();
            table.set(&mut caller, 0, wasmtime::Ref::Func(Some(put)))?;
            Ok(())
        },
    )?;
    let instance = linker.instantiate(&mut store, &module)?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    let base = memory.data_ptr(&store) as usize;
    let length = memory.data_size(&store);
    assert_eq!(length, 32 * 4096);
    let budget = Arc::new(WriteBudget {
        base,
        length,
        pages: AtomicU32::new(0),
        limit: AtomicU32::new(1),
        preflights: AtomicUsize::new(0),
        signals: AtomicUsize::new(0),
    });
    let signal = budget.clone();
    let writes = budget.clone();
    unsafe {
        store.set_signal_handler_with_trap(move |signum, info, context| {
            if signum != libc::SIGSEGV || (*info).si_code != 2 {
                return Ok(false);
            }
            let regs = &*(context as *const libc::ucontext_t);
            let address = (*info).si_addr() as usize;
            if regs.uc_mcontext.gregs[libc::REG_ERR as usize] & 2 == 0
                || address < signal.base
                || address >= signal.base + signal.length
            {
                return Ok(false);
            }
            signal.signals.fetch_add(1, Ordering::Relaxed);
            if signal.permit(address, 1) {
                Ok(true)
            } else {
                Err(Trap::AllocationTooLarge)
            }
        })?;
        store.set_memory_write_handler(move |address, length| {
            writes.preflights.fetch_add(1, Ordering::Relaxed);
            writes.permit(address, length)
        });
        assert_eq!(libc::mprotect(base as *mut _, length, libc::PROT_READ), 0);
    }
    let put = instance.get_typed_func::<i32, ()>(&mut store, "store")?;
    put.call(&mut store, 0)?;
    assert_eq!(budget.signals.load(Ordering::Relaxed), 1);
    let error = put.call(&mut store, 4096).unwrap_err();
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::AllocationTooLarge)
    );
    assert_eq!(budget.pages.load(Ordering::Relaxed).count_ones(), 1);
    assert!(error.downcast_ref::<wasmtime::WasmBacktrace>().is_none());
    let error = instance
        .get_typed_func::<(), ()>(&mut store, "fixed_store")?
        .call(&mut store, ())
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::AllocationTooLarge)
    );
    instance
        .get_typed_func::<(), ()>(&mut store, "nested")?
        .call(&mut store, ())?;
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    // The active Store's signal-safe ranges must include a module registered by
    // a host callback, even when its function is called without a new host entry.
    let error = instance
        .get_typed_func::<(), ()>(&mut store, "register_then_store")?
        .call(&mut store, ())
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::AllocationTooLarge)
    );
    assert_eq!(
        instance
            .get_typed_func::<(), i32>(&mut store, "healthy")?
            .call(&mut store, ())?,
        7
    );
    let signals = budget.signals.load(Ordering::Relaxed);
    for (name, arguments) in [
        ("fill", (4096, 3, 4096)),
        ("copy", (4096, 0, 4096)),
        ("init", (4096, 0, 8)),
    ] {
        let before = budget.preflights.load(Ordering::Relaxed);
        let error = instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut store, name)?
            .call(&mut store, arguments)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Trap>(),
            Some(&Trap::AllocationTooLarge),
            "{name}: {error}"
        );
        assert_eq!(budget.preflights.load(Ordering::Relaxed), before + 1);
        assert_eq!(budget.signals.load(Ordering::Relaxed), signals);
    }
    assert!(
        memory
            .write(&mut store, 4096, &[1])
            .unwrap_err()
            .is_write_denied()
    );
    assert!(
        memory
            .data_mut_range(&mut store, 4096..4100)
            .unwrap_err()
            .is_write_denied()
    );
    let error = instance
        .get_typed_func::<i32, ()>(&mut store, "host_write")?
        .call(&mut store, 4096)
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<wasmtime::MemoryAccessError>()
            .unwrap()
            .is_write_denied()
    );
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    // The infallible whole-memory APIs must reject instead of handing out a
    // slice that would fault later in arbitrary host code.
    for with_store_data in [false, true] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if with_store_data {
                let _ = memory.data_and_store_mut(&mut store);
            } else {
                let _ = memory.data_mut(&mut store);
            }
        }));
        assert!(result.is_err());
    }
    let before = budget.preflights.load(Ordering::Relaxed);
    assert!(
        !memory
            .write(&mut store, usize::MAX, &[1])
            .unwrap_err()
            .is_write_denied()
    );
    assert!(
        memory
            .data_mut_range(&mut store, length..length)?
            .is_empty()
    );
    assert!(
        !memory
            .data_mut_range(&mut store, 5..4)
            .unwrap_err()
            .is_write_denied()
    );
    for (name, source_end) in [("fill", 0), ("copy", length), ("init", 8)] {
        let operation = instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, name)?;
        operation.call(&mut store, (length as i32, source_end as i32, 0))?;
        let error = operation
            .call(&mut store, (length as i32 + 1, source_end as i32, 0))
            .unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::MemoryOutOfBounds));
    }
    // A bad source has priority over a denied (but in-bounds) destination.
    for (name, source_end) in [("copy", length), ("init", 8)] {
        let error = instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut store, name)?
            .call(&mut store, (4096, source_end as i32, 1))
            .unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::MemoryOutOfBounds));
    }
    assert_eq!(budget.preflights.load(Ordering::Relaxed), before);
    assert_eq!(&memory.data(&store)[4096..8192], &[0; 4096]);
    assert_eq!(&memory.data(&store)[..4], &42i32.to_le_bytes());
    budget.limit.store(2, Ordering::Relaxed);
    instance
        .get_typed_func::<(i32, i32, i32), ()>(&mut store, "fill")?
        .call(&mut store, (4096, 3, 4096))?;
    assert_eq!(&memory.data(&store)[4096..8192], &[3; 4096]);
    assert_eq!(budget.pages.load(Ordering::Relaxed).count_ones(), 2);
    memory
        .data_mut_range(&mut store, 4096..4098)?
        .copy_from_slice(&[6, 7]);
    assert_eq!(&memory.data(&store)[4096..4098], &[6, 7]);
    memory.write(&mut store, 0, &[1, 2, 3, 4, 5])?;
    instance
        .get_typed_func::<(i32, i32, i32), ()>(&mut store, "copy")?
        .call(&mut store, (1, 0, 4))?;
    assert_eq!(&memory.data(&store)[..5], &[1, 1, 2, 3, 4]);
    {
        store.call_hook(|_, event| {
            if matches!(event, wasmtime::CallHook::CallingHost) {
                wasmtime::bail!("bulk operations must not invoke reentrant call hooks");
            }
            Ok(())
        });
        for name in ["fill", "copy", "init"] {
            instance
                .get_typed_func::<(i32, i32, i32), ()>(&mut store, name)?
                .call(&mut store, (0, 0, 1))?;
        }
    }
    budget.limit.store(32, Ordering::Relaxed);
    memory.data_mut(&mut store)[length - 1] = 11;
    {
        let (bytes, store_drops) = memory.data_and_store_mut(&mut store);
        assert!(Arc::ptr_eq(store_drops, &drops));
        assert_eq!(bytes[length - 1], 11);
        bytes[length - 1] = 12;
    }
    assert_eq!(memory.data(&store)[length - 1], 12);
    assert_eq!(budget.pages.load(Ordering::Relaxed).count_ones(), 32);
    // A native callback panic must return through its Rust frames before the
    // trampoline unwinds Wasm and resumes the panic at the host call boundary.
    let callback_drops = drops.clone();
    unsafe {
        store.set_memory_write_handler(move |_, _| {
            let _witness = DropWitness(callback_drops.clone());
            panic!("write callback panic");
        });
    }
    let fill = instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "fill")?;
    let before = memory.data(&store)[0];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fill.call(&mut store, (0, 99, 1))
    }));
    assert!(result.is_err());
    assert_eq!(drops.load(Ordering::Relaxed), 3);
    assert_eq!(memory.data(&store)[0], before);
    assert_eq!(
        instance
            .get_typed_func::<(), i32>(&mut store, "healthy")?
            .call(&mut store, ())?,
        7
    );
    unsafe {
        assert_eq!(
            libc::mprotect(base as *mut _, length, libc::PROT_READ | libc::PROT_WRITE),
            0
        );
    }
    Ok(())
}
