use wasmer::{
    imports, wat2wasm, Extern, Function, Instance, Memory, MemoryType, Module, NativeFunc, Pages,
    Store, Table, TableType, Type, Value,
};
//use wasmer_compiler_cranelift::Cranelift;
use wasmer_compiler_singlepass::Singlepass;
use wasmer_engine_jit::JIT;

/// A function we'll call through a table.
fn host_callback(arg1: i32, arg2: i32) -> i32 {
    arg1 + arg2
}

fn main() -> anyhow::Result<()> {
    let wasm_bytes = wat2wasm(
        r#"
(module
  (type $mem_size_t (func (result i32)))
  (type $get_at_t (func (param i32) (result i32)))
  (type $set_at_t (func (param i32) (param i32)))

  (memory $mem 1)
  ;;(import "env" "memory" (memory $mem 1))

  (func $get_at (type $get_at_t) (param $idx i32) (result i32)
    (i32.load (local.get $idx)))

  (func $set_at (type $set_at_t) (param $idx i32) (param $val i32)
    (i32.store (local.get $idx) (local.get $val)))

  (func $mem_size (type $mem_size_t) (result i32)
    (memory.size))

  (export "get_at" (func $get_at))
  (export "set_at" (func $set_at))
  (export "mem_size" (func $mem_size))
  (export "memory" (memory $mem)))
"#
        .as_bytes(),
    )?;

    // We set up our store with an engine and a compiler.
    //let store = Store::new(&JIT::new(&Cranelift::default()).engine());
    let store = Store::new(&JIT::new(&Singlepass::default()).engine());
    // Then compile our Wasm.
    let module = Module::new(&store, wasm_bytes)?;
    //let memory = Memory::new(&store, MemoryType::new(1, None, false))?;
    let import_object = imports! {
        /*"env" => {
            "memory" => memory,
        }*/
    };
    // And instantiate it with no imports.
    let instance = Instance::new(&module, &import_object)?;

    let mem_size: NativeFunc<(), i32> = instance.exports.get_native_function("mem_size")?;
    let get_at: NativeFunc<i32, i32> = instance.exports.get_native_function("get_at")?;
    let set_at: NativeFunc<(i32, i32), ()> = instance.exports.get_native_function("set_at")?;
    let memory = instance.exports.get_memory("memory")?;

    let mem_addr = 0x2220;
    let val = 0xFEFEFFE;

    dbg!("before grow");

    assert_eq!(memory.size(), Pages::from(1));
    memory.grow(2)?;
    dbg!("after first grow");
    assert_eq!(memory.size(), Pages::from(3));
    let result = mem_size.call()?;
    assert_eq!(result, 3);

    dbg!("Setting value to read later");
    // -------------
    set_at.call(mem_addr, val)?;
    // -------------
    dbg!("Value set correctly");

    let page_size = 0x1_0000;
    let result = get_at.call(page_size * 3 - 4)?;
    dbg!("Before second grow");
    memory.grow(1025)?;
    dbg!("After second grow");
    assert_eq!(memory.size(), Pages::from(1028));
    set_at.call(page_size * 1027 - 4, 123456)?;
    let result = get_at.call(page_size * 1027 - 4)?;
    assert_eq!(result, 123456);
    set_at.call(1024, 123456)?;
    let result = get_at.call(1024)?;
    assert_eq!(result, 123456);

    // -------------
    let result = get_at.call(mem_addr)?;
    assert_eq!(result, val);
    // -------------

    //let result = get_at.call(page_size * 1028 - 4)?;

    Ok(())
}
