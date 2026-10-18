#![cfg(all(not(miri), feature = "cranelift", feature = "gc"))]

use std::{
    borrow::Cow,
    future::Future,
    task::{Context, Poll, Waker},
};
use wasm_encoder::{
    BlockType, Catch, CodeSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction as I, Module as EncodedModule, TagKind, TagSection,
    TagType, TypeSection, ValType,
};
use wasmtime::{
    Config, Engine, Error, Inlining, Instance, Linker, Module, Store, Trap, UpdateDeadline, V128,
};

fn engine() -> Engine {
    let mut config = Config::new();
    #[cfg(feature = "gc-copying")]
    config.collector(wasmtime::Collector::Copying);
    #[cfg(all(feature = "gc-null", not(feature = "gc-copying")))]
    config.collector(wasmtime::Collector::Null);
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .compiler_inlining(Inlining::No)
        .wasm_exceptions(true);
    Engine::new(&config).unwrap()
}

fn finish<T>(future: impl Future<Output = T>) -> (T, usize) {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for polls in 1..10_000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return (value, polls);
        }
    }
    panic!("metered invocation did not finish");
}

fn register_module() -> Vec<u8> {
    let mut m = EncodedModule::new();
    let mut types = TypeSection::new();
    types.ty().function(
        [
            ValType::I32,
            ValType::F64,
            ValType::F64,
            ValType::I64,
            ValType::I64,
            ValType::V128,
        ],
        [ValType::I64, ValType::F64, ValType::V128],
    );
    m.section(&types);
    let mut functions = FunctionSection::new();
    functions.function(0);
    m.section(&functions);
    let mut exports = ExportSection::new();
    exports.export("mixed", ExportKind::Func, 0);
    m.section(&exports);
    let mut f = Function::new([]);
    for i in [
        I::Block(BlockType::Empty),
        I::Loop(BlockType::Empty),
        I::LocalGet(0),
        I::I32Eqz,
        I::BrIf(1),
        I::LocalGet(1),
        I::LocalGet(2),
        I::F64Add,
        I::LocalSet(1),
        I::LocalGet(3),
        I::LocalGet(4),
        I::I64Xor,
        I::LocalSet(3),
        I::LocalGet(5),
        I::I32Const(7),
        I::I8x16Splat,
        I::I8x16Add,
        I::LocalSet(5),
        I::LocalGet(0),
        I::I32Const(1),
        I::I32Sub,
        I::LocalSet(0),
        I::Br(0),
        I::End,
        I::End,
        I::LocalGet(3),
        I::LocalGet(1),
        I::LocalGet(5),
        I::End,
    ] {
        f.instruction(&i);
    }
    let mut code = CodeSection::new();
    code.function(&f);
    m.section(&code);
    m.finish()
}

fn exception_module() -> Vec<u8> {
    let mut m = EncodedModule::new();
    let mut t = TypeSection::new();
    t.ty().function([ValType::I64], []);
    t.ty().function([ValType::I64], [ValType::I64]);
    t.ty().function([], []);
    m.section(&t);
    let mut imports = ImportSection::new();
    imports.import("host", "tick", EntityType::Function(2));
    m.section(&imports);
    let mut f = FunctionSection::new();
    f.function(1).function(1);
    m.section(&f);
    let mut tags = TagSection::new();
    tags.tag(TagType {
        kind: TagKind::Exception,
        func_type_idx: 0,
    });
    m.section(&tags);
    let mut e = ExportSection::new();
    e.export("caught", ExportKind::Func, 1);
    m.section(&e);
    let mut c = CodeSection::new();
    let mut run = Function::new([]);
    for i in [
        I::Block(BlockType::Result(ValType::I64)),
        I::TryTable(
            BlockType::Result(ValType::I64),
            Cow::Owned(vec![Catch::One { tag: 0, label: 0 }]),
        ),
        I::Call(0),
        I::LocalGet(0),
        I::Call(2),
        I::End,
        I::End,
        I::End,
    ] {
        run.instruction(&i);
    }
    c.function(&run);
    let mut throwing = Function::new([]);
    for i in [I::LocalGet(0), I::Throw(0), I::End] {
        throwing.instruction(&i);
    }
    c.function(&throwing);
    m.section(&c);
    m.finish()
}

#[test]
fn live_values_across_fuel_refills_and_epoch_updates() {
    let engine = engine();
    let module = Module::new(&engine, register_module()).unwrap();
    let vector = 0x0102030405060708090a0b0c0d0e0f10_u128;
    let expected_vector =
        u128::from_le_bytes(vector.to_le_bytes().map(|lane| lane.wrapping_add(13 * 7)));
    for interval in [None, Some(1), Some(7), Some(31)] {
        for mode in [0, 1, 2, 3] {
            for fuel in [0, 1, 19, 127, 4096] {
                let mut store = Store::new(&engine, 0_u32);
                let instance = Instance::new(&mut store, &module, &[]).unwrap();
                store.set_fuel(fuel).unwrap();
                if let Some(interval) = interval {
                    store.fuel_async_yield_interval(Some(interval)).unwrap();
                }
                store.set_epoch_deadline(if mode == 0 { 1 } else { 0 });
                store.epoch_deadline_callback(move |mut context| {
                    *context.data_mut() += 1;
                    Ok(match mode {
                        1 => UpdateDeadline::Continue(0),
                        2 => UpdateDeadline::Yield(0),
                        _ => UpdateDeadline::Continue(1),
                    })
                });
                let run = instance
                    .get_typed_func::<(i32, f64, f64, i64, i64, V128), (i64, f64, V128)>(
                        &mut store, "mixed",
                    )
                    .unwrap();
                let (result, polls) = finish(run.call_async(
                    &mut store,
                    (
                        13,
                        1.25,
                        0.5,
                        0x1234567890,
                        0x1020304050,
                        V128::from(vector),
                    ),
                ));
                if fuel < 280 {
                    assert_eq!(
                        result.unwrap_err().downcast_ref::<Trap>(),
                        Some(&Trap::OutOfFuel)
                    );
                    assert_eq!(store.get_fuel().unwrap(), 0);
                    continue;
                }
                let (integer, float, vector) = result.unwrap();
                assert_eq!(integer, 0x1234567890 ^ 0x1020304050);
                assert_eq!(float, 1.25 + 13.0 * 0.5);
                assert_eq!(vector.as_u128(), expected_vector);
                assert_eq!(store.get_fuel().unwrap(), fuel - 280);
                assert_eq!(polls > 1, interval.is_some() || mode == 2);
                match mode {
                    0 => assert_eq!(*store.data(), 0),
                    3 => assert_eq!(*store.data(), 1),
                    _ => assert!(*store.data() > 1),
                }
            }
        }
    }
}

#[test]
fn metering_traps_do_not_become_wasm_exceptions() {
    let engine = engine();
    let module = Module::new(&engine, exception_module()).unwrap();
    let mut linker = Linker::new(&engine);
    let tick = engine.clone();
    linker
        .func_wrap("host", "tick", move || {
            tick.increment_epoch();
        })
        .unwrap();
    for mode in 0..3 {
        for fuel in [0, 1, 7, 32, 1000] {
            let mut store = Store::new(&engine, 0_u32);
            let instance = linker.instantiate(&mut store, &module).unwrap();
            store.set_fuel(fuel).unwrap();
            store.set_epoch_deadline(1);
            store.epoch_deadline_callback(move |mut context| {
                *context.data_mut() += 1;
                match mode {
                    0 => Ok(UpdateDeadline::Continue(1)),
                    1 => Ok(UpdateDeadline::Interrupt),
                    _ => Err(Error::msg("epoch callback stop")),
                }
            });
            let run = instance
                .get_typed_func::<i64, i64>(&mut store, "caught")
                .unwrap();
            match run.call(&mut store, 0x1234_5678) {
                Ok(value) => {
                    assert_eq!(mode, 0);
                    assert_eq!(value, 0x1234_5678);
                }
                Err(error) => match error.downcast_ref::<Trap>() {
                    Some(Trap::OutOfFuel) => assert!(fuel < 1000),
                    Some(Trap::Interrupt) => assert_eq!(mode, 1),
                    None => {
                        assert_eq!(mode, 2);
                        assert_eq!(error.root_cause().to_string(), "epoch callback stop");
                    }
                    trap => panic!("unexpected trap: {trap:?}"),
                },
            }
            assert!(!store.has_pending_exception());
            if fuel == 1000 {
                assert_eq!(*store.data(), 1);
            }
        }
    }
}

#[test]
fn cancelled_fuel_yield_leaves_store_usable() {
    let engine = engine();
    let module = Module::new(&engine, register_module()).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    store.set_fuel(1_000_000).unwrap();
    store.set_epoch_deadline(1);
    store.fuel_async_yield_interval(Some(1)).unwrap();
    let run = instance
        .get_typed_func::<(i32, f64, f64, i64, i64, V128), (i64, f64, V128)>(&mut store, "mixed")
        .unwrap();
    {
        let mut future =
            std::pin::pin!(run.call_async(&mut store, (1_000_000, 1.0, 2.0, 3, 4, V128::from(0))));
        let mut context = Context::from_waker(Waker::noop());
        for _ in 0..32 {
            assert!(future.as_mut().poll(&mut context).is_pending());
        }
    }
    assert!(store.get_fuel().unwrap() < 1_000_000);
    store.fuel_async_yield_interval(None).unwrap();
    store.set_fuel(1000).unwrap();
    // Once fuel yielding has been enabled, this Store requires async entrypoints
    // even after its interval has been disabled.
    let (result, polls) = finish(run.call_async(&mut store, (1, 1.0, 2.0, 3, 4, V128::from(0))));
    let (integer, float, vector) = result.unwrap();
    assert_eq!(integer, 7);
    assert_eq!(float, 3.0);
    assert_eq!(vector.as_u128(), u128::from_le_bytes([7; 16]));
    assert_eq!(polls, 1);
}

fn exception_fuel_module(iterations: i32, depth: u32, throws: bool, catch: Catch) -> Vec<u8> {
    let mut module = EncodedModule::new();
    let mut types = TypeSection::new();
    types.ty().function([], []);
    types.ty().function([], [ValType::I32]);
    module.section(&types);
    let mut functions = FunctionSection::new();
    for _ in 0..=depth {
        functions.function(0);
    }
    functions.function(1);
    module.section(&functions);
    let mut tags = TagSection::new();
    tags.tag(TagType {
        kind: TagKind::Exception,
        func_type_idx: 0,
    });
    module.section(&tags);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, depth + 1);
    module.section(&exports);
    let mut code = CodeSection::new();
    let mut leaf = Function::new([(1, ValType::I32)]);
    for op in [
        I::I32Const(iterations),
        I::LocalSet(0),
        I::Loop(BlockType::Empty),
        I::LocalGet(0),
        I::I32Const(1),
        I::I32Sub,
        I::LocalTee(0),
        I::BrIf(0),
        I::End,
    ] {
        leaf.instruction(&op);
    }
    if throws {
        leaf.instruction(&I::Throw(0));
    }
    leaf.instruction(&I::End);
    code.function(&leaf);
    let retains_ref = matches!(catch, Catch::OneRef { .. } | Catch::AllRef { .. });
    let block_type = if retains_ref {
        BlockType::Result(ValType::EXNREF)
    } else {
        BlockType::Empty
    };
    for index in 1..=depth + 1 {
        let mut caller = Function::new([]);
        caller
            .instruction(&I::Block(block_type))
            .instruction(&I::TryTable(block_type, Cow::Owned(vec![catch.clone()])))
            .instruction(&I::Call(index - 1));
        // Return directly on the normal path; only the caught path below
        // consumes the exception reference or rethrows it.
        if index == depth + 1 {
            caller.instruction(&I::I32Const(42));
        }
        caller
            .instruction(&I::Return)
            .instruction(&I::End)
            .instruction(&I::End);
        if index == depth + 1 {
            if retains_ref {
                caller.instruction(&I::Drop);
            }
            caller.instruction(&I::I32Const(42));
        } else {
            caller.instruction(&if retains_ref {
                I::ThrowRef
            } else {
                I::Throw(0)
            });
        }
        caller.instruction(&I::End);
        code.function(&caller);
    }
    module.section(&code);
    module.finish()
}

#[test]
fn caught_exceptions_retain_callee_fuel_across_refills() {
    let engine = engine();
    for iterations in [10, 1_000] {
        for depth in [0, 2] {
            for catch in [
                Catch::All { label: 0 },
                Catch::One { tag: 0, label: 0 },
                Catch::AllRef { label: 0 },
                Catch::OneRef { tag: 0, label: 0 },
            ] {
                for throws in [false, true] {
                    let module = Module::new(
                        &engine,
                        exception_fuel_module(iterations, depth, throws, catch.clone()),
                    )
                    .unwrap();
                    // Each loop iteration costs five units. The two initial
                    // local operations, entries, calls, try_tables, constants
                    // and optional throws supply the remaining fixed charge.
                    let expected = 5 * iterations as u64
                        + 7
                        + u64::from(throws)
                        + u64::from(depth) * if throws { 4 } else { 3 };
                    for interval in [None, Some(1), Some(100), Some(1_000), Some(100_000)] {
                        let mut store = Store::new(&engine, ());
                        store.set_fuel(1_000_000).unwrap();
                        store.set_epoch_deadline(1);
                        let instance = Instance::new(&mut store, &module, &[]).unwrap();
                        if interval.is_some() {
                            store.fuel_async_yield_interval(interval).unwrap();
                        }
                        let run = instance
                            .get_typed_func::<(), i32>(&mut store, "run")
                            .unwrap();
                        let (result, _) = finish(run.call_async(&mut store, ()));
                        assert_eq!(result.unwrap(), 42);
                        assert_eq!(
                            1_000_000 - store.get_fuel().unwrap(),
                            expected,
                            "iterations={iterations} depth={depth} throws={throws} interval={interval:?} catch={catch:?}"
                        );
                        assert!(!store.has_pending_exception());
                    }
                }
            }
        }
    }
}

#[test]
fn caught_exceptions_do_not_restore_exhausted_fuel() {
    let engine = engine();
    let module = Module::new(
        &engine,
        exception_fuel_module(100, 2, true, Catch::AllRef { label: 0 }),
    )
    .unwrap();
    for interval in [None, Some(1), Some(31)] {
        for fuel in [0, 1, 100, 503, 507, 512] {
            let mut store = Store::new(&engine, ());
            store.set_fuel(fuel).unwrap();
            store.set_epoch_deadline(1);
            let instance = Instance::new(&mut store, &module, &[]).unwrap();
            if interval.is_some() {
                store.fuel_async_yield_interval(interval).unwrap();
            }
            let run = instance
                .get_typed_func::<(), i32>(&mut store, "run")
                .unwrap();
            let (result, _) = finish(run.call_async(&mut store, ()));
            // Entry/loop checkpoints may leave the final straight-line charge
            // until the next call. Either result must leave the budget spent.
            if let Err(error) = result {
                assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            }
            assert_eq!(
                store.get_fuel().unwrap(),
                0,
                "fuel={fuel} interval={interval:?}"
            );
            let (result, _) = finish(run.call_async(&mut store, ()));
            assert_eq!(
                result.unwrap_err().downcast_ref::<Trap>(),
                Some(&Trap::OutOfFuel)
            );
            assert!(!store.has_pending_exception());
        }
    }
}

fn polling_config(interval: u64) -> Config {
    let mut config = Config::new();
    #[cfg(feature = "gc-copying")]
    config.collector(wasmtime::Collector::Copying);
    #[cfg(all(feature = "gc-null", not(feature = "gc-copying")))]
    config.collector(wasmtime::Collector::Null);
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_exceptions(true)
        .compiler_inlining(Inlining::No)
        .fuel_epoch_poll_interval(interval)
        .concurrency_support(false);
    #[cfg(feature = "threads")]
    config.wasm_threads(false);
    #[cfg(feature = "component-model")]
    config.wasm_component_model(false);
    config
}

fn polling_engine(interval: u64) -> Engine {
    Engine::new(&polling_config(interval)).unwrap()
}

fn host_return_module() -> Vec<u8> {
    let mut m = EncodedModule::new();
    let mut types = TypeSection::new();
    types.ty().function([], []);
    types.ty().function([], [ValType::I32]);
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import("host", "tick", EntityType::Function(0));
    m.section(&imports);
    let mut functions = FunctionSection::new();
    functions.function(1).function(1);
    m.section(&functions);
    let mut exports = ExportSection::new();
    exports
        .export("run", ExportKind::Func, 1)
        .export("reenter", ExportKind::Func, 2);
    m.section(&exports);
    let mut code = CodeSection::new();
    let mut run = Function::new([]);
    run.instruction(&I::Call(0))
        .instruction(&I::I32Const(42))
        .instruction(&I::End);
    code.function(&run);
    let mut reenter = Function::new([]);
    reenter.instruction(&I::I32Const(43)).instruction(&I::End);
    code.function(&reenter);
    m.section(&code);
    m.finish()
}

fn call_tree_module(depth: u32, start: bool) -> Vec<u8> {
    let mut m = EncodedModule::new();
    let mut types = TypeSection::new();
    types.ty().function([], []);
    m.section(&types);
    let mut functions = FunctionSection::new();
    for _ in 0..=depth {
        functions.function(0);
    }
    m.section(&functions);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, depth);
    m.section(&exports);
    if start {
        m.section(&wasm_encoder::StartSection {
            function_index: depth,
        });
    }
    let mut code = CodeSection::new();
    for index in 0..=depth {
        let mut f = Function::new([]);
        if index > 0 {
            f.instruction(&I::Call(index - 1))
                .instruction(&I::Call(index - 1));
        }
        f.instruction(&I::End);
        code.function(&f);
    }
    m.section(&code);
    m.finish()
}

#[test]
fn polling_contract_rejects_incompatible_artifacts_and_options() {
    let candidate = polling_engine(100);
    let bytes = candidate
        .precompile_module(&call_tree_module(1, false))
        .unwrap();
    for interval in [0, 101] {
        let engine = polling_engine(interval);
        let error = unsafe { Module::deserialize(&engine, &bytes) }.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fuel-scheduled epoch polling interval"),
            "{error:#}"
        );
    }
    let matching = polling_engine(100);
    unsafe { Module::deserialize(&matching, &bytes) }.unwrap();
    for (fuel, epochs) in [(false, true), (true, false), (false, false)] {
        let mut config = polling_config(100);
        config
            .consume_fuel(fuel)
            .epoch_interruption(epochs)
            .fuel_epoch_poll_interval(100);
        assert!(Engine::new(&config).is_err());
    }
    let mut config = polling_config(100);
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .fuel_epoch_poll_interval(100)
        .operator_cost(wasmtime::OperatorCost::new());
    assert!(Engine::new(&config).is_err());
    let mut store = Store::new(&candidate, ());
    store.set_fuel(10_000).unwrap();
    assert!(store.fuel_async_yield_interval(Some(10)).is_err());
    assert_eq!(store.get_fuel().unwrap(), 10_000);
}

#[test]
fn polling_artifact_identity_and_metadata_versions() {
    use object::{Object, ObjectSection};
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut identities = Vec::new();
    for interval in [0, 100, 101] {
        let engine = polling_engine(interval);
        let mut hash = DefaultHasher::new();
        engine.precompile_compatibility_hash().hash(&mut hash);
        identities.push(hash.finish());
        let mut artifact = engine
            .precompile_module(&call_tree_module(1, false))
            .unwrap();
        let object = object::File::parse(&*artifact).unwrap();
        let section = object.section_by_name(".wasmtime.engine").unwrap();
        let (offset, len) = section.file_range().unwrap();
        let offset = usize::try_from(offset).unwrap();
        let len = usize::try_from(len).unwrap();
        assert_eq!(artifact[offset], if interval == 0 { 2 } else { 3 });
        if interval != 0 {
            // These intervals fit in one postcard byte. A zero extension must
            // not turn an artifact without internal checks into a legacy one.
            assert_eq!(artifact[offset + len - 1], interval as u8);
            artifact[offset + len - 1] = 0;
            let mut config = polling_config(0);
            config
                .module_version(wasmtime::ModuleVersionStrategy::None)
                .unwrap();
            let consumer = Engine::new(&config).unwrap();
            let error = unsafe { Module::deserialize(&consumer, &artifact) }.unwrap_err();
            assert!(
                error.to_string().contains("interval must be nonzero"),
                "{error:#}"
            );
        }
    }
    assert_ne!(identities[0], identities[1]);
    assert_ne!(identities[0], identities[2]);
    assert_ne!(identities[1], identities[2]);

    // Explicitly disabling the mode must retain the ordinary engine identity.
    let mut config = Config::new();
    #[cfg(feature = "gc-copying")]
    config.collector(wasmtime::Collector::Copying);
    #[cfg(all(feature = "gc-null", not(feature = "gc-copying")))]
    config.collector(wasmtime::Collector::Null);
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_exceptions(true)
        .compiler_inlining(Inlining::No)
        .concurrency_support(false);
    #[cfg(feature = "threads")]
    config.wasm_threads(false);
    #[cfg(feature = "component-model")]
    config.wasm_component_model(false);
    let default = Engine::new(&config).unwrap();
    let mut hash = DefaultHasher::new();
    default.precompile_compatibility_hash().hash(&mut hash);
    assert_eq!(hash.finish(), identities[0]);
}

#[test]
fn polling_rejects_unsupported_execution_modes() {
    let mut config = polling_config(100);
    config.wasm_stack_switching(true);
    assert!(
        Engine::new(&config)
            .unwrap_err()
            .to_string()
            .contains("does not support stack switching")
    );

    let mut config = polling_config(100);
    config.concurrency_support(true);
    assert!(
        Engine::new(&config)
            .unwrap_err()
            .to_string()
            .contains("does not support component concurrency")
    );

    #[cfg(feature = "threads")]
    {
        let mut config = polling_config(100);
        config.wasm_threads(true);
        assert!(
            Engine::new(&config)
                .unwrap_err()
                .to_string()
                .contains("does not support shared-memory threads")
        );
    }
    #[cfg(feature = "component-model")]
    {
        let mut config = polling_config(100);
        config.wasm_component_model(true);
        assert!(
            Engine::new(&config)
                .unwrap_err()
                .to_string()
                .contains("does not support components")
        );
    }
    #[cfg(feature = "debug")]
    {
        let mut config = polling_config(100);
        config.guest_debug(true);
        assert!(
            Engine::new(&config)
                .unwrap_err()
                .to_string()
                .contains("does not support guest debugging")
        );
    }
}

#[test]
fn polling_stores_preserve_fuel_and_exception_charges() {
    for interval in [1, 7, 100, 100_000] {
        let engine = polling_engine(interval);
        for catch in [
            Catch::All { label: 0 },
            Catch::One { tag: 0, label: 0 },
            Catch::AllRef { label: 0 },
            Catch::OneRef { tag: 0, label: 0 },
        ] {
            let module = Module::new(&engine, exception_fuel_module(100, 2, true, catch)).unwrap();
            let mut store = Store::new(&engine, ());
            store.set_fuel(10_000).unwrap();
            store.set_epoch_deadline(1);
            let instance = Instance::new(&mut store, &module, &[]).unwrap();
            let run = instance
                .get_typed_func::<(), i32>(&mut store, "run")
                .unwrap();
            assert_eq!(run.call(&mut store, ()).unwrap(), 42);
            assert_eq!(store.get_fuel().unwrap(), 10_000 - 516);
            store.set_fuel(512).unwrap();
            let first = run.call(&mut store, ());
            if let Err(error) = first {
                assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            }
            assert_eq!(store.get_fuel().unwrap(), 0);
            assert_eq!(
                run.call(&mut store, ()).unwrap_err().downcast_ref::<Trap>(),
                Some(&Trap::OutOfFuel)
            );
        }
    }
}

#[test]
fn polling_retains_expired_native_entry_and_startup_checks() {
    let engine = polling_engine(100_000);
    let module = Module::new(&engine, call_tree_module(0, false)).unwrap();
    let mut store = Store::new(&engine, ());
    store.set_fuel(10_000).unwrap();
    store.set_epoch_deadline(1);
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .unwrap();
    engine.increment_epoch();
    assert_eq!(
        run.call(&mut store, ()).unwrap_err().downcast_ref::<Trap>(),
        Some(&Trap::Interrupt)
    );
    let startup = Module::new(&engine, call_tree_module(1, true)).unwrap();
    assert_eq!(
        Instance::new(&mut store, &startup, &[])
            .unwrap_err()
            .downcast_ref::<Trap>(),
        Some(&Trap::Interrupt)
    );
}

#[test]
fn polling_handles_internal_loops_call_trees_yields_and_callback_errors() {
    for is_tree in [false, true] {
        for error_callback in [false, true] {
            let engine = polling_engine(100);
            let bytes = if is_tree {
                call_tree_module(12, false)
            } else {
                exception_fuel_module(100_000, 0, false, Catch::All { label: 0 })
            };
            let module = Module::new(&engine, bytes).unwrap();
            let mut store = Store::new(&engine, 0_u32);
            store.set_fuel(1_000_000).unwrap();
            store.set_epoch_deadline(1);
            let instance = Instance::new(&mut store, &module, &[]).unwrap();
            store.set_epoch_deadline(0);
            store.epoch_deadline_callback(move |mut context| {
                *context.data_mut() += 1;
                if *context.data() == 3 {
                    if error_callback {
                        Err(Error::msg("polling callback stop"))
                    } else {
                        Ok(UpdateDeadline::Interrupt)
                    }
                } else {
                    Ok(UpdateDeadline::Yield(0))
                }
            });
            let (error, polls) = if is_tree {
                let run = instance
                    .get_typed_func::<(), ()>(&mut store, "run")
                    .unwrap();
                let (result, polls) = finish(run.call_async(&mut store, ()));
                (result.unwrap_err(), polls)
            } else {
                let run = instance
                    .get_typed_func::<(), i32>(&mut store, "run")
                    .unwrap();
                let (result, polls) = finish(run.call_async(&mut store, ()));
                (result.unwrap_err(), polls)
            };
            assert_eq!(*store.data(), 3);
            assert!(polls >= 3);
            assert!((200..220).contains(&(1_000_000 - store.get_fuel().unwrap())));
            if error_callback {
                assert_eq!(error.root_cause().to_string(), "polling callback stop");
            } else {
                assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::Interrupt));
            }
            assert!(!store.has_pending_exception());
        }
    }
}

#[test]
fn polling_checks_synchronous_host_return_and_native_reentry() {
    for reentry in [false, true] {
        let engine = polling_engine(100_000);
        let module = Module::new(&engine, host_return_module()).unwrap();
        let mut linker = Linker::new(&engine);
        let tick = engine.clone();
        linker
            .func_wrap(
                "host",
                "tick",
                move |mut caller: wasmtime::Caller<'_, ()>| -> Result<(), Error> {
                    tick.increment_epoch();
                    if reentry {
                        let run = caller
                            .get_export("reenter")
                            .unwrap()
                            .into_func()
                            .unwrap()
                            .typed::<(), i32>(&caller)?;
                        run.call(&mut caller, ())?;
                    }
                    Ok(())
                },
            )
            .unwrap();
        let mut store = Store::new(&engine, ());
        store.set_fuel(1_000_000).unwrap();
        store.set_epoch_deadline(1);
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let run = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .unwrap();
        assert_eq!(
            run.call(&mut store, ()).unwrap_err().downcast_ref::<Trap>(),
            Some(&Trap::Interrupt)
        );
    }
}

#[test]
fn polling_checks_asynchronous_host_return_and_yields() {
    let engine = polling_engine(100_000);
    let module = Module::new(&engine, host_return_module()).unwrap();
    let mut linker = Linker::new(&engine);
    let tick = engine.clone();
    linker
        .func_wrap_async(
            "host",
            "tick",
            move |_caller: wasmtime::Caller<'_, u32>, ()| {
                let tick = tick.clone();
                Box::new(async move {
                    let mut first = true;
                    std::future::poll_fn(move |context| {
                        if first {
                            first = false;
                            context.waker().wake_by_ref();
                            Poll::Pending
                        } else {
                            Poll::Ready(())
                        }
                    })
                    .await;
                    tick.increment_epoch();
                })
            },
        )
        .unwrap();
    let mut store = Store::new(&engine, 0_u32);
    store.set_fuel(1_000_000).unwrap();
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|mut context| {
        *context.data_mut() += 1;
        Ok(UpdateDeadline::Yield(1))
    });
    let (instance, _) = finish(linker.instantiate_async(&mut store, &module));
    let instance = instance.unwrap();
    let run = instance
        .get_typed_func::<(), i32>(&mut store, "run")
        .unwrap();
    let (result, polls) = finish(run.call_async(&mut store, ()));
    assert_eq!(result.unwrap(), 42);
    assert_eq!(*store.data(), 1);
    assert!(polls >= 3);
}

#[test]
fn polling_retains_checks_for_table_only_targets() {
    use wasm_encoder::{ConstExpr, ElementSection, Elements, RefType, TableSection, TableType};
    let engine = polling_engine(100_000);
    let mut m = EncodedModule::new();
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    m.section(&types);
    let mut functions = FunctionSection::new();
    functions.function(0);
    m.section(&functions);
    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        minimum: 1,
        maximum: Some(1),
        table64: false,
        shared: false,
    });
    m.section(&tables);
    let mut exports = ExportSection::new();
    exports.export("targets", ExportKind::Table, 0);
    m.section(&exports);
    let mut elements = ElementSection::new();
    elements.active(
        None,
        &ConstExpr::i32_const(0),
        Elements::Functions(Cow::Borrowed(&[0])),
    );
    m.section(&elements);
    let mut code = CodeSection::new();
    let mut target = Function::new([]);
    target.instruction(&I::I32Const(42)).instruction(&I::End);
    code.function(&target);
    m.section(&code);
    let module = Module::new(&engine, m.finish()).unwrap();
    let mut store = Store::new(&engine, ());
    store.set_fuel(10_000).unwrap();
    store.set_epoch_deadline(1);
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let table = instance.get_table(&mut store, "targets").unwrap();
    let wasmtime::Ref::Func(Some(target)) = table.get(&mut store, 0).unwrap() else {
        panic!("missing target")
    };
    let target = target.typed::<(), i32>(&store).unwrap();
    engine.increment_epoch();
    assert_eq!(
        target
            .call(&mut store, ())
            .unwrap_err()
            .downcast_ref::<Trap>(),
        Some(&Trap::Interrupt)
    );
}

#[test]
fn polling_preserves_host_reference_results_across_callbacks_and_gc() {
    let engine = polling_engine(100_000);
    let mut m = EncodedModule::new();
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::EXTERNREF]);
    types.ty().function([], []);
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import("host", "value", EntityType::Function(0));
    imports.import("host", "collect", EntityType::Function(1));
    m.section(&imports);
    let mut functions = FunctionSection::new();
    functions.function(0);
    m.section(&functions);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 2);
    m.section(&exports);
    let mut code = CodeSection::new();
    let mut run = Function::new([]);
    run.instruction(&I::Call(0))
        .instruction(&I::Call(1))
        .instruction(&I::End);
    code.function(&run);
    m.section(&code);
    let module = Module::new(&engine, m.finish()).unwrap();
    let mut linker = Linker::new(&engine);
    let tick = engine.clone();
    linker.func_wrap("host", "value", move |mut caller: wasmtime::Caller<'_, u32>| -> Result<Option<wasmtime::Rooted<wasmtime::ExternRef>>, Error> {
        let value = wasmtime::ExternRef::new(&mut caller, 42_i32)?;
        tick.increment_epoch();
        Ok(Some(value))
    }).unwrap();
    // Epoch hooks only expose StoreHookState. Collect in a host call while
    // the previous host result is live in the guest, after its deadline check.
    linker
        .func_wrap("host", "collect", |mut caller: wasmtime::Caller<'_, u32>| {
            caller.gc(None)
        })
        .unwrap();
    let mut store = Store::new(&engine, 0_u32);
    store.set_fuel(1_000_000).unwrap();
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|mut context| {
        *context.data_mut() += 1;
        Ok(UpdateDeadline::Continue(1))
    });
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let run = instance
        .get_typed_func::<(), Option<wasmtime::Rooted<wasmtime::ExternRef>>>(&mut store, "run")
        .unwrap();
    let value = run.call(&mut store, ()).unwrap().unwrap();
    assert_eq!(*store.data(), 1);
    assert_eq!(
        value.data(&store).unwrap().unwrap().downcast_ref::<i32>(),
        Some(&42)
    );
}

#[test]
fn polling_checks_host_exceptions_before_guest_catch() {
    for action in 0..4 {
        let engine = polling_engine(100_000);
        let mut m = EncodedModule::new();
        let mut types = TypeSection::new();
        types.ty().function([], []);
        types.ty().function([], [ValType::I32]);
        types.ty().function([ValType::I32], []);
        m.section(&types);
        let mut imports = ImportSection::new();
        imports.import("host", "throw", EntityType::Function(0));
        m.section(&imports);
        let mut functions = FunctionSection::new();
        functions.function(1).function(0);
        m.section(&functions);
        let mut tags = TagSection::new();
        tags.tag(TagType {
            kind: TagKind::Exception,
            func_type_idx: 2,
        });
        m.section(&tags);
        let mut exports = ExportSection::new();
        exports
            .export("run", ExportKind::Func, 1)
            .export("throw", ExportKind::Func, 2);
        m.section(&exports);
        let mut code = CodeSection::new();
        let mut run = Function::new([]);
        run.instruction(&I::Block(BlockType::Result(ValType::I32)))
            .instruction(&I::TryTable(
                BlockType::Empty,
                Cow::Borrowed(&[Catch::One { tag: 0, label: 0 }]),
            ))
            .instruction(&I::Call(0))
            .instruction(&I::Unreachable)
            .instruction(&I::End)
            .instruction(&I::Unreachable)
            .instruction(&I::End)
            .instruction(&I::End);
        code.function(&run);
        let mut throw = Function::new([]);
        throw
            .instruction(&I::I32Const(42))
            .instruction(&I::Throw(0))
            .instruction(&I::End);
        code.function(&throw);
        m.section(&code);
        let module = Module::new(&engine, m.finish()).unwrap();
        let mut linker = Linker::new(&engine);
        let tick = engine.clone();
        linker
            .func_wrap_async(
                "host",
                "throw",
                move |mut caller: wasmtime::Caller<'_, u32>, ()| {
                    let tick = tick.clone();
                    Box::new(async move {
                        let throw = caller
                            .get_export("throw")
                            .unwrap()
                            .into_func()
                            .unwrap()
                            .typed::<(), ()>(&caller)?;
                        let error = throw.call_async(&mut caller, ()).await.unwrap_err();
                        assert!(error.is::<wasmtime::ThrownException>());
                        caller.gc_async(None).await?;
                        tick.increment_epoch();
                        Err::<(), Error>(error)
                    })
                },
            )
            .unwrap();
        let mut store = Store::new(&engine, 0_u32);
        store.set_fuel(1_000_000).unwrap();
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |mut context| {
            *context.data_mut() += 1;
            match action {
                0 => Ok(UpdateDeadline::Continue(1)),
                1 => Ok(UpdateDeadline::Yield(1)),
                2 => Ok(UpdateDeadline::Interrupt),
                3 => Err(Error::msg("host-exception deadline stop")),
                _ => unreachable!(),
            }
        });
        let (instance, _) = finish(linker.instantiate_async(&mut store, &module));
        let run = instance
            .unwrap()
            .get_typed_func::<(), i32>(&mut store, "run")
            .unwrap();
        let (result, polls) = finish(run.call_async(&mut store, ()));
        assert_eq!(*store.data(), 1, "action={action}");
        match action {
            0 => assert_eq!(result.unwrap(), 42),
            1 => {
                assert_eq!(result.unwrap(), 42);
                assert!(polls >= 2);
            }
            2 => assert_eq!(
                result.unwrap_err().downcast_ref::<Trap>(),
                Some(&Trap::Interrupt)
            ),
            3 => assert_eq!(
                result.unwrap_err().root_cause().to_string(),
                "host-exception deadline stop"
            ),
            _ => unreachable!(),
        }
        assert!(!store.has_pending_exception());
    }
}
