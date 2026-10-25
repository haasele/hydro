//! Statement execution: control flow, loops, exception regions, classes and
//! assignments.

use pyx_ir::{DeleteTarget, Expr, Handler, Stmt, StmtKind};
use pyx_runtime::abi;
use pyx_runtime::host;

use crate::frame::{Exec, Signal};
use crate::Interp;

fn bytes(text: &str) -> (*const u8, usize) {
    (text.as_ptr(), text.len())
}

/// True for the exception kinds whose handler match is decided by the runtime
/// hierarchy rather than by tag equality.
fn hierarchy_match(kind: pyx_ir::ExcKind) -> bool {
    matches!(
        kind,
        pyx_ir::ExcKind::Exception
            | pyx_ir::ExcKind::BaseException
            | pyx_ir::ExcKind::OSError
            | pyx_ir::ExcKind::FileNotFoundError
    )
}

impl Interp {
    pub(crate) fn exec_stmts(&mut self, stmts: &[Stmt]) -> Exec {
        for stmt in stmts {
            // A statement never hands temporaries to the next statement:
            // locals, module globals, the runtime exception/handler state and
            // the frame's return slot hold everything that must survive. Drop
            // the statement's scratch entries on both the normal and the
            // unwinding path so caught exceptions in a loop cannot retain
            // roots indefinitely.
            let mark = self.scratch_mark();
            abi::pyx_set_line(stmt.line);
            let top_level = self.current_frame().stmt_depth == 0;
            if top_level && self.current_frame().echo {
                // Only a *trailing* top-level expression is echoed: every
                // other top-level statement clears the pending value.
                self.last_value = None;
            }
            self.current_frame_mut().stmt_depth += 1;
            let result = self.exec_stmt(&stmt.kind);
            self.current_frame_mut().stmt_depth -= 1;
            self.scratch_release(mark);
            result?;
        }
        Ok(())
    }

    fn exec_stmt(&mut self, stmt: &StmtKind) -> Exec {
        match stmt {
            StmtKind::Expr(expr) => {
                let value = self.eval(expr)?;
                // `stmt_depth == 1` means this expression is the fragment's
                // top-level statement, not one nested in `if`/`for`/`try`.
                if self.current_frame().echo && self.current_frame().stmt_depth == 1 {
                    self.last_value = Some(value);
                }
                Ok(())
            }
            StmtKind::Assign { target, value } => {
                let value = self.eval(value)?;
                self.set_local(*target, value);
                Ok(())
            }
            StmtKind::CellInit { target, value } => {
                let value = match value {
                    Some(expr) => {
                        let value = self.eval(expr)?;
                        self.scratch_push(value);
                        value
                    }
                    None => host::none(),
                };
                let cell = abi::pyx_cell_new(value);
                self.check(cell)?;
                self.set_local(*target, cell);
                Ok(())
            }
            StmtKind::CellAssign { target, value } => {
                let value = self.eval(value)?;
                self.scratch_push(value);
                let cell = self.local(*target);
                abi::pyx_cell_set(cell, value);
                Ok(())
            }
            StmtKind::GlobalSet {
                module,
                name,
                value,
            } => {
                let value = self.eval(value)?;
                self.scratch_push(value);
                let (module_ptr, module_len) = bytes(module);
                let (name_ptr, name_len) = bytes(name);
                let result = abi::pyx_global_set(module_ptr, module_len, name_ptr, name_len, value);
                self.check(result)?;
                self.sync_module_global(name, value);
                Ok(())
            }
            StmtKind::AugAssign { target, op, value } => {
                let mark = self.scratch_mark();
                let rhs = self.eval(value)?;
                self.scratch_push(rhs);
                let lhs = self.local(*target);
                let result = abi::pyx_binop(op.code(), lhs, rhs);
                self.check(result)?;
                self.set_local(*target, result);
                self.scratch_release(mark);
                Ok(())
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                let condition = self.eval(cond)?;
                if self.truthy(condition) {
                    self.exec_stmts(then_body)
                } else {
                    self.exec_stmts(else_body)
                }
            }
            StmtKind::While {
                cond,
                body,
                else_body,
            } => self.exec_while(cond, body, else_body),
            StmtKind::ForRange {
                var,
                start,
                stop,
                step,
                body,
                else_body,
            } => self.exec_for_range(*var, start, stop, step.as_ref(), body, else_body),
            StmtKind::ForEach {
                var,
                iterable,
                body,
                else_body,
            } => self.exec_for_each(*var, iterable, body, else_body),
            StmtKind::Unpack { targets, value } => {
                let value = self.eval(value)?;
                self.bind_unpack(targets, value)
            }
            StmtKind::SetSubscript {
                object,
                index,
                value,
            } => {
                let mark = self.scratch_mark();
                let value = self.eval(value)?;
                self.scratch_push(value);
                let object = self.eval(object)?;
                self.scratch_push(object);
                let index = self.eval(index)?;
                self.scratch_push(index);
                let result = abi::pyx_subscript_set(object, index, value);
                self.scratch_release(mark);
                self.check(result)?;
                Ok(())
            }
            StmtKind::SetSlice {
                target,
                start,
                stop,
                step,
                value,
            } => {
                let mark = self.scratch_mark();
                let value = self.eval(value)?;
                self.scratch_push(value);
                let object = self.eval(target)?;
                self.scratch_push(object);
                let start = self.eval(start)?;
                self.scratch_push(start);
                let stop = self.eval(stop)?;
                self.scratch_push(stop);
                let step = self.eval(step)?;
                self.scratch_push(step);
                let result = abi::pyx_slice_set(object, start, stop, step, value);
                self.scratch_release(mark);
                self.check(result)?;
                Ok(())
            }
            StmtKind::Closure {
                target,
                function,
                captures,
            } => {
                let mark = self.scratch_mark();
                let captures: Vec<i64> = captures.iter().map(|id| self.local(*id)).collect();
                for &value in &captures {
                    self.scratch_push(value);
                }
                let value = self.make_function(*function, captures)?;
                self.set_local(*target, value);
                self.scratch_release(mark);
                Ok(())
            }
            StmtKind::ClassDef {
                target,
                name,
                base,
                methods,
                attributes,
            } => self.exec_class_def(*target, name, base.as_ref(), methods, attributes),
            StmtKind::SetAttr {
                object,
                name,
                value,
            } => {
                let mark = self.scratch_mark();
                let value = self.eval(value)?;
                self.scratch_push(value);
                let object = self.eval(object)?;
                self.scratch_push(object);
                let (name_ptr, name_len) = bytes(name);
                let result = abi::pyx_setattr(object, name_ptr, name_len, value);
                self.scratch_release(mark);
                self.check(result)?;
                Ok(())
            }
            StmtKind::Return(value) => {
                let value = match value {
                    Some(expr) => self.eval(expr)?,
                    None => host::none(),
                };
                // Park the result in a rooted local slot: enclosing `finally`
                // bodies run before the frame returns.
                self.set_return_value(value);
                Err(Signal::Return(value))
            }
            StmtKind::Break => Err(Signal::Break),
            StmtKind::Continue => Err(Signal::Continue),
            StmtKind::Pass => Ok(()),
            StmtKind::Yield { value } => {
                let value = match value {
                    Some(expr) => {
                        let value = self.eval(expr)?;
                        self.scratch_push(value);
                        value
                    }
                    None => host::none(),
                };
                let sent = abi::pyx_yield(value);
                self.check(sent)?;
                Ok(())
            }
            StmtKind::Raise { exc, msg, cause } => {
                self.exec_raise(*exc, msg.as_ref(), cause.as_ref())
            }
            StmtKind::Assert { test, msg } => {
                let condition = self.eval(test)?;
                if self.truthy(condition) {
                    return Ok(());
                }
                let message = match msg {
                    Some(expr) => {
                        let value = self.eval(expr)?;
                        self.scratch_push(value);
                        value
                    }
                    None => host::none(),
                };
                abi::pyx_raise(pyx_ir::ExcKind::AssertionError.tag(), message);
                Err(Signal::Raise)
            }
            StmtKind::Delete { targets } => {
                for target in targets {
                    self.exec_delete(target)?;
                }
                Ok(())
            }
            StmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => self.exec_try(body, handlers, else_body, finally_body),
        }
    }

    fn exec_while(&mut self, cond: &Expr, body: &[Stmt], else_body: &[Stmt]) -> Exec {
        let loop_depth = abi::pyx_exc_handler_depth();
        let mut broke = false;
        loop {
            abi::pyx_safepoint();
            let condition = self.eval(cond)?;
            if !self.truthy(condition) {
                break;
            }
            match self.exec_stmts(body) {
                Ok(()) => {}
                Err(Signal::Break) => {
                    abi::pyx_exc_handler_truncate(loop_depth);
                    broke = true;
                    break;
                }
                Err(Signal::Continue) => {
                    abi::pyx_exc_handler_truncate(loop_depth);
                }
                Err(signal) => return Err(signal),
            }
        }
        if broke || else_body.is_empty() {
            Ok(())
        } else {
            self.exec_stmts(else_body)
        }
    }

    fn exec_for_range(
        &mut self,
        var: pyx_ir::LocalId,
        start: &Expr,
        stop: &Expr,
        step: Option<&Expr>,
        body: &[Stmt],
        else_body: &[Stmt],
    ) -> Exec {
        let loop_depth = abi::pyx_exc_handler_depth();
        let mark = self.scratch_mark();
        let start_value = self.eval(start)?;
        self.scratch_push(start_value);
        let stop_value = self.eval(stop)?;
        self.scratch_push(stop_value);
        let step_value = match step {
            Some(expr) => {
                let value = self.eval(expr)?;
                self.scratch_push(value);
                value
            }
            None => host::tagged_int(1),
        };
        if step_value == 0 {
            let message = b"range() arg 3 must not be zero";
            abi::pyx_raise_msg(
                pyx_ir::ExcKind::ValueError.tag(),
                message.as_ptr(),
                message.len(),
            );
            return Err(Signal::Raise);
        }
        let current_index = self.scratch_mark();
        self.scratch_push(start_value);
        let zero = host::tagged_int(0);
        let mut broke = false;
        loop {
            abi::pyx_safepoint();
            let current = self.scratch_value(current_index);
            let positive = abi::pyx_binop(pyx_ir::BinOp::Gt.code(), step_value, zero);
            self.check(positive)?;
            if self.truthy(positive) {
                let comparison = abi::pyx_binop(pyx_ir::BinOp::Lt.code(), current, stop_value);
                self.check(comparison)?;
                if !self.truthy(comparison) {
                    break;
                }
            } else {
                let comparison = abi::pyx_binop(pyx_ir::BinOp::Gt.code(), current, stop_value);
                self.check(comparison)?;
                if !self.truthy(comparison) {
                    break;
                }
            }
            self.set_local(var, current);
            match self.exec_stmts(body) {
                Ok(()) => {}
                Err(Signal::Break) => {
                    abi::pyx_exc_handler_truncate(loop_depth);
                    broke = true;
                    break;
                }
                Err(Signal::Continue) => {
                    abi::pyx_exc_handler_truncate(loop_depth);
                }
                Err(signal) => return Err(signal),
            }
            let current = self.scratch_value(current_index);
            let next = abi::pyx_binop(pyx_ir::BinOp::Add.code(), current, step_value);
            self.check(next)?;
            self.scratch_store(current_index, next);
        }
        self.scratch_release(mark);
        if broke || else_body.is_empty() {
            Ok(())
        } else {
            self.exec_stmts(else_body)
        }
    }

    fn exec_for_each(
        &mut self,
        var: pyx_ir::LocalId,
        iterable: &Expr,
        body: &[Stmt],
        else_body: &[Stmt],
    ) -> Exec {
        let loop_depth = abi::pyx_exc_handler_depth();
        let mark = self.scratch_mark();
        let iterable = self.eval(iterable)?;
        self.scratch_push(iterable);
        let iterator = abi::pyx_iter_new(iterable);
        self.check(iterator)?;
        self.scratch_push(iterator);
        let loop_mark = self.scratch_mark();
        let mut broke = false;
        loop {
            abi::pyx_safepoint();
            let mut item = host::none();
            let produced = abi::pyx_iter_next(iterator, &mut item);
            self.check(produced)?;
            if produced == 0 {
                break;
            }
            self.set_local(var, item);
            match self.exec_stmts(body) {
                Ok(()) => {}
                Err(Signal::Break) => {
                    abi::pyx_exc_handler_truncate(loop_depth);
                    broke = true;
                    break;
                }
                Err(Signal::Continue) => {
                    abi::pyx_exc_handler_truncate(loop_depth);
                }
                Err(signal) => {
                    self.scratch_release(mark);
                    return Err(signal);
                }
            }
            self.scratch_release(loop_mark);
        }
        self.scratch_release(mark);
        if broke || else_body.is_empty() {
            Ok(())
        } else {
            self.exec_stmts(else_body)
        }
    }

    fn exec_class_def(
        &mut self,
        target: pyx_ir::LocalId,
        name: &str,
        base: Option<&Expr>,
        methods: &[pyx_ir::ClassMethod],
        attributes: &[pyx_ir::ClassAttr],
    ) -> Exec {
        let mark = self.scratch_mark();
        let base = match base {
            Some(expr) => {
                let value = self.eval(expr)?;
                self.scratch_push(value);
                value
            }
            None => host::none(),
        };
        let mut names = Vec::with_capacity(methods.len());
        let mut functions = Vec::with_capacity(methods.len());
        let mut kinds = Vec::with_capacity(methods.len());
        for method in methods {
            let (ptr, len) = bytes(&method.name);
            let method_name = abi::pyx_str_new(ptr, len);
            self.check(method_name)?;
            self.scratch_push(method_name);
            let function = self.make_function(method.function, Vec::new())?;
            self.scratch_push(function);
            names.push(method_name);
            functions.push(function);
            kinds.push(method.kind.abi_tag());
        }
        let mut attr_names = Vec::with_capacity(attributes.len());
        let mut attr_values = Vec::with_capacity(attributes.len());
        for attribute in attributes {
            let (ptr, len) = bytes(&attribute.name);
            let attr_name = abi::pyx_str_new(ptr, len);
            self.check(attr_name)?;
            self.scratch_push(attr_name);
            let value = self.eval(&attribute.value)?;
            self.scratch_push(value);
            attr_names.push(attr_name);
            attr_values.push(value);
        }
        let class = {
            let names_token = abi::pyx_gc_scope_push(names.as_ptr(), names.len());
            let functions_token = abi::pyx_gc_scope_push(functions.as_ptr(), functions.len());
            let attr_names_token = abi::pyx_gc_scope_push(attr_names.as_ptr(), attr_names.len());
            let attr_values_token = abi::pyx_gc_scope_push(attr_values.as_ptr(), attr_values.len());
            let class = abi::pyx_class_new(
                name.as_ptr(),
                name.len(),
                base,
                methods.len(),
                names.as_ptr(),
                functions.as_ptr(),
                kinds.as_ptr(),
                attributes.len(),
                attr_names.as_ptr(),
                attr_values.as_ptr(),
            );
            abi::pyx_gc_scope_pop(attr_values_token);
            abi::pyx_gc_scope_pop(attr_names_token);
            abi::pyx_gc_scope_pop(functions_token);
            abi::pyx_gc_scope_pop(names_token);
            class
        };
        self.check(class)?;
        self.set_local(target, class);
        self.scratch_release(mark);
        Ok(())
    }

    fn exec_raise(
        &mut self,
        exc: Option<pyx_ir::ExcKind>,
        msg: Option<&Expr>,
        cause: Option<&Expr>,
    ) -> Exec {
        match exc {
            Some(kind) => {
                let mark = self.scratch_mark();
                let message = match msg {
                    Some(expr) => {
                        let value = self.eval(expr)?;
                        self.scratch_push(value);
                        value
                    }
                    None => host::none(),
                };
                match cause {
                    Some(expr) => {
                        let cause = self.eval(expr)?;
                        self.scratch_push(cause);
                        abi::pyx_raise_from(kind.tag(), message, cause);
                    }
                    None => {
                        abi::pyx_raise(kind.tag(), message);
                    }
                }
                self.scratch_release(mark);
                Err(Signal::Raise)
            }
            None => {
                if let Some(expr) = msg {
                    let mark = self.scratch_mark();
                    let value = self.eval(expr)?;
                    self.scratch_push(value);
                    match cause {
                        Some(expr) => {
                            let cause = self.eval(expr)?;
                            self.scratch_push(cause);
                            abi::pyx_raise_value_from(value, cause);
                        }
                        None => {
                            abi::pyx_raise_value(value);
                        }
                    }
                    self.scratch_release(mark);
                    return Err(Signal::Raise);
                }
                if let Some(value) = self.current_frame().handled.last().copied() {
                    abi::pyx_raise_value(value);
                    return Err(Signal::Raise);
                }
                if let Some(value) = self.current_frame().finally_slots.last().copied() {
                    if value != host::none() {
                        abi::pyx_raise_value(value);
                        return Err(Signal::Raise);
                    }
                }
                if abi::pyx_exc_pending() != 0 {
                    return Err(Signal::Raise);
                }
                let message = b"No active exception to reraise";
                abi::pyx_raise_msg(
                    pyx_ir::ExcKind::RuntimeError.tag(),
                    message.as_ptr(),
                    message.len(),
                );
                Err(Signal::Raise)
            }
        }
    }

    fn exec_delete(&mut self, target: &DeleteTarget) -> Exec {
        match target {
            DeleteTarget::Name(id) => {
                self.set_local(*id, host::none());
                Ok(())
            }
            DeleteTarget::Cell(id) => {
                let cell = self.local(*id);
                abi::pyx_cell_set(cell, host::none());
                Ok(())
            }
            DeleteTarget::Global { module, name } => {
                let (module_ptr, module_len) = bytes(module);
                let (name_ptr, name_len) = bytes(name);
                let result = abi::pyx_global_delete(module_ptr, module_len, name_ptr, name_len);
                self.check(result)?;
                Ok(())
            }
            DeleteTarget::Attr { object, name } => {
                let object = self.eval(object)?;
                self.scratch_push(object);
                let (name_ptr, name_len) = bytes(name);
                let result = abi::pyx_delete_attr(object, name_ptr, name_len);
                self.check(result)?;
                Ok(())
            }
            DeleteTarget::Subscript { object, index } => {
                let object = self.eval(object)?;
                self.scratch_push(object);
                let index = self.eval(index)?;
                self.scratch_push(index);
                let result = abi::pyx_delete_subscript(object, index);
                self.check(result)?;
                Ok(())
            }
            DeleteTarget::Slice {
                object,
                start,
                stop,
                step,
            } => {
                let mark = self.scratch_mark();
                let object = self.eval(object)?;
                self.scratch_push(object);
                let start = self.eval(start)?;
                self.scratch_push(start);
                let stop = self.eval(stop)?;
                self.scratch_push(stop);
                let step = self.eval(step)?;
                self.scratch_push(step);
                let result = abi::pyx_delete_slice(object, start, stop, step);
                self.scratch_release(mark);
                self.check(result)?;
                Ok(())
            }
        }
    }

    fn exec_try(
        &mut self,
        body: &[Stmt],
        handlers: &[Handler],
        else_body: &[Stmt],
        finally_body: &[Stmt],
    ) -> Exec {
        let outer_depth = abi::pyx_exc_handler_depth();
        let mut result = self.exec_stmts(body);
        let mut dispatch_handlers = matches!(&result, Err(Signal::Raise));

        // `else` runs only after an exception-free body; exceptions raised
        // there bypass this statement's handlers.
        if matches!(&result, Ok(())) && !else_body.is_empty() {
            result = self.exec_stmts(else_body);
            dispatch_handlers = false;
        }

        // `except` dispatch for exceptions escaping the body.
        if dispatch_handlers && !handlers.is_empty() {
            abi::pyx_exc_handler_truncate(outer_depth);
            let mut matched = None;
            for handler in handlers {
                if abi::pyx_exc_pending() == 0 {
                    break;
                }
                let catches = match handler.exc {
                    None => true,
                    Some(kind) if hierarchy_match(kind) => abi::pyx_exc_matches(kind.tag()) != 0,
                    Some(kind) => abi::pyx_exc_tag() == kind.tag() as i64,
                };
                if catches {
                    matched = Some(handler);
                    break;
                }
            }
            if let Some(handler) = matched {
                let value = abi::pyx_exc_value();
                // Root the exception object across the binding and body.
                self.scratch_push(value);
                if let Some(binding) = handler.binding {
                    if handler.binding_cell {
                        let cell = self.local(binding);
                        abi::pyx_cell_set(cell, value);
                    } else {
                        self.set_local(binding, value);
                    }
                }
                abi::pyx_exc_clear();
                abi::pyx_exc_handler_push(value);
                self.current_frame_mut().handled.push(value);
                let arm = self.exec_stmts(&handler.body);
                self.current_frame_mut().handled.pop();
                result = match arm {
                    Ok(()) => {
                        abi::pyx_exc_handler_pop();
                        Ok(())
                    }
                    other => other,
                };
            }
        }

        if finally_body.is_empty() {
            return result;
        }

        // Unwind nested handler contexts before running `finally`; park an
        // in-flight exception, clear the pending flag and enter the body with
        // a handler marker so a bare `raise` sees it.
        abi::pyx_exc_handler_truncate(outer_depth);
        let in_flight = matches!(&result, Err(Signal::Raise));
        let parked = if in_flight {
            let value = abi::pyx_exc_value();
            abi::pyx_exc_clear();
            self.scratch_push(value);
            value
        } else {
            host::none()
        };
        abi::pyx_exc_handler_push(parked);
        self.current_frame_mut().finally_slots.push(parked);
        let finally_result = self.exec_stmts(finally_body);
        self.current_frame_mut().finally_slots.pop();
        match finally_result {
            Ok(()) => {
                abi::pyx_exc_handler_pop();
                if in_flight {
                    abi::pyx_raise_value(parked);
                    Err(Signal::Raise)
                } else {
                    result
                }
            }
            // A signal from `finally` replaces the in-flight transfer.
            other => other,
        }
    }
}
