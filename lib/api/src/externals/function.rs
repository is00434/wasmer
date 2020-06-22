use crate::exports::{ExportError, Exportable};
use crate::externals::Extern;
use crate::store::Store;
use crate::types::Val;
use crate::FunctionType;
use crate::NativeFunc;
use crate::RuntimeError;
pub use inner::{HostFunction, WasmExternType, WasmTypeList};
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
    /// Creates a new `Function` that is:
    ///
    /// 1. Static/Monomorphic, i.e. all inputs and outputs have a
    ///    unique _statically declared type_. The outputs can be
    ///    wrapped in a `Result`.
    /// 2. Independent, i.e. the function _does not_ receive an
    ///    environment argument.
    pub fn new<F, Args, Rets, Env>(store: &Store, func: F) -> Self
    where
        F: HostFunction<Args, Rets, WithoutEnv, Env>,
        Args: WasmTypeList,
        Rets: WasmTypeList,
        Env: Sized + 'static,
    {
        let function = inner::Function::<Args, Rets>::new(func);
        let address = function.address() as *const VMFunctionBody;
        let vmctx = std::ptr::null_mut() as *mut _ as *mut VMContext;
        let signature = function.ty();

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

    /// Creates a new `Function` that is:
    ///
    /// 1. Static/Monomorphic, i.e. all inputs and outputs have a
    ///    unique statically declared type. The outputs can be wrapped
    ///    in a `Result`.
    /// 2. Dependent, i.e. the function _does_ receive an environment
    ///    argument (given by `env`).
    pub fn new_env<F, Args, Rets, Env>(store: &Store, env: Env, func: F) -> Self
    where
        F: HostFunction<Args, Rets, WithEnv, Env>,
        Args: WasmTypeList,
        Rets: WasmTypeList,
        Env: Sized + 'static,
    {
        let function = inner::Function::<Args, Rets>::new(func);
        let address = function.address();

        // TODO: We need to refactor the Function context.
        // Right now is structured as it's always a `VMContext`. However, only
        // Wasm-defined functions have a `VMContext`.
        // In the case of Host-defined functions `VMContext` is whatever environment
        // the user want to attach to the function.
        let box_env = Box::new(env);
        let vmctx = Box::into_raw(box_env) as *mut _ as *mut VMContext;
        let signature = function.ty();

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

    /// Creates a new `Function` that is:
    ///
    /// 1. Dynamic/Polymorphic, i.e. all inputs are received in a
    ///    slice of `Val` (the set of all Wasm values), and all
    ///    outputs are stored in a vector of `Val`, wrapped in a
    ///    `Result`.
    /// 2. Independent, i.e. the function _does not_ receive an
    ///    environment argument.
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

    /// Creates a new `Function` that is:
    ///
    /// 1. Dynamic/Polymorphic, i.e. all inputs are received in a
    ///    slice of `Val` (the set of all Wasm values), and all
    ///    outputs are stored in a vector of `Val`, wrapped in a
    ///    `Result`.
    /// 2. Dependent, i.e. the function _does_ receive an environment
    ///    argument (given by `env`).
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

/// This private inner module contains the low-level implementation
/// for `Function` and its siblings.
mod inner {
    use std::convert::Infallible;
    use std::error::Error;
    use std::marker::PhantomData;
    use std::panic::{self, AssertUnwindSafe};
    use wasm_common::{FunctionType, NativeWasmType, Type};
    use wasmer_runtime::{raise_user_trap, resume_panic, VMFunctionBody};

    /// A trait to represent a wasm extern type.
    pub unsafe trait WasmExternType: Copy
    where
        Self: Sized,
    {
        /// Native wasm type for this `WasmExternType`.
        type Native: NativeWasmType;

        /// Convert from given `Native` type to self.
        fn from_native(native: Self::Native) -> Self;

        /// Convert self to `Native` type.
        fn to_native(self) -> Self::Native;
    }

    macro_rules! wasm_extern_type {
        ( $( $type:ty => $native_type:ty ),* ) => {
            $(
                #[allow(clippy::use_self)]
                unsafe impl WasmExternType for $type {
                    type Native = $native_type;

                    #[inline]
                    fn from_native(native: Self::Native) -> Self {
                        native as _
                    }

                    #[inline]
                    fn to_native(self) -> Self::Native {
                        self as _
                    }
                }
            )*
        };
    }

    wasm_extern_type!(
        i8 => i32,
        u8 => i32,
        i16 => i32,
        u16 => i32,
        i32 => i32,
        u32 => i32,
        i64 => i64,
        u64 => i64,
        f32 => f32,
        f64 => f64
    );

    /// The `WasmTypeList` trait represents a tuple (list) of Wasm
    /// typed values. It is used to get low-level representation of
    /// such a tuple.
    pub trait WasmTypeList {
        /// The C type (a struct) that can hold/represent all the
        /// represented values.
        type CStruct;

        /// The array type that can hold all the represented values.
        ///
        /// Note that all values are stored in their binary form.
        type Array: AsMut<[i128]>;

        /// Constructs `Self` based on an array of values.
        fn from_array(array: Self::Array) -> Self;

        /// Builds and returns an array of type `Array` from a tuple
        /// (list) of values.
        fn into_array(self) -> Self::Array;

        /// Allocates and return an empty array of type `Array` that
        /// will hold a tuple (list) of values, usually to hold the
        /// returned values of a WebAssembly function call.
        fn empty_array() -> Self::Array;

        /// Builds a tuple (list) of values from a C struct of type
        /// `CStruct`.
        fn from_c_struct(c_struct: Self::CStruct) -> Self;

        /// Builds and returns a C struct of type `CStruct` from a
        /// tuple (list) of values.
        fn into_c_struct(self) -> Self::CStruct;

        /// Get the Wasm types for the tuple (list) of currently
        /// represented values.
        fn wasm_types() -> &'static [Type];
    }

    /// The `IntoResult` trait turns a `WasmTypeList` into a
    /// `Result<WasmTypeList, Self::Error>`.
    ///
    /// It is mostly used to turn result values of a Wasm function
    /// call into a `Result`.
    pub trait IntoResult<T>
    where
        T: WasmTypeList,
    {
        /// The error type for this trait.
        type Error: Error + Sync + Send + 'static;

        /// Transforms `Self` into a `Result`.
        fn into_result(self) -> Result<T, Self::Error>;
    }

    impl<T> IntoResult<T> for T
    where
        T: WasmTypeList,
    {
        // `T` is not a `Result`, it's already a value, so no error
        // can be built.
        type Error = Infallible;

        fn into_result(self) -> Result<Self, Infallible> {
            Ok(self)
        }
    }

    impl<T, E> IntoResult<T> for Result<T, E>
    where
        T: WasmTypeList,
        E: Error + Sync + Send + 'static,
    {
        type Error = E;

        fn into_result(self) -> Self {
            self
        }
    }

    /// The `HostFunction` trait represents the set of functions that
    /// can be used as host function. To uphold this statement, it is
    /// necessary for a function to be transformed into a pointer to
    /// `VMFunctionBody`.
    pub trait HostFunction<Args, Rets, Kind, T>
    where
        Args: WasmTypeList,
        Rets: WasmTypeList,
        Kind: HostFunctionKind,
        T: Sized,
        Self: Sized,
    {
        /// Get the pointer to the function body.
        fn function_body_ptr(self) -> *const VMFunctionBody;
    }

    /// Empty trait to specify the kind of `HostFunction`: With or
    /// without an environment.
    ///
    /// This trait is never aimed to be used by a user. It is used by
    /// the trait system to automatically generate the appropriate
    /// host functions.
    #[doc(hidden)]
    pub trait HostFunctionKind {}

    /// An empty struct to help Rust typing to determine
    /// when a `HostFunction` does have an environment.
    pub struct WithEnv;

    impl HostFunctionKind for WithEnv {}

    /// An empty struct to help Rust typing to determine
    /// when a `HostFunction` does not have an environment.
    pub struct WithoutEnv;

    impl HostFunctionKind for WithoutEnv {}

    /// Represents a low-level Wasm static host function. See
    /// `super::Function::new` and `super::Function::new_env` to learn
    /// more.
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct Function<Args = (), Rets = ()> {
        address: *const VMFunctionBody,
        _phantom: PhantomData<(Args, Rets)>,
    }

    unsafe impl<Args, Rets> Send for Function<Args, Rets> {}

    impl<Args, Rets> Function<Args, Rets>
    where
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        /// Creates a new `Function`.
        pub fn new<F, T, E>(function: F) -> Self
        where
            F: HostFunction<Args, Rets, T, E>,
            T: HostFunctionKind,
            E: Sized,
        {
            Self {
                address: function.function_body_ptr(),
                _phantom: PhantomData,
            }
        }

        /// Get the function type of this `Function`.
        pub fn ty(&self) -> FunctionType {
            FunctionType::new(Args::wasm_types(), Rets::wasm_types())
        }

        /// Get the address of this `Function`.
        pub fn address(&self) -> *const VMFunctionBody {
            self.address
        }
    }

    macro_rules! impl_host_function {
        ( [$c_struct_representation:ident]
           $c_struct_name:ident,
           $( $x:ident ),* ) => {

            /// A structure with a C-compatible representation that can hold a set of Wasm values.
            /// This type is used by `WasmTypeList::CStruct`.
            #[repr($c_struct_representation)]
            pub struct $c_struct_name< $( $x ),* > ( $( <$x as WasmExternType>::Native ),* )
            where
                $( $x: WasmExternType ),*;

            // Implement `WasmTypeList` for a specific tuple.
            #[allow(unused_parens, dead_code)]
            impl< $( $x ),* >
                WasmTypeList
            for
                ( $( $x ),* )
            where
                $( $x: WasmExternType ),*
            {
                type CStruct = $c_struct_name< $( $x ),* >;

                type Array = [i128; count_idents!( $( $x ),* )];

                fn from_array(array: Self::Array) -> Self {
                    // Unpack items of the array.
                    #[allow(non_snake_case)]
                    let [ $( $x ),* ] = array;

                    // Build the tuple.
                    (
                        $(
                            WasmExternType::from_native(NativeWasmType::from_binary($x))
                        ),*
                    )
                }

                fn into_array(self) -> Self::Array {
                    // Unpack items of the tuple.
                    #[allow(non_snake_case)]
                    let ( $( $x ),* ) = self;

                    // Build the array.
                    [
                        $(
                            WasmExternType::to_native($x).to_binary()
                        ),*
                    ]
                }

                fn empty_array() -> Self::Array {
                    // Build an array initialized with `0`.
                    [0; count_idents!( $( $x ),* )]
                }

                fn from_c_struct(c_struct: Self::CStruct) -> Self {
                    // Unpack items of the C structure.
                    #[allow(non_snake_case)]
                    let $c_struct_name( $( $x ),* ) = c_struct;

                    (
                        $(
                            WasmExternType::from_native($x)
                        ),*
                    )
                }

                #[allow(unused_parens, non_snake_case)]
                fn into_c_struct(self) -> Self::CStruct {
                    // Unpack items of the tuple.
                    let ( $( $x ),* ) = self;

                    // Build the C structure.
                    $c_struct_name(
                        $(
                            WasmExternType::to_native($x)
                        ),*
                    )
                }

                fn wasm_types() -> &'static [Type] {
                    &[
                        $(
                            $x::Native::WASM_TYPE
                        ),*
                    ]
                }
            }

            // Implement `HostFunction` for a function that has the same arity than the tuple.
            // This specific function has no environment.
            #[allow(unused_parens)]
            impl< $( $x, )* Rets, RetsAsResult, Func >
                HostFunction<( $( $x ),* ), Rets, WithoutEnv, ()>
            for
                Func
            where
                $( $x: WasmExternType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Func: Fn($( $x , )*) -> RetsAsResult + 'static + Send,
            {
                #[allow(non_snake_case)]
                fn function_body_ptr(self) -> *const VMFunctionBody {
                    /// This is a function that wraps the real host
                    /// function. Its address will be used inside the
                    /// runtime.
                    extern fn func_wrapper<$( $x, )* Rets, RetsAsResult, Func>( _: usize, $($x: $x::Native, )* ) -> Rets::CStruct
                    where
                        $( $x: WasmExternType, )*
                        Rets: WasmTypeList,
                        RetsAsResult: IntoResult<Rets>,
                        Func: Fn( $( $x ),* ) -> RetsAsResult + 'static
                    {
                        let func: &Func = unsafe { &*(&() as *const () as *const Func) };
                        let result = panic::catch_unwind(AssertUnwindSafe(|| {
                            func( $( WasmExternType::from_native($x) ),* ).into_result()
                        }));

                        match result {
                            Ok(Ok(result)) => return result.into_c_struct(),
                            Ok(Err(trap)) => unsafe { raise_user_trap(Box::new(trap)) },
                            Err(panic) => unsafe { resume_panic(panic) },
                        }
                    }

                    func_wrapper::<$( $x, )* Rets, RetsAsResult, Self> as *const VMFunctionBody
                }
            }

            #[allow(unused_parens)]
            impl< $( $x, )* Rets, RetsAsResult, Env, Func >
                HostFunction<( $( $x ),* ), Rets, WithEnv, Env>
            for
                Func
            where
                $( $x: WasmExternType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Env: Sized,
                Func: Fn(&mut Env, $( $x , )*) -> RetsAsResult + Send + 'static,
            {
                #[allow(non_snake_case)]
                fn function_body_ptr(self) -> *const VMFunctionBody {
                    /// This is a function that wraps the real host
                    /// function. Its address will be used inside the
                    /// runtime.
                    extern fn func_wrapper<$( $x, )* Rets, RetsAsResult, Env, Func>( env: &mut Env, $( $x: $x::Native, )* ) -> Rets::CStruct
                    where
                        $( $x: WasmExternType, )*
                        Rets: WasmTypeList,
                        RetsAsResult: IntoResult<Rets>,
                        Env: Sized,
                        Func: Fn(&mut Env, $( $x ),* ) -> RetsAsResult + 'static
                    {
                        let func: &Func = unsafe { &*(&() as *const () as *const Func) };

                        let result = panic::catch_unwind(AssertUnwindSafe(|| {
                            func(env, $( WasmExternType::from_native($x) ),* ).into_result()
                        }));

                        match result {
                            Ok(Ok(result)) => return result.into_c_struct(),
                            Ok(Err(trap)) => unsafe { raise_user_trap(Box::new(trap)) },
                            Err(panic) => unsafe { resume_panic(panic) },
                        }
                    }

                    func_wrapper::<$( $x, )* Rets, RetsAsResult, Env, Self> as *const VMFunctionBody
                }
            }

        };
    }

    // Black-magic to count the number of identifiers at compile-time.
    macro_rules! count_idents {
        ( $($idents:ident),* ) => {
            {
                #[allow(dead_code, non_camel_case_types)]
                enum Idents { $( $idents, )* __CountIdentsLast }
                const COUNT: usize = Idents::__CountIdentsLast as usize;
                COUNT
            }
        };
    }

    // Here we go! Let's generate all the C struct, `WasmTypeList`
    // implementations and `HostFunction` implementations.
    impl_host_function!([C] S0,);
    impl_host_function!([transparent] S1, A1);
    impl_host_function!([C] S2, A1, A2);
    impl_host_function!([C] S3, A1, A2, A3);
    impl_host_function!([C] S4, A1, A2, A3, A4);
    impl_host_function!([C] S5, A1, A2, A3, A4, A5);
    impl_host_function!([C] S6, A1, A2, A3, A4, A5, A6);
    impl_host_function!([C] S7, A1, A2, A3, A4, A5, A6, A7);
    impl_host_function!([C] S8, A1, A2, A3, A4, A5, A6, A7, A8);
    impl_host_function!([C] S9, A1, A2, A3, A4, A5, A6, A7, A8, A9);
    impl_host_function!([C] S10, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
    impl_host_function!([C] S11, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
    impl_host_function!([C] S12, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
    impl_host_function!([C] S13, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
    impl_host_function!([C] S14, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
    impl_host_function!([C] S15, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15);
    impl_host_function!([C] S16, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16);
    impl_host_function!([C] S17, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17);
    impl_host_function!([C] S18, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18);
    impl_host_function!([C] S19, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19);
    impl_host_function!([C] S20, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20);
    impl_host_function!([C] S21, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21);
    impl_host_function!([C] S22, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22);
    impl_host_function!([C] S23, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23);
    impl_host_function!([C] S24, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24);
    impl_host_function!([C] S25, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25);
    impl_host_function!([C] S26, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25, A26);

    // Implement `WasmTypeList` on `Infallible`, which means that
    // `Infallible` can be used as a returned type of a host function
    // to express that it doesn't return.
    impl WasmTypeList for Infallible {
        type CStruct = Self;
        type Array = [i128; 0];

        fn from_array(_: Self::Array) -> Self {
            unreachable!()
        }

        fn into_array(self) -> Self::Array {
            []
        }

        fn empty_array() -> Self::Array {
            unreachable!()
        }

        fn from_c_struct(_: Self::CStruct) -> Self {
            unreachable!()
        }

        fn into_c_struct(self) -> Self::CStruct {
            unreachable!()
        }

        fn wasm_types() -> &'static [Type] {
            &[]
        }
    }

    #[cfg(test)]
    mod test_wasm_type_list {
        use super::*;
        use wasm_common::Type;

        #[test]
        fn test_from_array() {
            assert_eq!(<()>::from_array([]), ());
            assert_eq!(<i32>::from_array([1]), (1i32));
            assert_eq!(<(i32, i64)>::from_array([1, 2]), (1i32, 2i64));
            assert_eq!(
                <(i32, i64, f32, f64)>::from_array([
                    1,
                    2,
                    (3.1f32).to_bits().into(),
                    (4.2f64).to_bits().into()
                ]),
                (1, 2, 3.1f32, 4.2f64)
            );
        }

        #[test]
        fn test_into_array() {
            assert_eq!(().into_array(), []);
            assert_eq!((1).into_array(), [1]);
            assert_eq!((1i32, 2i64).into_array(), [1, 2]);
            assert_eq!(
                (1i32, 2i32, 3.1f32, 4.2f64).into_array(),
                [1, 2, (3.1f32).to_bits().into(), (4.2f64).to_bits().into()]
            );
        }

        #[test]
        fn test_empty_array() {
            assert_eq!(<()>::empty_array().len(), 0);
            assert_eq!(<i32>::empty_array().len(), 1);
            assert_eq!(<(i32, i64)>::empty_array().len(), 2);
        }

        #[test]
        fn test_from_c_struct() {
            assert_eq!(<()>::from_c_struct(S0()), ());
            assert_eq!(<i32>::from_c_struct(S1(1)), (1i32));
            assert_eq!(<(i32, i64)>::from_c_struct(S2(1, 2)), (1i32, 2i64));
            assert_eq!(
                <(i32, i64, f32, f64)>::from_c_struct(S4(1, 2, 3.1, 4.2)),
                (1i32, 2i64, 3.1f32, 4.2f64)
            );
        }

        #[test]
        fn test_wasm_types_for_uni_values() {
            assert_eq!(<i32>::wasm_types(), [Type::I32]);
            assert_eq!(<i64>::wasm_types(), [Type::I64]);
            assert_eq!(<f32>::wasm_types(), [Type::F32]);
            assert_eq!(<f64>::wasm_types(), [Type::F64]);
        }

        #[test]
        fn test_wasm_types_for_multi_values() {
            assert_eq!(<(i32, i32)>::wasm_types(), [Type::I32, Type::I32]);
            assert_eq!(<(i64, i64)>::wasm_types(), [Type::I64, Type::I64]);
            assert_eq!(<(f32, f32)>::wasm_types(), [Type::F32, Type::F32]);
            assert_eq!(<(f64, f64)>::wasm_types(), [Type::F64, Type::F64]);

            assert_eq!(
                <(i32, i64, f32, f64)>::wasm_types(),
                [Type::I32, Type::I64, Type::F32, Type::F64]
            );
        }
    }

    #[allow(non_snake_case)]
    #[cfg(test)]
    mod test_function {
        use super::*;
        use wasm_common::Type;

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
            assert_eq!(Function::new(func).ty(), FunctionType::new(vec![], vec![]));
            assert_eq!(
                Function::new(func__i32).ty(),
                FunctionType::new(vec![], vec![Type::I32])
            );
            assert_eq!(
                Function::new(func_i32).ty(),
                FunctionType::new(vec![Type::I32], vec![])
            );
            assert_eq!(
                Function::new(func_i32__i32).ty(),
                FunctionType::new(vec![Type::I32], vec![Type::I32])
            );
            assert_eq!(
                Function::new(func_i32_i32__i32).ty(),
                FunctionType::new(vec![Type::I32, Type::I32], vec![Type::I32])
            );
            assert_eq!(
                Function::new(func_i32_i32__i32_i32).ty(),
                FunctionType::new(vec![Type::I32, Type::I32], vec![Type::I32, Type::I32])
            );
            assert_eq!(
                Function::new(func_f32_i32__i32_f32).ty(),
                FunctionType::new(vec![Type::F32, Type::I32], vec![Type::I32, Type::F32])
            );
        }

        #[test]
        fn test_function_pointer() {
            let f = Function::new(func_i32__i32);
            let function = unsafe { std::mem::transmute::<_, fn(usize, i32) -> i32>(f.address) };
            assert_eq!(function(0, 3), 6);
        }
    }
}
