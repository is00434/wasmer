use crate::exports::{ExportError, Exportable};
use crate::externals::Extern;
use crate::store::Store;
use crate::types::Val;
use crate::FunctionType;
use crate::NativeFunc;
use crate::RuntimeError;
pub use inner::{HostFunction, WasmTypeList};
use inner::{WithEnv, WithoutEnv};
use std::cell::RefCell;
use std::cmp::max;
use wasmer_runtime::{
    raise_user_trap, resume_panic, wasmer_call_trampoline, Export, ExportFunction,
    VMCallerCheckedAnyfunc, VMContext, VMDynamicFunctionContext, VMFunctionBody, VMFunctionKind,
    VMTrampoline,
};

/// A function defined in the Wasm module
#[derive(Clone, PartialEq)]
pub struct WasmFunctionDefinition {
    // The trampoline to do the call
    pub(crate) trampoline: VMTrampoline,
}

/// A function defined in the Host
#[derive(Clone, PartialEq)]
pub struct HostFunctionDefinition {
    /// If the host function has a custom environment attached
    pub(crate) has_env: bool,
}

/// The inner helper
#[derive(Clone, PartialEq)]
pub enum FunctionDefinition {
    /// A function defined in the Wasm side
    Wasm(WasmFunctionDefinition),
    /// A function defined in the Host side
    Host(HostFunctionDefinition),
}

/// A WebAssembly `function`.
#[derive(Clone, PartialEq)]
pub struct Function {
    pub(crate) store: Store,
    pub(crate) definition: FunctionDefinition,
    // If the Function is owned by the Store, not the instance
    pub(crate) owned_by_store: bool,
    pub(crate) exported: ExportFunction,
}

impl Function {
    /// Creates a new `Func` with the given parameters.
    ///
    /// * `store` - a global cache to store information in
    /// * `func` - the function.
    pub fn new<F, Args, Rets, Env>(store: &Store, func: F) -> Self
    where
        F: HostFunction<Args, Rets, WithoutEnv, Env>,
        Args: WasmTypeList,
        Rets: WasmTypeList,
        Env: Sized + 'static,
    {
        let func: inner::Func<Args, Rets> = inner::Func::new(func);
        let address = func.address() as *const VMFunctionBody;
        let vmctx = std::ptr::null_mut() as *mut _ as *mut VMContext;
        let signature = func.ty();
        Self {
            store: store.clone(),
            owned_by_store: true,
            definition: FunctionDefinition::Host(HostFunctionDefinition { has_env: false }),
            exported: ExportFunction {
                address,
                vmctx,
                signature,
                kind: VMFunctionKind::Static,
            },
        }
    }

    #[allow(clippy::cast_ptr_alignment)]
    pub fn new_dynamic<F>(store: &Store, ty: &FunctionType, func: F) -> Self
    where
        F: Fn(&[Val]) -> Result<Vec<Val>, RuntimeError> + 'static,
    {
        let dynamic_ctx = VMDynamicFunctionContext::from_context(VMDynamicFunctionWithoutEnv {
            func: Box::new(func),
            function_type: ty.clone(),
        });
        // We don't yet have the address with the Wasm ABI signature.
        // The engine linker will replace the address with one pointing to a
        // generated dynamic trampoline.
        let address = std::ptr::null() as *const VMFunctionBody;
        let vmctx = Box::into_raw(Box::new(dynamic_ctx)) as *mut VMContext;
        Self {
            store: store.clone(),
            owned_by_store: true,
            definition: FunctionDefinition::Host(HostFunctionDefinition { has_env: false }),
            exported: ExportFunction {
                address,
                kind: VMFunctionKind::Dynamic,
                vmctx,
                signature: ty.clone(),
            },
        }
    }

    #[allow(clippy::cast_ptr_alignment)]
    pub fn new_dynamic_env<F, Env>(store: &Store, ty: &FunctionType, env: Env, func: F) -> Self
    where
        F: Fn(&mut Env, &[Val]) -> Result<Vec<Val>, RuntimeError> + 'static,
        Env: Sized + 'static,
    {
        let dynamic_ctx = VMDynamicFunctionContext::from_context(VMDynamicFunctionWithEnv {
            env: RefCell::new(env),
            func: Box::new(func),
            function_type: ty.clone(),
        });
        // We don't yet have the address with the Wasm ABI signature.
        // The engine linker will replace the address with one pointing to a
        // generated dynamic trampoline.
        let address = std::ptr::null() as *const VMFunctionBody;
        let vmctx = Box::into_raw(Box::new(dynamic_ctx)) as *mut VMContext;
        Self {
            store: store.clone(),
            owned_by_store: true,
            definition: FunctionDefinition::Host(HostFunctionDefinition { has_env: true }),
            exported: ExportFunction {
                address,
                kind: VMFunctionKind::Dynamic,
                vmctx,
                signature: ty.clone(),
            },
        }
    }

    /// Creates a new `Func` with the given parameters.
    ///
    /// * `store` - a global cache to store information in.
    /// * `env` - the function environment.
    /// * `func` - the function.
    pub fn new_env<F, Args, Rets, Env>(store: &Store, env: Env, func: F) -> Self
    where
        F: HostFunction<Args, Rets, WithEnv, Env>,
        Args: WasmTypeList,
        Rets: WasmTypeList,
        Env: Sized + 'static,
    {
        let func: inner::Func<Args, Rets> = inner::Func::new(func);
        let address = func.address() as *const VMFunctionBody;
        // TODO: We need to refactor the Function context.
        // Right now is structured as it's always a `VMContext`. However, only
        // Wasm-defined functions have a `VMContext`.
        // In the case of Host-defined functions `VMContext` is whatever environment
        // the user want to attach to the function.
        let box_env = Box::new(env);
        let vmctx = Box::into_raw(box_env) as *mut _ as *mut VMContext;
        let signature = func.ty();
        Self {
            store: store.clone(),
            owned_by_store: true,
            definition: FunctionDefinition::Host(HostFunctionDefinition { has_env: true }),
            exported: ExportFunction {
                address,
                kind: VMFunctionKind::Static,
                vmctx,
                signature,
            },
        }
    }

    /// Returns the underlying type of this function.
    pub fn ty(&self) -> &FunctionType {
        &self.exported.signature
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn call_wasm(
        &self,
        func: &WasmFunctionDefinition,
        params: &[Val],
        results: &mut [Val],
    ) -> Result<(), RuntimeError> {
        let format_types_for_error_message = |items: &[Val]| {
            items
                .iter()
                .map(|param| param.ty().to_string())
                .collect::<Vec<String>>()
                .join(", ")
        };
        let signature = self.ty();
        if signature.params().len() != params.len() {
            return Err(RuntimeError::new(format!(
                "Parameters of type [{}] did not match signature {}",
                format_types_for_error_message(params),
                &signature
            )));
        }
        if signature.results().len() != results.len() {
            return Err(RuntimeError::new(format!(
                "Results of type [{}] did not match signature {}",
                format_types_for_error_message(results),
                &signature,
            )));
        }

        let mut values_vec = vec![0; max(params.len(), results.len())];

        // Store the argument values into `values_vec`.
        let param_tys = signature.params().iter();
        for ((arg, slot), ty) in params.iter().zip(&mut values_vec).zip(param_tys) {
            if arg.ty() != *ty {
                let param_types = format_types_for_error_message(params);
                return Err(RuntimeError::new(format!(
                    "Parameters of type [{}] did not match signature {}",
                    param_types, &signature,
                )));
            }
            unsafe {
                arg.write_value_to(slot);
            }
        }

        // Call the trampoline.
        if let Err(error) = unsafe {
            wasmer_call_trampoline(
                self.exported.vmctx,
                func.trampoline,
                self.exported.address,
                values_vec.as_mut_ptr() as *mut u8,
            )
        } {
            return Err(RuntimeError::from_trap(error));
        }

        // Load the return values out of `values_vec`.
        for (index, &value_type) in signature.results().iter().enumerate() {
            unsafe {
                let ptr = values_vec.as_ptr().add(index);
                results[index] = Val::read_value_from(ptr, value_type);
            }
        }

        Ok(())
    }

    /// Returns the number of parameters that this function takes.
    pub fn param_arity(&self) -> usize {
        self.ty().params().len()
    }

    /// Returns the number of results this function produces.
    pub fn result_arity(&self) -> usize {
        self.ty().results().len()
    }

    /// Call the [`Function`] function.
    ///
    /// Depending on where the Function is defined, it will call it.
    /// 1. If the function is defined inside a WebAssembly, it will call the trampoline
    ///    for the function signature.
    /// 2. If the function is defined in the host (in a native way), it will
    ///    call the trampoline.
    pub fn call(&self, params: &[Val]) -> Result<Box<[Val]>, RuntimeError> {
        let mut results = vec![Val::null(); self.result_arity()];

        match &self.definition {
            FunctionDefinition::Wasm(wasm) => {
                self.call_wasm(&wasm, params, &mut results)?;
            }
            _ => unimplemented!("The function definition isn't supported for the moment"),
        }

        Ok(results.into_boxed_slice())
    }

    pub(crate) fn from_export(store: &Store, wasmer_export: ExportFunction) -> Self {
        let vmsignature = store.engine().register_signature(&wasmer_export.signature);
        let trampoline = store
            .engine()
            .function_call_trampoline(vmsignature)
            .expect("Can't get call trampoline for the function");
        Self {
            store: store.clone(),
            owned_by_store: false,
            definition: FunctionDefinition::Wasm(WasmFunctionDefinition { trampoline }),
            exported: wasmer_export,
        }
    }

    pub(crate) fn checked_anyfunc(&self) -> VMCallerCheckedAnyfunc {
        let vmsignature = self
            .store
            .engine()
            .register_signature(&self.exported.signature);
        VMCallerCheckedAnyfunc {
            func_ptr: self.exported.address,
            type_index: vmsignature,
            vmctx: self.exported.vmctx,
        }
    }

    pub fn native<'a, Args, Rets>(&self) -> Option<NativeFunc<'a, Args, Rets>>
    where
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        // type check
        if self.exported.signature.params() != Args::wasm_types() {
            // todo: error param types don't match
            return None;
        }
        if self.exported.signature.results() != Rets::wasm_types() {
            // todo: error result types don't match
            return None;
        }

        Some(NativeFunc::new(
            self.store.clone(),
            self.exported.address,
            self.exported.vmctx,
            self.exported.kind,
            self.definition.clone(),
        ))
    }
}

impl<'a> Exportable<'a> for Function {
    fn to_export(&self) -> Export {
        self.exported.clone().into()
    }
    fn get_self_from_extern(_extern: &'a Extern) -> Result<&'a Self, ExportError> {
        match _extern {
            Extern::Function(func) => Ok(func),
            _ => Err(ExportError::IncompatibleType),
        }
    }
}

impl std::fmt::Debug for Function {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// This trait is one that all dynamic functions must fulfill.
pub(crate) trait VMDynamicFunction {
    fn call(&self, args: &[Val]) -> Result<Vec<Val>, RuntimeError>;
    fn function_type(&self) -> &FunctionType;
}

pub(crate) struct VMDynamicFunctionWithoutEnv {
    #[allow(clippy::type_complexity)]
    func: Box<dyn Fn(&[Val]) -> Result<Vec<Val>, RuntimeError> + 'static>,
    function_type: FunctionType,
}

impl VMDynamicFunction for VMDynamicFunctionWithoutEnv {
    fn call(&self, args: &[Val]) -> Result<Vec<Val>, RuntimeError> {
        (*self.func)(&args)
    }
    fn function_type(&self) -> &FunctionType {
        &self.function_type
    }
}

pub(crate) struct VMDynamicFunctionWithEnv<Env>
where
    Env: Sized + 'static,
{
    function_type: FunctionType,
    #[allow(clippy::type_complexity)]
    func: Box<dyn Fn(&mut Env, &[Val]) -> Result<Vec<Val>, RuntimeError> + 'static>,
    env: RefCell<Env>,
}

impl<Env> VMDynamicFunction for VMDynamicFunctionWithEnv<Env>
where
    Env: Sized + 'static,
{
    fn call(&self, args: &[Val]) -> Result<Vec<Val>, RuntimeError> {
        // TODO: the `&mut *self.env.as_ptr()` is likely invoking some "mild"
        //      undefined behavior due to how it's used in the static fn call
        unsafe { (*self.func)(&mut *self.env.as_ptr(), &args) }
    }
    fn function_type(&self) -> &FunctionType {
        &self.function_type
    }
}

trait VMDynamicFunctionCall<T: VMDynamicFunction> {
    fn from_context(ctx: T) -> Self;
    fn address_ptr() -> *const VMFunctionBody;
    unsafe fn func_wrapper(&self, values_vec: *mut i128);
}

impl<T: VMDynamicFunction> VMDynamicFunctionCall<T> for VMDynamicFunctionContext<T> {
    fn from_context(ctx: T) -> Self {
        Self {
            address: Self::address_ptr(),
            ctx,
        }
    }

    fn address_ptr() -> *const VMFunctionBody {
        Self::func_wrapper as *const () as *const VMFunctionBody
    }

    // This function wraps our func, to make it compatible with the
    // reverse trampoline signature
    unsafe fn func_wrapper(
        // Note: we use the trick that the first param to this function is the `VMDynamicFunctionContext`
        // itself, so rather than doing `dynamic_ctx: &VMDynamicFunctionContext<T>`, we simplify it a bit
        &self,
        values_vec: *mut i128,
    ) {
        use std::panic::{self, AssertUnwindSafe};
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let func_ty = self.ctx.function_type();
            let mut args = Vec::with_capacity(func_ty.params().len());
            for (i, ty) in func_ty.params().iter().enumerate() {
                args.push(Val::read_value_from(values_vec.add(i), *ty));
            }
            let returns = self.ctx.call(&args)?;

            // We need to dynamically check that the returns
            // match the expected types, as well as expected length.
            let return_types = returns.iter().map(|ret| ret.ty()).collect::<Vec<_>>();
            if return_types != func_ty.results() {
                return Err(RuntimeError::new(format!(
                    "Dynamic function returned wrong signature. Expected {:?} but got {:?}",
                    func_ty.results(),
                    return_types
                )));
            }
            for (i, ret) in returns.iter().enumerate() {
                ret.write_value_to(values_vec.add(i));
            }
            Ok(())
        }));

        match result {
            Ok(Ok(())) => {}
            Ok(Err(trap)) => raise_user_trap(Box::new(trap)),
            Err(panic) => resume_panic(panic),
        }
    }
}

mod inner {
    use std::convert::Infallible;
    use std::error::Error;
    use std::marker::PhantomData;
    use std::panic::{self, AssertUnwindSafe};
    use wasm_common::{FunctionType, NativeWasmType, Type, WasmExternType};
    use wasmer_runtime::{raise_user_trap, resume_panic};

    /// Represents a list of WebAssembly values.
    pub trait WasmTypeList {
        /// CStruct type.
        type CStruct;

        /// Array of return values.
        type Array: AsMut<[i128]>;

        /// Construct `Self` based on an array of returned values.
        fn from_array(array: Self::Array) -> Self;

        /// Transforms Rust values into an Array
        fn into_array(self) -> Self::Array;

        /// Generates an empty array that will hold the returned values of
        /// the WebAssembly function.
        fn empty_array() -> Self::Array;

        /// Transforms C values into Rust values.
        fn from_c_struct(c_struct: Self::CStruct) -> Self;

        /// Transforms Rust values into C values.
        fn into_c_struct(self) -> Self::CStruct;

        /// Get types of the current values.
        fn wasm_types() -> &'static [Type];
    }

    /// Represents a TrapEarly type.
    pub trait TrapEarly<Rets>
    where
        Rets: WasmTypeList,
    {
        /// The error type for this trait.
        type Error: Error + Sync + Send + 'static;

        /// Get returns or error result.
        fn report(self) -> Result<Rets, Self::Error>;
    }

    impl<Rets> TrapEarly<Rets> for Rets
    where
        Rets: WasmTypeList,
    {
        type Error = Infallible;

        fn report(self) -> Result<Self, Infallible> {
            Ok(self)
        }
    }

    impl<Rets, E> TrapEarly<Rets> for Result<Rets, E>
    where
        Rets: WasmTypeList,
        E: Error + Sync + Send + 'static,
    {
        type Error = E;

        fn report(self) -> Self {
            self
        }
    }

    /// Empty trait to specify the kind of `HostFunction`: With or
    /// without a `vm::Ctx` argument. See the `ExplicitVmCtx` and the
    /// `ImplicitVmCtx` structures.
    ///
    /// This trait is never aimed to be used by a user. It is used by the
    /// trait system to automatically generate an appropriate `wrap`
    /// function.
    #[doc(hidden)]
    pub trait HostFunctionKind {}

    /// An empty struct to help Rust typing to determine
    /// when a `HostFunction` doesn't take an Environment
    pub struct WithEnv {}

    impl HostFunctionKind for WithEnv {}

    /// An empty struct to help Rust typing to determine
    /// when a `HostFunction` takes an Environment
    pub struct WithoutEnv {}

    impl HostFunctionKind for WithoutEnv {}

    /// Represents a function that can be converted to a `vm::Func`
    /// (function pointer) that can be called within WebAssembly.
    pub trait HostFunction<Args, Rets, Kind, T>
    where
        Args: WasmTypeList,
        Rets: WasmTypeList,
        Kind: HostFunctionKind,
        T: Sized,
        Self: Sized,
    {
        /// Convert to function pointer.
        fn to_raw(self) -> *const FunctionBody;
    }

    #[repr(transparent)]
    pub struct FunctionBody(*mut u8);

    /// Represents a function that can be used by WebAssembly.
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct Func<Args = (), Rets = ()> {
        address: *const FunctionBody,
        _phantom: PhantomData<(Args, Rets)>,
    }

    unsafe impl<Args, Rets> Send for Func<Args, Rets> {}

    impl<Args, Rets> Func<Args, Rets>
    where
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        /// Creates a new `Func`.
        pub fn new<F, T, E>(func: F) -> Self
        where
            F: HostFunction<Args, Rets, T, E>,
            T: HostFunctionKind,
            E: Sized,
        {
            Self {
                address: func.to_raw(),
                _phantom: PhantomData,
            }
        }

        /// Get the type of the Func
        pub fn ty(&self) -> FunctionType {
            FunctionType::new(Args::wasm_types(), Rets::wasm_types())
        }

        /// Get the address of the Func
        pub fn address(&self) -> *const FunctionBody {
            self.address
        }
    }

    macro_rules! impl_traits {
        ( [$repr:ident] $struct_name:ident, $( $x:ident ),* ) => {
            /// Struct for typed funcs.
            #[repr($repr)]
            pub struct $struct_name< $( $x ),* > ( $( <$x as WasmExternType>::Native ),* )
            where
                $( $x: WasmExternType ),*;

            #[allow(unused_parens, dead_code)]
            impl< $( $x ),* > WasmTypeList for ( $( $x ),* )
            where
                $( $x: WasmExternType ),*
            {
                type CStruct = $struct_name<$( $x ),*>;

                type Array = [i128; count_idents!( $( $x ),* )];

                fn from_array(array: Self::Array) -> Self {
                    #[allow(non_snake_case)]
                    let [ $( $x ),* ] = array;

                    ( $( WasmExternType::from_native(NativeWasmType::from_binary($x)) ),* )
                }

                fn into_array(self) -> Self::Array {
                    #[allow(non_snake_case)]
                    let ( $( $x ),* ) = self;
                    [ $( WasmExternType::to_native($x).to_binary() ),* ]
                }

                fn empty_array() -> Self::Array {
                    [0; count_idents!( $( $x ),* )]
                }

                fn from_c_struct(c_struct: Self::CStruct) -> Self {
                    #[allow(non_snake_case)]
                    let $struct_name ( $( $x ),* ) = c_struct;

                    ( $( WasmExternType::from_native($x) ),* )
                }

                #[allow(unused_parens, non_snake_case)]
                fn into_c_struct(self) -> Self::CStruct {
                    let ( $( $x ),* ) = self;

                    $struct_name ( $( WasmExternType::to_native($x) ),* )
                }

                fn wasm_types() -> &'static [Type] {
                    &[$( $x::Native::WASM_TYPE ),*]
                }
            }

            #[allow(unused_parens)]
            impl< $( $x, )* Rets, Trap, FN > HostFunction<( $( $x ),* ), Rets, WithoutEnv, ()> for FN
            where
                $( $x: WasmExternType, )*
                Rets: WasmTypeList,
                Trap: TrapEarly<Rets>,
                FN: Fn($( $x , )*) -> Trap + 'static + Send,
            {
                #[allow(non_snake_case)]
                fn to_raw(self) -> *const FunctionBody {
                    extern fn wrap<$( $x, )* Rets, Trap, FN>( _: usize, $($x: $x::Native, )* ) -> Rets::CStruct
                    where
                        Rets: WasmTypeList,
                        Trap: TrapEarly<Rets>,
                        $( $x: WasmExternType, )*
                        FN: Fn( $( $x ),* ) -> Trap + 'static
                    {
                        let f: &FN = unsafe { &*(&() as *const () as *const FN) };
                        let result = panic::catch_unwind(AssertUnwindSafe(|| {
                            f( $( WasmExternType::from_native($x) ),* ).report()
                        }));

                        match result {
                            Ok(Ok(result)) => return result.into_c_struct(),
                            Ok(Err(trap)) => unsafe { raise_user_trap(Box::new(trap)) },
                            Err(panic) => unsafe { resume_panic(panic) },
                        }
                    }

                    wrap::<$( $x, )* Rets, Trap, Self> as *const FunctionBody
                }
            }

            #[allow(unused_parens)]
            impl< $( $x, )* Rets, Trap, T, FN > HostFunction<( $( $x ),* ), Rets, WithEnv, T> for FN
            where
                $( $x: WasmExternType, )*
                Rets: WasmTypeList,
                Trap: TrapEarly<Rets>,
                T: Sized,
                FN: Fn(&mut T, $( $x , )*) -> Trap + 'static + Send
            {
                #[allow(non_snake_case)]
                fn to_raw(self) -> *const FunctionBody {
                    extern fn wrap<$( $x, )* Rets, Trap, T, FN>( ctx: &mut T, $($x: $x::Native, )* ) -> Rets::CStruct
                    where
                        Rets: WasmTypeList,
                        Trap: TrapEarly<Rets>,
                        $( $x: WasmExternType, )*
                        T: Sized,
                        FN: Fn(&mut T, $( $x ),* ) -> Trap + 'static
                    {
                        let f: &FN = unsafe { &*(&() as *const () as *const FN) };

                        let result = panic::catch_unwind(AssertUnwindSafe(|| {
                            f(ctx, $( WasmExternType::from_native($x) ),* ).report()
                        }));

                        match result {
                            Ok(Ok(result)) => return result.into_c_struct(),
                            Ok(Err(trap)) => unsafe { raise_user_trap(Box::new(trap)) },
                            Err(panic) => unsafe { resume_panic(panic) },
                        }
                    }

                    wrap::<$( $x, )* Rets, Trap, T, Self> as *const FunctionBody
                }
            }
        };
    }

    macro_rules! count_idents {
        ( $($idents:ident),* ) => {{
            #[allow(dead_code, non_camel_case_types)]
            enum Idents { $($idents,)* __CountIdentsLast }
            const COUNT: usize = Idents::__CountIdentsLast as usize;
            COUNT
        }};
    }

    impl_traits!([C] S0,);
    //impl_traits!([transparent] S1, A1);
    impl_traits!([C] S2, A1, A2);
    impl_traits!([C] S3, A1, A2, A3);
    impl_traits!([C] S4, A1, A2, A3, A4);
    impl_traits!([C] S5, A1, A2, A3, A4, A5);
    impl_traits!([C] S6, A1, A2, A3, A4, A5, A6);
    impl_traits!([C] S7, A1, A2, A3, A4, A5, A6, A7);
    impl_traits!([C] S8, A1, A2, A3, A4, A5, A6, A7, A8);
    impl_traits!([C] S9, A1, A2, A3, A4, A5, A6, A7, A8, A9);
    impl_traits!([C] S10, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
    impl_traits!([C] S11, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
    impl_traits!([C] S12, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
    impl_traits!([C] S13, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
    impl_traits!([C] S14, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
    impl_traits!([C] S15, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15);
    impl_traits!([C] S16, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16);
    impl_traits!([C] S17, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17);
    impl_traits!([C] S18, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18);
    impl_traits!([C] S19, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19);
    impl_traits!([C] S20, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20);
    impl_traits!([C] S21, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21);
    impl_traits!([C] S22, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22);
    impl_traits!([C] S23, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23);
    impl_traits!([C] S24, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24);
    impl_traits!([C] S25, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25);
    impl_traits!([C] S26, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25, A26);

    #[cfg(test)]
    mod test_wasm_type_list {
        use super::*;
        use crate::types::Type;
        // WasmTypeList

        #[test]
        fn test_simple_values() {
            // Simple values
            assert_eq!(<i32>::wasm_types(), [Type::I32]);
            assert_eq!(<i64>::wasm_types(), [Type::I64]);
            assert_eq!(<f32>::wasm_types(), [Type::F32]);
            assert_eq!(<f64>::wasm_types(), [Type::F64]);

            // Multi values
            assert_eq!(<(i32, i32)>::wasm_types(), [Type::I32, Type::I32]);
            assert_eq!(<(i64, i64)>::wasm_types(), [Type::I64, Type::I64]);
            assert_eq!(<(f32, f32)>::wasm_types(), [Type::F32, Type::F32]);
            assert_eq!(<(f64, f64)>::wasm_types(), [Type::F64, Type::F64]);

            // Mixed values
            // assert_eq!(<(i32, i64, f32, f64)>::wasm_types(), [Type::I32, Type::I64, Type::F32, Type::F64]);
        }

        #[test]
        fn test_empty_array() {
            assert_eq!(<()>::empty_array().len(), 0);
            assert_eq!(<i32>::empty_array().len(), 1);
            assert_eq!(<(i32, i64)>::empty_array().len(), 2);
        }

        // #[test]
        // fn test_from_array() {
        //     assert_eq!(<()>::from_array([]), ());
        //     assert_eq!(<(i32)>::from_array([1]), (1));
        //     assert_eq!(<(i32, i32)>::from_array([1, 1]), (1, 1));
        //     // This doesn't work
        //     // assert_eq!(<(i32, i64, f32, f64)>::from_array([1, 2, (3.1f32).to_bits().into(), (4.2f64).to_bits().into()]), (1, 2, 3.1f32, 4.2f64));
        // }

        // #[test]
        // fn test_into_array() {
        //     assert_eq!(().into_array(), []);
        //     assert_eq!((1).into_array(), [1]);
        //     assert_eq!((1, 2).into_array(), [1, 2]);
        //     assert_eq!((1, 2, 3).into_array(), [1, 2, 3]);
        //     // This doesn't work
        //     // assert_eq!(<(i32, i64, f32, f64)>::from_array([1, 2, (3.1f32).to_bits().into(), (4.2f64).to_bits().into()]), (1, 2, 3.1f32, 4.2f64));
        // }

        #[test]
        fn test_into_c_struct() {
            // assert_eq!(<()>::into_c_struct(), &[]);
        }
    }

    #[allow(non_snake_case)]
    #[cfg(test)]
    mod test_func {
        use super::*;
        use crate::types::Type;
        use std::ptr;
        // WasmTypeList

        fn func() {}
        fn func__i32() -> i32 {
            0
        }
        fn func_i32(_a: i32) {}
        fn func_i32__i32(a: i32) -> i32 {
            a * 2
        }
        fn func_i32_i32__i32(a: i32, b: i32) -> i32 {
            a + b
        }
        fn func_i32_i32__i32_i32(a: i32, b: i32) -> (i32, i32) {
            (a, b)
        }
        fn func_f32_i32__i32_f32(a: f32, b: i32) -> (i32, f32) {
            (b, a)
        }

        #[test]
        fn test_function_types() {
            assert_eq!(Func::new(func).ty(), FunctionType::new(vec![], vec![]));
            assert_eq!(
                Func::new(func__i32).ty(),
                FunctionType::new(vec![], vec![Type::I32])
            );
            assert_eq!(
                Func::new(func_i32).ty(),
                FunctionType::new(vec![Type::I32], vec![])
            );
            assert_eq!(
                Func::new(func_i32__i32).ty(),
                FunctionType::new(vec![Type::I32], vec![Type::I32])
            );
            assert_eq!(
                Func::new(func_i32_i32__i32).ty(),
                FunctionType::new(vec![Type::I32, Type::I32], vec![Type::I32])
            );
            assert_eq!(
                Func::new(func_i32_i32__i32_i32).ty(),
                FunctionType::new(vec![Type::I32, Type::I32], vec![Type::I32, Type::I32])
            );
            assert_eq!(
                Func::new(func_f32_i32__i32_f32).ty(),
                FunctionType::new(vec![Type::F32, Type::I32], vec![Type::I32, Type::F32])
            );
        }

        #[test]
        fn test_function_pointer() {
            let f = Func::new(func_i32__i32);
            let function = unsafe {
                std::mem::transmute::<*const FunctionBody, fn(usize, i32) -> i32>(f.address)
            };
            assert_eq!(function(0, 3), 6);
        }

        #[test]
        fn test_function_call() {
            let f = Func::new(func_i32__i32);
            let x = |args: <(i32, i32) as WasmTypeList>::Array,
                     rets: &mut <(i32, i32) as WasmTypeList>::Array| {
                let result = func_i32_i32__i32_i32(args[0] as _, args[1] as _);
                rets[0] = result.0 as _;
                rets[1] = result.1 as _;
            };
            let mut rets = <(i64, i64)>::empty_array();
            x([20, 10], &mut rets);
            // panic!("Rets: {:?}",rets);
            let mut rets = <(i64)>::empty_array();
            // let result = f.call([1], &mut rets);
            // assert_eq!(result.is_err(), true);
        }
    }
}
