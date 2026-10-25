//! Expression evaluation against the shared runtime ABI.
//!
//! Every operand is evaluated left to right and parked in the frame's rooted
//! scratch stack before the next operand runs. Argument buffers are additional
//! registered scopes for the duration of a single runtime call; the elements
//! themselves are already reachable through the scratch/frame roots.

use pyx_ir::{
    BoolOp, Callee, CompClause, CompTarget, Const, Expr, ExprKind, FunctionId, UnpackTarget,
};
use pyx_runtime::abi;
use pyx_runtime::host;

use crate::frame::Signal;
use crate::Interp;

#[derive(Clone, Copy)]
enum LiteralKind {
    List,
    Tuple,
    Set,
}

/// Registers `values` as a precise root scope for one runtime call.
fn scoped<T>(values: &[i64], call: impl FnOnce() -> T) -> T {
    let token = abi::pyx_gc_scope_push(values.as_ptr(), values.len());
    let result = call();
    abi::pyx_gc_scope_pop(token);
    result
}

fn bytes(text: &str) -> (*const u8, usize) {
    (text.as_ptr(), text.len())
}

impl Interp {
    pub(crate) fn truthy(&self, value: i64) -> bool {
        abi::pyx_truthy(value) != 0
    }

    pub(crate) fn eval(&mut self, expr: &Expr) -> Result<i64, Signal> {
        match &expr.kind {
            ExprKind::Const(value) => self.eval_const(value),
            ExprKind::Local(id) => Ok(self.local(*id)),
            ExprKind::CellGet(id) => {
                let cell = self.local(*id);
                let value = abi::pyx_cell_get(cell);
                self.check(value)
            }
            ExprKind::CellSet { target, value } => {
                let value = self.eval(value)?;
                self.scratch_push(value);
                let cell = self.local(*target);
                abi::pyx_cell_set(cell, value);
                Ok(value)
            }
            ExprKind::NamedExpr { target, value } => {
                let value = self.eval(value)?;
                self.set_local(*target, value);
                Ok(value)
            }
            ExprKind::StrBuilderNew { init } => {
                let init = self.eval(init)?;
                self.scratch_push(init);
                let builder = abi::pyx_str_builder_new(init);
                self.check(builder)
            }
            ExprKind::StrBuilderAppend { builder, value } => {
                let value = self.eval(value)?;
                self.scratch_push(value);
                let builder = self.eval(builder)?;
                self.scratch_push(builder);
                let result = abi::pyx_str_builder_append(builder, value);
                self.check(result)
            }
            ExprKind::StrBuilderFinish { builder } => {
                let builder = self.eval(builder)?;
                self.scratch_push(builder);
                let result = abi::pyx_str_builder_finish(builder);
                self.check(result)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let mark = self.scratch_mark();
                let lhs_value = self.eval(lhs)?;
                self.scratch_push(lhs_value);
                let rhs_value = self.eval(rhs)?;
                self.scratch_push(rhs_value);
                let result = abi::pyx_binop(op.code(), lhs_value, rhs_value);
                self.scratch_release(mark);
                self.check(result)
            }
            ExprKind::Un { op, operand } => {
                let value = self.eval(operand)?;
                self.scratch_push(value);
                let result = abi::pyx_unop(op.code(), value);
                self.check(result)
            }
            ExprKind::BoolOp { op, lhs, rhs } => {
                let value = self.eval(lhs)?;
                self.scratch_push(value);
                let truth = self.truthy(value);
                let take_rhs = match op {
                    BoolOp::And => truth,
                    BoolOp::Or => !truth,
                };
                if take_rhs {
                    self.eval(rhs)
                } else {
                    Ok(value)
                }
            }
            ExprKind::IfExp { test, then, orelse } => {
                let value = self.eval(test)?;
                if self.truthy(value) {
                    self.eval(then)
                } else {
                    self.eval(orelse)
                }
            }
            ExprKind::List(items) => self.eval_literal(items, LiteralKind::List),
            ExprKind::Tuple(items) => self.eval_literal(items, LiteralKind::Tuple),
            ExprKind::Set(items) => self.eval_literal(items, LiteralKind::Set),
            ExprKind::Dict(pairs) => self.eval_dict(pairs),
            ExprKind::Subscript { object, index } => {
                let mark = self.scratch_mark();
                let object = self.eval(object)?;
                self.scratch_push(object);
                let index = self.eval(index)?;
                self.scratch_push(index);
                let result = abi::pyx_subscript(object, index);
                self.scratch_release(mark);
                self.check(result)
            }
            ExprKind::Slice {
                object,
                start,
                stop,
                step,
            } => {
                let mark = self.scratch_mark();
                let object = self.eval(object)?;
                self.scratch_push(object);
                let start = self.eval_optional(start)?;
                self.scratch_push(start);
                let stop = self.eval_optional(stop)?;
                self.scratch_push(stop);
                let step = self.eval_optional(step)?;
                self.scratch_push(step);
                let result = abi::pyx_slice(object, start, stop, step);
                self.scratch_release(mark);
                self.check(result)
            }
            ExprKind::FunctionRef(id) => self.make_function(*id, Vec::new()),
            ExprKind::BuiltinRef(builtin) => {
                let (ptr, len) = bytes(builtin.name());
                let value = abi::pyx_builtin_function(ptr, len);
                self.check(value)
            }
            ExprKind::ExceptionClass(kind) => {
                let value = abi::pyx_exception_class(kind.tag());
                self.check(value)
            }
            ExprKind::ModuleCall {
                module,
                function,
                args,
                keywords,
            } => self.eval_module_call(module, function, args, keywords),
            ExprKind::ModuleRef { module } => {
                let (ptr, len) = bytes(module);
                let value = abi::pyx_module_value(ptr, len);
                self.check(value)
            }
            ExprKind::DeferredImport { module, member } => {
                let (module_ptr, module_len) = bytes(module);
                let (member_ptr, member_len) = bytes(member.as_deref().unwrap_or(""));
                let value =
                    abi::pyx_deferred_import(module_ptr, module_len, member_ptr, member_len);
                self.check(value)
            }
            ExprKind::ModuleAttr { module, name } => {
                let (module_ptr, module_len) = bytes(module);
                let (name_ptr, name_len) = bytes(name);
                let value = abi::pyx_module_get(module_ptr, module_len, name_ptr, name_len);
                self.check(value)
            }
            ExprKind::GlobalGet { module, name } => {
                let (module_ptr, module_len) = bytes(module);
                let (name_ptr, name_len) = bytes(name);
                let value = abi::pyx_global_get(module_ptr, module_len, name_ptr, name_len);
                self.check(value)
            }
            ExprKind::ExtensionAttr { path, member } => {
                let (path_ptr, path_len) = bytes(path);
                let (member_ptr, member_len) = bytes(member);
                let value = abi::pyx_extension_get(path_ptr, path_len, member_ptr, member_len);
                self.check(value)
            }
            ExprKind::GetAttr { object, name } => {
                let object = self.eval(object)?;
                self.scratch_push(object);
                let (name_ptr, name_len) = bytes(name);
                let value = abi::pyx_getattr(object, name_ptr, name_len);
                self.check(value)
            }
            ExprKind::IndirectCall { callee, args, star } => {
                self.eval_indirect_call(callee, args, star.as_deref())
            }
            ExprKind::Call { callee, args } => self.eval_call(callee, args),
            ExprKind::CallKw {
                callee,
                args,
                keywords,
            } => self.eval_call_kw(callee, args, keywords),
            ExprKind::MethodCall {
                object,
                method,
                args,
                star,
                keywords,
            } => self.eval_method_call(object, method, args, star.as_deref(), keywords),
            ExprKind::ClassCall {
                class,
                init,
                args,
                dynamic_kwargs,
                param_names,
                kwarg_index,
                class_name,
            } => self.eval_class_call(
                class,
                *init,
                args,
                dynamic_kwargs.as_deref(),
                param_names,
                *kwarg_index,
                class_name,
            ),
            ExprKind::DeferredCall { callee, args } => {
                let mut operands = Vec::with_capacity(args.len() + usize::from(callee.is_some()));
                if let Some(callee) = callee {
                    let value = self.eval(callee)?;
                    self.scratch_push(value);
                    operands.push(value);
                }
                for arg in args {
                    let value = self.eval(arg)?;
                    self.scratch_push(value);
                    operands.push(value);
                }
                let result = scoped(&operands, || {
                    abi::pyx_deferred_call(operands.len(), operands.as_ptr())
                });
                self.check(result)
            }
            ExprKind::Await { value } => {
                let value = self.eval(value)?;
                self.scratch_push(value);
                let result = abi::pyx_await(value);
                self.check(result)
            }
            ExprKind::Yield { value } => {
                let value = self.eval_optional(value)?;
                self.scratch_push(value);
                let sent = abi::pyx_yield(value);
                self.check(sent)
            }
            ExprKind::YieldFrom { value } => {
                let value = self.eval(value)?;
                self.scratch_push(value);
                let result = abi::pyx_yield_from(value);
                self.check(result)
            }
            ExprKind::ListComp { element, clauses } => {
                self.eval_comprehension(clauses, CompEmit::List(element))
            }
            ExprKind::SetComp { element, clauses } => {
                self.eval_comprehension(clauses, CompEmit::Set(element))
            }
            ExprKind::DictComp {
                key,
                value,
                clauses,
            } => self.eval_comprehension(clauses, CompEmit::Dict(key, value)),
        }
    }

    fn eval_optional(&mut self, expr: &Option<Box<Expr>>) -> Result<i64, Signal> {
        match expr {
            Some(expr) => self.eval(expr),
            None => Ok(host::none()),
        }
    }

    pub(crate) fn eval_const(&mut self, value: &Const) -> Result<i64, Signal> {
        match value {
            Const::Int(value) => Ok(host::tagged_int(*value)),
            Const::Bool(true) => Ok(abi::pyx_true()),
            Const::Bool(false) => Ok(abi::pyx_false()),
            Const::None => Ok(abi::pyx_none()),
            // `...` materialises as `None`, matching codegen.
            Const::Ellipsis => Ok(abi::pyx_none()),
            Const::NotImplemented => Ok(host::not_implemented()),
            Const::Float(value) => Ok(abi::pyx_float_new(*value)),
            Const::Str(text) => {
                let (ptr, len) = bytes(text);
                let value = abi::pyx_str_new(ptr, len);
                self.check(value)
            }
            Const::BigInt(text) => {
                let (ptr, len) = bytes(text);
                let value = abi::pyx_bigint_from_str(ptr, len);
                self.check(value)
            }
            Const::Bytes(data) => {
                let value = abi::pyx_bytes_new(data.as_ptr(), data.len());
                self.check(value)
            }
            Const::Global { module, name } => {
                let (module_ptr, module_len) = bytes(module);
                let (name_ptr, name_len) = bytes(name);
                let value = abi::pyx_global_get(module_ptr, module_len, name_ptr, name_len);
                self.check(value)
            }
            Const::Tuple(items) => {
                let mark = self.scratch_mark();
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    let value = self.eval_const(item)?;
                    self.scratch_push(value);
                    values.push(value);
                }
                let result = scoped(&values, || {
                    abi::pyx_tuple_new(values.len(), values.as_ptr())
                });
                let result = self.check(result);
                self.scratch_release(mark);
                result
            }
        }
    }

    fn eval_literal(&mut self, items: &[Expr], kind: LiteralKind) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            let value = self.eval(item)?;
            self.scratch_push(value);
            values.push(value);
        }
        let result = scoped(&values, || match kind {
            LiteralKind::List => abi::pyx_list_new(values.len(), values.as_ptr()),
            LiteralKind::Tuple => abi::pyx_tuple_new(values.len(), values.as_ptr()),
            LiteralKind::Set => abi::pyx_set_new(values.len(), values.as_ptr()),
        });
        self.scratch_release(mark);
        self.check(result)
    }

    fn eval_dict(&mut self, pairs: &[(Expr, Expr)]) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let mut keys = Vec::with_capacity(pairs.len());
        let mut values = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            let key = self.eval(key)?;
            self.scratch_push(key);
            let value = self.eval(value)?;
            self.scratch_push(value);
            keys.push(key);
            values.push(value);
        }
        let result = {
            let keys_token = abi::pyx_gc_scope_push(keys.as_ptr(), keys.len());
            let values_token = abi::pyx_gc_scope_push(values.as_ptr(), values.len());
            let result = abi::pyx_dict_new(keys.len(), keys.as_ptr(), values.as_ptr());
            abi::pyx_gc_scope_pop(values_token);
            abi::pyx_gc_scope_pop(keys_token);
            result
        };
        self.scratch_release(mark);
        self.check(result)
    }

    fn eval_module_call(
        &mut self,
        module: &str,
        function: &str,
        args: &[Expr],
        keywords: &[(String, Expr)],
    ) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let mut arg_values = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.eval(arg)?;
            self.scratch_push(value);
            arg_values.push(value);
        }
        let mut kw_values = Vec::with_capacity(keywords.len());
        for (name, value) in keywords {
            let (ptr, len) = bytes(name);
            let name = abi::pyx_str_new(ptr, len);
            self.check(name)?;
            self.scratch_push(name);
            let value = self.eval(value)?;
            self.scratch_push(value);
            kw_values.push((name, value));
        }
        let mut pairs = Vec::with_capacity(kw_values.len() * 2);
        for (name, value) in &kw_values {
            pairs.push(*name);
            pairs.push(*value);
        }
        let (module_ptr, module_len) = bytes(module);
        let (function_ptr, function_len) = bytes(function);
        let result = {
            let argv_token = abi::pyx_gc_scope_push(arg_values.as_ptr(), arg_values.len());
            let kw_token = abi::pyx_gc_scope_push(pairs.as_ptr(), pairs.len());
            let result = if keywords.is_empty() {
                abi::pyx_module_call(
                    module_ptr,
                    module_len,
                    function_ptr,
                    function_len,
                    arg_values.len(),
                    arg_values.as_ptr(),
                )
            } else {
                abi::pyx_module_call_kw(
                    module_ptr,
                    module_len,
                    function_ptr,
                    function_len,
                    arg_values.len(),
                    arg_values.as_ptr(),
                    kw_values.len(),
                    pairs.as_ptr(),
                )
            };
            abi::pyx_gc_scope_pop(kw_token);
            abi::pyx_gc_scope_pop(argv_token);
            result
        };
        self.scratch_release(mark);
        self.check(result)
    }

    fn eval_indirect_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        star: Option<&Expr>,
    ) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let callee_value = self.eval(callee)?;
        self.scratch_push(callee_value);
        let mut arg_values = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.eval(arg)?;
            self.scratch_push(value);
            arg_values.push(value);
        }
        let star_value = match star {
            Some(star) => {
                let value = self.eval(star)?;
                self.scratch_push(value);
                Some(value)
            }
            None => None,
        };
        let result = scoped(&arg_values, || match star_value {
            Some(star) => {
                abi::pyx_call_star(callee_value, arg_values.len(), arg_values.as_ptr(), star)
            }
            None => abi::pyx_call(callee_value, arg_values.len(), arg_values.as_ptr()),
        });
        self.scratch_release(mark);
        self.check(result)
    }

    fn eval_call(&mut self, callee: &Callee, args: &[Expr]) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let mut arg_values = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.eval(arg)?;
            self.scratch_push(value);
            arg_values.push(value);
        }
        let result: Result<i64, Signal> = match callee {
            Callee::Function(id) => self.direct_call(*id, &arg_values),
            Callee::Builtin(builtin) => {
                // `{**a, **b}` lowers to the internal `DictMerge` builtin,
                // which has no source-level name and therefore no
                // first-class function value.
                if *builtin == pyx_ir::Builtin::DictMerge {
                    if arg_values.len() != 2 {
                        Err(self.type_error("internal dict merge expects exactly 2 arguments"))
                    } else {
                        let value = abi::pyx_dict_merge(arg_values[0], arg_values[1]);
                        self.check(value)
                    }
                } else {
                    let (ptr, len) = bytes(builtin.name());
                    let function = abi::pyx_builtin_function(ptr, len);
                    match self.check(function) {
                        Ok(function) => {
                            self.scratch_push(function);
                            let value = scoped(&arg_values, || {
                                abi::pyx_call(function, arg_values.len(), arg_values.as_ptr())
                            });
                            self.check(value)
                        }
                        Err(signal) => Err(signal),
                    }
                }
            }
            Callee::Extension { path, member } => {
                let (path_ptr, path_len) = bytes(path);
                let (member_ptr, member_len) = bytes(member);
                let value = scoped(&arg_values, || {
                    abi::pyx_extension_call(
                        path_ptr,
                        path_len,
                        member_ptr,
                        member_len,
                        arg_values.len(),
                        arg_values.as_ptr(),
                    )
                });
                self.check(value)
            }
        };
        self.scratch_release(mark);
        result
    }

    fn eval_call_kw(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        keywords: &[(String, Expr)],
    ) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let function = self.eval(callee)?;
        self.scratch_push(function);
        let mut arg_values = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.eval(arg)?;
            self.scratch_push(value);
            arg_values.push(value);
        }
        let mut pairs = Vec::with_capacity(keywords.len() * 2);
        for (name, value) in keywords {
            let (ptr, len) = bytes(name);
            let name = abi::pyx_str_new(ptr, len);
            self.check(name)?;
            self.scratch_push(name);
            let value = self.eval(value)?;
            self.scratch_push(value);
            pairs.push(name);
            pairs.push(value);
        }
        let result = {
            let argv_token = abi::pyx_gc_scope_push(arg_values.as_ptr(), arg_values.len());
            let kw_token = abi::pyx_gc_scope_push(pairs.as_ptr(), pairs.len());
            let result = abi::pyx_call_kw(
                function,
                arg_values.len(),
                arg_values.as_ptr(),
                keywords.len(),
                pairs.as_ptr(),
            );
            abi::pyx_gc_scope_pop(kw_token);
            abi::pyx_gc_scope_pop(argv_token);
            result
        };
        self.scratch_release(mark);
        self.check(result)
    }

    fn eval_method_call(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[Expr],
        star: Option<&Expr>,
        keywords: &[(String, Expr)],
    ) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let receiver = self.eval(object)?;
        self.scratch_push(receiver);
        let mut arg_values = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.eval(arg)?;
            self.scratch_push(value);
            arg_values.push(value);
        }
        let star_value = match star {
            Some(star) => {
                let value = self.eval(star)?;
                self.scratch_push(value);
                Some(value)
            }
            None => None,
        };
        let mut pairs = Vec::with_capacity(keywords.len() * 2);
        for (name, value) in keywords {
            let (ptr, len) = bytes(name);
            let name = abi::pyx_str_new(ptr, len);
            self.check(name)?;
            self.scratch_push(name);
            let value = self.eval(value)?;
            self.scratch_push(value);
            pairs.push(name);
            pairs.push(value);
        }
        let (name_ptr, name_len) = bytes(method);
        let result = {
            let receiver_scope = [receiver];
            let receiver_token =
                abi::pyx_gc_scope_push(receiver_scope.as_ptr(), receiver_scope.len());
            let argv_token = abi::pyx_gc_scope_push(arg_values.as_ptr(), arg_values.len());
            let kw_token = abi::pyx_gc_scope_push(pairs.as_ptr(), pairs.len());
            let result = match star_value {
                Some(star) => abi::pyx_method_star(
                    receiver,
                    name_ptr,
                    name_len,
                    star,
                    keywords.len(),
                    pairs.as_ptr(),
                ),
                None if keywords.is_empty() => abi::pyx_method(
                    receiver,
                    name_ptr,
                    name_len,
                    arg_values.len(),
                    arg_values.as_ptr(),
                ),
                None => abi::pyx_method_kw(
                    receiver,
                    name_ptr,
                    name_len,
                    arg_values.len(),
                    arg_values.as_ptr(),
                    keywords.len(),
                    pairs.as_ptr(),
                ),
            };
            abi::pyx_gc_scope_pop(kw_token);
            abi::pyx_gc_scope_pop(argv_token);
            abi::pyx_gc_scope_pop(receiver_token);
            result
        };
        self.scratch_release(mark);
        self.check(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_class_call(
        &mut self,
        class: &Expr,
        init: FunctionId,
        args: &[Expr],
        dynamic_kwargs: Option<&Expr>,
        param_names: &[String],
        kwarg_index: Option<usize>,
        class_name: &str,
    ) -> Result<i64, Signal> {
        let mark = self.scratch_mark();
        let class_value = self.eval(class)?;
        self.scratch_push(class_value);
        let instance = abi::pyx_instance_new(class_value);
        self.check(instance)?;
        self.scratch_push(instance);

        let mut slots: Vec<i64> = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.eval(arg)?;
            self.scratch_push(value);
            slots.push(value);
        }

        if let Some(mapping) = dynamic_kwargs {
            let mapping = self.eval(mapping)?;
            self.scratch_push(mapping);
            let mut names = Vec::with_capacity(param_names.len() * 2);
            for name in param_names {
                names.push(name.as_ptr() as i64);
                names.push(name.len() as i64);
            }
            let result = {
                let slots_token = abi::pyx_gc_scope_push(slots.as_ptr(), slots.len());
                let result = abi::pyx_bind_class_kwargs(
                    mapping,
                    names.as_ptr(),
                    param_names.len(),
                    slots.as_mut_ptr(),
                    kwarg_index.map_or(-1, |index| index as i64),
                    class_name.as_ptr(),
                    class_name.len(),
                );
                abi::pyx_gc_scope_pop(slots_token);
                result
            };
            self.check(result)?;
        }

        let mut bound = Vec::with_capacity(slots.len() + 1);
        bound.push(instance);
        bound.extend_from_slice(&slots);
        let mark2 = self.scratch_mark();
        for &value in &bound {
            self.scratch_push(value);
        }
        let result = self.direct_call(init, &bound);
        self.scratch_release(mark2);
        result?;
        self.scratch_release(mark);
        Ok(instance)
    }

    /// Materialises an IR function (or closure) as a runtime function value.
    pub(crate) fn make_function(
        &mut self,
        function: FunctionId,
        captures: Vec<i64>,
    ) -> Result<i64, Signal> {
        let (program, base) = self.current_program();
        let func = &program.functions[function as usize];
        let params = &func.params[func.capture_count.min(func.params.len())..];
        let is_async = func.is_async;
        let is_generator = func.is_generator;
        let arity = func.call_arity();
        let name = func.name.clone();
        let mark = self.scratch_mark();
        let mut host_params = Vec::with_capacity(params.len());
        for param in params {
            let default = if let Some(expr) = &param.default_expr {
                let value = self.eval(expr)?;
                self.scratch_push(value);
                Some(value)
            } else if let Some(default) = &param.default {
                let value = self.eval_const(default)?;
                self.scratch_push(value);
                Some(value)
            } else if param.kind == pyx_ir::ParamKind::KwOnly {
                Some(host::none())
            } else {
                None
            };
            host_params.push(host::HostParam {
                name: param.name.clone(),
                kind: param.kind,
                default,
            });
        }
        let value = host::function_new(
            base + function as u64,
            captures,
            arity,
            name,
            is_async,
            is_generator,
            host_params,
        );
        let value = self.check(value);
        self.scratch_release(mark);
        value
    }

    /// Direct call to a program function with the complete parameter slots.
    pub(crate) fn direct_call(
        &mut self,
        function: FunctionId,
        args: &[i64],
    ) -> Result<i64, Signal> {
        let (program, base) = self.current_program();
        let func = &program.functions[function as usize];
        if func.capture_count != 0 {
            return Err(self.runtime_error(
                "internal error: a function with captures must be called through its closure value",
            ));
        }
        if args.len() != func.params.len() {
            abi::pyx_arity_error(func.params.len() as i64, args.len() as i64);
            return Err(Signal::Raise);
        }
        let token = base + function as u64;
        if func.is_async || func.is_generator {
            let fiber = host::fiber_new(
                token,
                &[],
                args,
                func.is_async,
                func.is_generator,
                &func.name,
            );
            return self.check(fiber);
        }
        self.invoke_bound(program, base, function, args)
    }

    fn eval_comprehension(
        &mut self,
        clauses: &[CompClause],
        emit: CompEmit<'_>,
    ) -> Result<i64, Signal> {
        if clauses.is_empty() {
            return Err(self.runtime_error("internal error: comprehension has no for clause"));
        }
        let mark = self.scratch_mark();
        let empty: [i64; 0] = [];
        let result = match emit {
            CompEmit::List(_) => abi::pyx_list_new(0, empty.as_ptr()),
            CompEmit::Set(_) => abi::pyx_set_new(0, empty.as_ptr()),
            CompEmit::Dict(..) => abi::pyx_dict_new(0, empty.as_ptr(), empty.as_ptr()),
        };
        self.check(result)?;
        self.scratch_push(result);
        self.comp_level(clauses, 0, result, emit)?;
        self.scratch_release(mark);
        Ok(result)
    }

    fn comp_level(
        &mut self,
        clauses: &[CompClause],
        index: usize,
        result: i64,
        emit: CompEmit<'_>,
    ) -> Result<(), Signal> {
        let clause = &clauses[index];
        let iterable = self.eval(&clause.iterable)?;
        self.scratch_push(iterable);
        let iterator = abi::pyx_iter_new(iterable);
        self.check(iterator)?;
        self.scratch_push(iterator);
        let loop_mark = self.scratch_mark();

        let mut item = host::none();
        loop {
            abi::pyx_safepoint();
            let produced = abi::pyx_iter_next(iterator, &mut item);
            self.check(produced)?;
            if produced == 0 {
                return Ok(());
            }
            self.scratch_push(item);
            match &clause.target {
                CompTarget::Name(id) => self.set_local(*id, item),
                CompTarget::Unpack(targets) => self.bind_unpack(targets, item)?,
            }
            let mut matched = true;
            for condition in &clause.conditions {
                let value = self.eval(condition)?;
                if !self.truthy(value) {
                    matched = false;
                    break;
                }
            }
            if matched {
                if index + 1 < clauses.len() {
                    self.comp_level(clauses, index + 1, result, emit)?;
                } else {
                    self.emit_comp_element(result, emit)?;
                }
            }
            // Only the iterator (and the result container, owned by the
            // caller's mark) survive into the next iteration.
            self.scratch_release(loop_mark);
        }
    }

    fn emit_comp_element(&mut self, result: i64, emit: CompEmit<'_>) -> Result<(), Signal> {
        match emit {
            CompEmit::List(element) => {
                let value = self.eval(element)?;
                self.scratch_push(value);
                let appended = abi::pyx_list_append(result, value);
                self.check(appended)?;
                Ok(())
            }
            CompEmit::Set(element) => {
                let value = self.eval(element)?;
                self.scratch_push(value);
                let appended = abi::pyx_set_add(result, value);
                self.check(appended)?;
                Ok(())
            }
            CompEmit::Dict(key, value) => {
                let key_value = self.eval(key)?;
                self.scratch_push(key_value);
                let value_value = self.eval(value)?;
                self.scratch_push(value_value);
                let result = abi::pyx_dict_set(result, key_value, value_value);
                self.check(result)?;
                Ok(())
            }
        }
    }

    pub(crate) fn bind_unpack(
        &mut self,
        targets: &[UnpackTarget],
        value: i64,
    ) -> Result<(), Signal> {
        let mark = self.scratch_mark();
        self.scratch_push(value);
        let sequence = if let Some(star) = targets
            .iter()
            .position(|target| matches!(target, UnpackTarget::Star(_)))
        {
            abi::pyx_unpack_star(value, star, targets.len() - star - 1)
        } else {
            abi::pyx_unpack_seq(value, targets.len())
        };
        self.check(sequence)?;
        self.scratch_push(sequence);
        for (index, target) in targets.iter().enumerate() {
            let index_value = host::tagged_int(index as i64);
            let item = abi::pyx_subscript(sequence, index_value);
            self.check(item)?;
            self.scratch_push(item);
            self.bind_unpack_target(target, item)?;
        }
        self.scratch_release(mark);
        Ok(())
    }

    fn bind_unpack_target(&mut self, target: &UnpackTarget, value: i64) -> Result<(), Signal> {
        match target {
            UnpackTarget::Name(id) => self.set_local(*id, value),
            UnpackTarget::Cell(id) => {
                let cell = self.local(*id);
                abi::pyx_cell_set(cell, value);
            }
            UnpackTarget::Star(inner) => self.bind_unpack_target(inner, value)?,
            UnpackTarget::Nested(targets) => self.bind_unpack(targets, value)?,
            UnpackTarget::Attr { object, name } => {
                self.scratch_push(value);
                let object = self.eval(object)?;
                self.scratch_push(object);
                let (name_ptr, name_len) = bytes(name);
                let result = abi::pyx_setattr(object, name_ptr, name_len, value);
                self.check(result)?;
            }
            UnpackTarget::Subscript { object, index } => {
                self.scratch_push(value);
                let object = self.eval(object)?;
                self.scratch_push(object);
                let index = self.eval(index)?;
                self.scratch_push(index);
                let result = abi::pyx_subscript_set(object, index, value);
                self.check(result)?;
            }
        }
        Ok(())
    }
}

/// What a comprehension appends once every clause has matched.
#[derive(Clone, Copy)]
enum CompEmit<'a> {
    List(&'a Expr),
    Set(&'a Expr),
    Dict(&'a Expr, &'a Expr),
}
