/// Helpers for interacting with the zig that generates bitcode
use crate::debug_info_init;
use crate::llvm::build::{Env, C_CALL_CONV, FAST_CALL_CONV};
use crate::llvm::convert::basic_type_from_layout;
use crate::llvm::refcounting::{
    decrement_refcount_layout, increment_n_refcount_layout, increment_refcount_layout,
};
use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::types::{BasicType, BasicTypeEnum};
use inkwell::values::{BasicValue, BasicValueEnum, CallSiteValue, FunctionValue, InstructionValue};
use inkwell::AddressSpace;
use roc_module::symbol::Symbol;
use roc_mono::layout::{Layout, LayoutIds};

pub fn call_bitcode_fn<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    args: &[BasicValueEnum<'ctx>],
    fn_name: &str,
) -> BasicValueEnum<'ctx> {
    call_bitcode_fn_help(env, args, fn_name)
        .try_as_basic_value()
        .left()
        .unwrap_or_else(|| {
            panic!(
                "LLVM error: Did not get return value from bitcode function {:?}",
                fn_name
            )
        })
}

pub fn call_void_bitcode_fn<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    args: &[BasicValueEnum<'ctx>],
    fn_name: &str,
) -> InstructionValue<'ctx> {
    call_bitcode_fn_help(env, args, fn_name)
        .try_as_basic_value()
        .right()
        .unwrap_or_else(|| panic!("LLVM error: Tried to call void bitcode function, but got return value from bitcode function, {:?}", fn_name))
}

fn call_bitcode_fn_help<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    args: &[BasicValueEnum<'ctx>],
    fn_name: &str,
) -> CallSiteValue<'ctx> {
    let fn_val = env
        .module
        .get_function(fn_name)
        .unwrap_or_else(|| panic!("Unrecognized builtin function: {:?} - if you're working on the Roc compiler, do you need to rebuild the bitcode? See compiler/builtins/bitcode/README.md", fn_name));

    let call = env.builder.build_call(fn_val, args, "call_builtin");

    call.set_call_convention(fn_val.get_call_conventions());
    call
}

const ARGUMENT_SYMBOLS: [Symbol; 8] = [
    Symbol::ARG_1,
    Symbol::ARG_2,
    Symbol::ARG_3,
    Symbol::ARG_4,
    Symbol::ARG_5,
    Symbol::ARG_6,
    Symbol::ARG_7,
    Symbol::ARG_8,
];

pub fn build_transform_caller<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    function: FunctionValue<'ctx>,
    closure_data_layout: Layout<'a>,
    argument_layouts: &[Layout<'a>],
) -> FunctionValue<'ctx> {
    let fn_name: &str = &format!(
        "{}_zig_function_caller",
        function.get_name().to_string_lossy()
    );

    match env.module.get_function(fn_name) {
        Some(function_value) => function_value,
        None => build_transform_caller_help(
            env,
            function,
            closure_data_layout,
            argument_layouts,
            &fn_name,
        ),
    }
}

fn build_transform_caller_help<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    roc_function: FunctionValue<'ctx>,
    closure_data_layout: Layout<'a>,
    argument_layouts: &[Layout<'a>],
    fn_name: &str,
) -> FunctionValue<'ctx> {
    debug_assert!(argument_layouts.len() <= 7);

    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

    let function_value = crate::llvm::refcounting::build_header_help(
        env,
        &fn_name,
        env.context.void_type().into(),
        &(bumpalo::vec![ in env.arena; BasicTypeEnum::PointerType(arg_type); argument_layouts.len() + 2 ]),
    );

    let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
    debug_assert!(kind_id > 0);
    let attr = env.context.create_enum_attribute(kind_id, 1);
    function_value.add_attribute(AttributeLoc::Function, attr);

    let entry = env.context.append_basic_block(function_value, "entry");
    env.builder.position_at_end(entry);

    debug_info_init!(env, function_value);

    let mut it = function_value.get_param_iter();
    let closure_ptr = it.next().unwrap().into_pointer_value();
    closure_ptr.set_name(Symbol::ARG_1.ident_string(&env.interns));

    let arguments =
        bumpalo::collections::Vec::from_iter_in(it.take(argument_layouts.len()), env.arena);

    for (argument, name) in arguments.iter().zip(ARGUMENT_SYMBOLS[1..].iter()) {
        argument.set_name(name.ident_string(&env.interns));
    }

    let mut arguments_cast =
        bumpalo::collections::Vec::with_capacity_in(arguments.len(), env.arena);

    for (argument_ptr, layout) in arguments.iter().zip(argument_layouts) {
        let basic_type = basic_type_from_layout(env, layout).ptr_type(AddressSpace::Generic);

        let argument_cast = env
            .builder
            .build_bitcast(*argument_ptr, basic_type, "load_opaque")
            .into_pointer_value();

        let argument = env.builder.build_load(argument_cast, "load_opaque");

        arguments_cast.push(argument);
    }

    match closure_data_layout {
        Layout::FunctionPointer(_, _) => {
            // do nothing
        }
        Layout::Closure(_, lambda_set, _) => {
            if let Layout::Struct(&[]) = lambda_set.runtime_representation() {
                // do nothing
            } else {
                let closure_type =
                    basic_type_from_layout(env, &lambda_set.runtime_representation())
                        .ptr_type(AddressSpace::Generic);

                let closure_cast = env
                    .builder
                    .build_bitcast(closure_ptr, closure_type, "load_opaque")
                    .into_pointer_value();

                let closure_data = env.builder.build_load(closure_cast, "load_opaque");

                arguments_cast.push(closure_data);
            }
        }
        Layout::Struct([Layout::Closure(_, lambda_set, _)]) => {
            // a case required for Set.walk; may be able to remove when we can define builtins in
            // terms of other builtins in the right way (using their function symbols instead of
            // hacking with lowlevel ops).
            let closure_type = basic_type_from_layout(
                env,
                &Layout::Struct(&[lambda_set.runtime_representation()]),
            )
            .ptr_type(AddressSpace::Generic);

            let closure_cast = env
                .builder
                .build_bitcast(closure_ptr, closure_type, "load_opaque")
                .into_pointer_value();

            let closure_data = env.builder.build_load(closure_cast, "load_opaque");

            arguments_cast.push(closure_data);
        }
        Layout::Struct([]) => {
            // do nothing, should try to remove this case later
        }
        Layout::Struct(_) => {
            // do nothing, should try to remove this case later
        }
        other => unreachable!("layout is not valid for a closure: {:?}", other),
    }

    let call = {
        env.builder
            .build_call(roc_function, arguments_cast.as_slice(), "tmp")
    };

    call.set_call_convention(FAST_CALL_CONV);

    let result = call
        .try_as_basic_value()
        .left()
        .unwrap_or_else(|| panic!("LLVM error: Invalid call by pointer."));

    let result_u8_ptr = function_value
        .get_nth_param(argument_layouts.len() as u32 + 1)
        .unwrap();
    let result_ptr = env
        .builder
        .build_bitcast(
            result_u8_ptr,
            result.get_type().ptr_type(AddressSpace::Generic),
            "write_result",
        )
        .into_pointer_value();

    env.builder.build_store(result_ptr, result);
    env.builder.build_return(None);

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}

enum Mode {
    Inc,
    IncN,
    Dec,
}

/// a functin that accepts two arguments: the value to increment, and an amount to increment by
pub fn build_inc_n_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    build_rc_wrapper(env, layout_ids, layout, Mode::IncN)
}

/// a functin that accepts two arguments: the value to increment; increments by 1
pub fn build_inc_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    build_rc_wrapper(env, layout_ids, layout, Mode::Inc)
}

pub fn build_dec_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    build_rc_wrapper(env, layout_ids, layout, Mode::Dec)
}

fn build_rc_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
    rc_operation: Mode,
) -> FunctionValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_RC_REF;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let fn_name = match rc_operation {
        Mode::IncN => format!("{}_inc_n", fn_name),
        Mode::Inc => format!("{}_inc", fn_name),
        Mode::Dec => format!("{}_dec", fn_name),
    };

    let function_value = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

            let function_value = match rc_operation {
                Mode::Inc | Mode::Dec => crate::llvm::refcounting::build_header_help(
                    env,
                    &fn_name,
                    env.context.void_type().into(),
                    &[arg_type.into()],
                ),
                Mode::IncN => crate::llvm::refcounting::build_header_help(
                    env,
                    &fn_name,
                    env.context.void_type().into(),
                    &[arg_type.into(), env.ptr_int().into()],
                ),
            };

            let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
            debug_assert!(kind_id > 0);
            let attr = env.context.create_enum_attribute(kind_id, 1);
            function_value.add_attribute(AttributeLoc::Function, attr);

            let entry = env.context.append_basic_block(function_value, "entry");
            env.builder.position_at_end(entry);

            debug_info_init!(env, function_value);

            let mut it = function_value.get_param_iter();
            let value_ptr = it.next().unwrap().into_pointer_value();

            value_ptr.set_name(Symbol::ARG_1.ident_string(&env.interns));

            let value_type = basic_type_from_layout(env, layout).ptr_type(AddressSpace::Generic);

            let value_cast = env
                .builder
                .build_bitcast(value_ptr, value_type, "load_opaque")
                .into_pointer_value();

            let value = env.builder.build_load(value_cast, "load_opaque");

            match rc_operation {
                Mode::Inc => {
                    let n = 1;
                    increment_refcount_layout(env, function_value, layout_ids, n, value, layout);
                }
                Mode::IncN => {
                    let n = it.next().unwrap().into_int_value();
                    n.set_name(Symbol::ARG_2.ident_string(&env.interns));

                    increment_n_refcount_layout(env, function_value, layout_ids, n, value, layout);
                }
                Mode::Dec => {
                    decrement_refcount_layout(env, function_value, layout_ids, value, layout);
                }
            }

            env.builder.build_return(None);

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}

pub fn build_eq_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_EQ_REF;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let function_value = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                env.context.bool_type().into(),
                &[arg_type.into(), arg_type.into()],
            );

            let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
            debug_assert!(kind_id > 0);
            let attr = env.context.create_enum_attribute(kind_id, 1);
            function_value.add_attribute(AttributeLoc::Function, attr);

            let entry = env.context.append_basic_block(function_value, "entry");
            env.builder.position_at_end(entry);

            debug_info_init!(env, function_value);

            let mut it = function_value.get_param_iter();
            let value_ptr1 = it.next().unwrap().into_pointer_value();
            let value_ptr2 = it.next().unwrap().into_pointer_value();

            value_ptr1.set_name(Symbol::ARG_1.ident_string(&env.interns));
            value_ptr2.set_name(Symbol::ARG_2.ident_string(&env.interns));

            let value_type = basic_type_from_layout(env, layout).ptr_type(AddressSpace::Generic);

            let value_cast1 = env
                .builder
                .build_bitcast(value_ptr1, value_type, "load_opaque")
                .into_pointer_value();

            let value_cast2 = env
                .builder
                .build_bitcast(value_ptr2, value_type, "load_opaque")
                .into_pointer_value();

            let value1 = env.builder.build_load(value_cast1, "load_opaque");
            let value2 = env.builder.build_load(value_cast2, "load_opaque");

            let result =
                crate::llvm::compare::generic_eq(env, layout_ids, value1, value2, layout, layout);

            env.builder.build_return(Some(&result));

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}

pub fn build_compare_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    roc_function: FunctionValue<'ctx>,
    closure_data_layout: Layout<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let fn_name: &str = &format!(
        "{}_compare_wrapper",
        roc_function.get_name().to_string_lossy()
    );

    let function_value = match env.module.get_function(fn_name) {
        Some(function_value) => function_value,
        None => {
            let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                env.context.i8_type().into(),
                &[arg_type.into(), arg_type.into(), arg_type.into()],
            );

            // we expose this function to zig; must use c calling convention
            function_value.set_call_conventions(C_CALL_CONV);

            let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
            debug_assert!(kind_id > 0);
            let attr = env.context.create_enum_attribute(kind_id, 1);
            function_value.add_attribute(AttributeLoc::Function, attr);

            let entry = env.context.append_basic_block(function_value, "entry");
            env.builder.position_at_end(entry);

            debug_info_init!(env, function_value);

            let mut it = function_value.get_param_iter();
            let closure_ptr = it.next().unwrap().into_pointer_value();
            let value_ptr1 = it.next().unwrap().into_pointer_value();
            let value_ptr2 = it.next().unwrap().into_pointer_value();

            closure_ptr.set_name(Symbol::ARG_1.ident_string(&env.interns));
            value_ptr1.set_name(Symbol::ARG_2.ident_string(&env.interns));
            value_ptr2.set_name(Symbol::ARG_3.ident_string(&env.interns));

            let value_type = basic_type_from_layout(env, layout);
            let value_ptr_type = value_type.ptr_type(AddressSpace::Generic);

            let value_cast1 = env
                .builder
                .build_bitcast(value_ptr1, value_ptr_type, "load_opaque")
                .into_pointer_value();

            let value_cast2 = env
                .builder
                .build_bitcast(value_ptr2, value_ptr_type, "load_opaque")
                .into_pointer_value();

            let value1 = env.builder.build_load(value_cast1, "load_opaque");
            let value2 = env.builder.build_load(value_cast2, "load_opaque");

            let default = [value1, value2];

            let arguments_cast = match closure_data_layout {
                Layout::FunctionPointer(_, _) => &default,
                Layout::Closure(_, lambda_set, _) => {
                    if let Layout::Struct(&[]) = lambda_set.runtime_representation() {
                        &default
                    } else {
                        let closure_type =
                            basic_type_from_layout(env, &lambda_set.runtime_representation())
                                .ptr_type(AddressSpace::Generic);

                        let closure_cast = env
                            .builder
                            .build_bitcast(closure_ptr, closure_type, "load_opaque")
                            .into_pointer_value();

                        let closure_data = env.builder.build_load(closure_cast, "load_opaque");

                        env.arena.alloc([value1, value2, closure_data]) as &[_]
                    }
                }
                Layout::Struct([]) => &default,
                other => unreachable!("layout is not valid for a closure: {:?}", other),
            };

            let call = env.builder.build_call(
                roc_function,
                arguments_cast,
                "call_user_defined_compare_function",
            );

            let result = call.try_as_basic_value().left().unwrap();

            // IMPORTANT! we call a user function, so it has the fast calling convention
            call.set_call_convention(FAST_CALL_CONV);

            env.builder.build_return(Some(&result));

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}