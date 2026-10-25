//! String-accumulator lowering.
//!
//! Appending to a `str` local in a loop costs O(n²) with immutable strings:
//! every `str + str` copies the whole left operand. When the typed IR proves
//! that a local is used *only* as a pure accumulator, this pass rewrites the
//! loop to the runtime's mutable string builder
//! ([`ExprKind::StrBuilderNew`] / [`ExprKind::StrBuilderAppend`] /
//! [`ExprKind::StrBuilderFinish`]), which appends in place and finishes as a
//! normal `str`.
//!
//! The pass is deliberately conservative. It accepts a top-level pattern per
//! accumulator (several accumulators per function are independent):
//!
//! ```text
//! s = <str>                  # seed, same statement list
//! <statements without s>
//! while/for ...:             # header and else-clause never touch s
//!     s += <str>             # or `s = s + <str>`, unconditionally
//!     <statements without s>
//! <statements; at least one reads s>
//! ```
//!
//! Everything else keeps the generic `pyx_binop` path unchanged: aliasing
//! (`t = s`), reads or conditional appends inside the loop, closure captures,
//! `del`/unpack writes, loop conditions reading the accumulator, nested
//! loops, early `return s`, and module globals that other functions observe
//! through the global table.

use std::collections::HashMap;

use pyx_ir::{
    BinOp, CompClause, CompTarget, DeleteTarget, Expr, ExprKind, Function, Handler, LocalId,
    LocalInfo, Program, Stmt, StmtKind, Type, UnpackTarget,
};

/// Rewrites every function of `program` that matches the accumulator proof.
pub(crate) fn apply(program: &mut Program) {
    for function in &mut program.functions {
        apply_function(function);
    }
}

/// How one statement (or expression) uses the candidate local.
#[derive(Clone, Copy, Default)]
struct Use {
    /// The local appears anywhere: read, write, capture or deletion.
    mentioned: bool,
    /// The local's current value is consumed.
    read: bool,
}

impl Use {
    fn none() -> Use {
        Use::default()
    }

    fn read() -> Use {
        Use {
            mentioned: true,
            read: true,
        }
    }

    fn write() -> Use {
        Use {
            mentioned: true,
            read: false,
        }
    }

    fn merge(self, other: Use) -> Use {
        Use {
            mentioned: self.mentioned || other.mentioned,
            read: self.read || other.read,
        }
    }
}

fn expr_use(expr: &Expr, local: LocalId) -> Use {
    match &expr.kind {
        ExprKind::Local(id) | ExprKind::CellGet(id) => {
            if *id == local {
                Use::read()
            } else {
                Use::none()
            }
        }
        ExprKind::CellSet { target, value } | ExprKind::NamedExpr { target, value } => {
            let write = if *target == local {
                Use::write()
            } else {
                Use::none()
            };
            write.merge(expr_use(value, local))
        }
        ExprKind::StrBuilderNew { init } => expr_use(init, local),
        ExprKind::StrBuilderAppend { builder, value } => {
            expr_use(builder, local).merge(expr_use(value, local))
        }
        ExprKind::StrBuilderFinish { builder } => expr_use(builder, local),
        ExprKind::Bin { lhs, rhs, .. } => expr_use(lhs, local).merge(expr_use(rhs, local)),
        ExprKind::Un { operand, .. } | ExprKind::Await { value: operand } => {
            expr_use(operand, local)
        }
        ExprKind::BoolOp { lhs, rhs, .. } => expr_use(lhs, local).merge(expr_use(rhs, local)),
        ExprKind::IfExp { test, then, orelse } => expr_use(test, local)
            .merge(expr_use(then, local))
            .merge(expr_use(orelse, local)),
        ExprKind::List(items)
        | ExprKind::Tuple(items)
        | ExprKind::Set(items)
        | ExprKind::Call { args: items, .. } => exprs_use(items, local),
        ExprKind::Dict(pairs) => pairs.iter().fold(Use::none(), |acc, (key, value)| {
            acc.merge(expr_use(key, local))
                .merge(expr_use(value, local))
        }),
        ExprKind::Subscript { object, index } => {
            expr_use(object, local).merge(expr_use(index, local))
        }
        ExprKind::Slice {
            object,
            start,
            stop,
            step,
        } => {
            let mut use_ = expr_use(object, local);
            for bound in [start, stop, step].into_iter().flatten() {
                use_ = use_.merge(expr_use(bound, local));
            }
            use_
        }
        ExprKind::GetAttr { object, .. } => expr_use(object, local),
        ExprKind::IndirectCall { callee, args, star } => {
            let mut use_ = expr_use(callee, local).merge(exprs_use(args, local));
            if let Some(star) = star {
                use_ = use_.merge(expr_use(star, local));
            }
            use_
        }
        ExprKind::ListComp { element, clauses } | ExprKind::SetComp { element, clauses } => {
            expr_use(element, local).merge(clauses_use(clauses, local))
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => expr_use(key, local)
            .merge(expr_use(value, local))
            .merge(clauses_use(clauses, local)),
        ExprKind::MethodCall {
            object,
            args,
            star,
            keywords,
            ..
        } => {
            let mut use_ = expr_use(object, local).merge(exprs_use(args, local));
            if let Some(star) = star {
                use_ = use_.merge(expr_use(star, local));
            }
            for (_, keyword) in keywords {
                use_ = use_.merge(expr_use(keyword, local));
            }
            use_
        }
        ExprKind::CallKw {
            callee,
            args,
            keywords,
        } => {
            let mut use_ = expr_use(callee, local).merge(exprs_use(args, local));
            for (_, keyword) in keywords {
                use_ = use_.merge(expr_use(keyword, local));
            }
            use_
        }
        ExprKind::ClassCall {
            class,
            args,
            dynamic_kwargs,
            ..
        } => {
            let mut use_ = expr_use(class, local).merge(exprs_use(args, local));
            if let Some(kwargs) = dynamic_kwargs {
                use_ = use_.merge(expr_use(kwargs, local));
            }
            use_
        }
        ExprKind::DeferredCall { callee, args } => {
            let mut use_ = exprs_use(args, local);
            if let Some(callee) = callee {
                use_ = use_.merge(expr_use(callee, local));
            }
            use_
        }
        ExprKind::Yield { value } => match value {
            Some(value) => expr_use(value, local),
            None => Use::none(),
        },
        ExprKind::YieldFrom { value } => expr_use(value, local),
        ExprKind::ModuleCall { args, keywords, .. } => {
            let mut use_ = exprs_use(args, local);
            for (_, keyword) in keywords {
                use_ = use_.merge(expr_use(keyword, local));
            }
            use_
        }
        // Leaves: no local slot is referenced.
        ExprKind::Const(_)
        | ExprKind::FunctionRef(_)
        | ExprKind::BuiltinRef(_)
        | ExprKind::ExceptionClass(_)
        | ExprKind::ModuleRef { .. }
        | ExprKind::DeferredImport { .. }
        | ExprKind::ModuleAttr { .. }
        | ExprKind::GlobalGet { .. }
        | ExprKind::ExtensionAttr { .. } => Use::none(),
    }
}

fn exprs_use(exprs: &[Expr], local: LocalId) -> Use {
    exprs
        .iter()
        .fold(Use::none(), |acc, expr| acc.merge(expr_use(expr, local)))
}

fn clauses_use(clauses: &[CompClause], local: LocalId) -> Use {
    clauses.iter().fold(Use::none(), |acc, clause| {
        acc.merge(comp_target_use(&clause.target, local))
            .merge(expr_use(&clause.iterable, local))
            .merge(exprs_use(&clause.conditions, local))
    })
}

/// Loop/comprehension targets bind a value; they never read the local.
fn comp_target_use(target: &CompTarget, local: LocalId) -> Use {
    match target {
        CompTarget::Name(id) => local_use(*id, local),
        CompTarget::Unpack(targets) => unpack_targets_use(targets, local),
    }
}

fn local_use(id: LocalId, local: LocalId) -> Use {
    if id == local {
        Use::write()
    } else {
        Use::none()
    }
}

fn unpack_targets_use(targets: &[UnpackTarget], local: LocalId) -> Use {
    targets.iter().fold(Use::none(), |acc, target| {
        acc.merge(unpack_target_use(target, local))
    })
}

fn unpack_target_use(target: &UnpackTarget, local: LocalId) -> Use {
    match target {
        UnpackTarget::Name(id) | UnpackTarget::Cell(id) => local_use(*id, local),
        UnpackTarget::Star(inner) => unpack_target_use(inner, local),
        UnpackTarget::Nested(inner) => unpack_targets_use(inner, local),
        UnpackTarget::Attr { object, .. } => expr_use(object, local),
        UnpackTarget::Subscript { object, index } => {
            expr_use(object, local).merge(expr_use(index, local))
        }
    }
}

fn delete_target_use(target: &DeleteTarget, local: LocalId) -> Use {
    match target {
        DeleteTarget::Name(id) | DeleteTarget::Cell(id) => local_use(*id, local),
        DeleteTarget::Global { .. } => Use::none(),
        DeleteTarget::Attr { object, .. } => expr_use(object, local),
        DeleteTarget::Subscript { object, index } => {
            expr_use(object, local).merge(expr_use(index, local))
        }
        DeleteTarget::Slice {
            object,
            start,
            stop,
            step,
        } => expr_use(object, local)
            .merge(expr_use(start, local))
            .merge(expr_use(stop, local))
            .merge(expr_use(step, local)),
    }
}

fn handler_use(handler: &Handler, local: LocalId) -> Use {
    let binding = match handler.binding {
        Some(id) => local_use(id, local),
        None => Use::none(),
    };
    binding.merge(stmts_use(&handler.body, local))
}

fn stmts_use(stmts: &[Stmt], local: LocalId) -> Use {
    stmts
        .iter()
        .fold(Use::none(), |acc, stmt| acc.merge(stmt_use(stmt, local)))
}

fn stmt_use(stmt: &Stmt, local: LocalId) -> Use {
    match &stmt.kind {
        StmtKind::Expr(expr) => expr_use(expr, local),
        StmtKind::Assign { target, value } => {
            local_use(*target, local).merge(expr_use(value, local))
        }
        StmtKind::CellInit { target, value } => {
            let mut use_ = local_use(*target, local);
            if let Some(value) = value {
                use_ = use_.merge(expr_use(value, local));
            }
            use_
        }
        StmtKind::CellAssign { target, value } => {
            local_use(*target, local).merge(expr_use(value, local))
        }
        StmtKind::GlobalSet { value, .. } => expr_use(value, local),
        StmtKind::AugAssign { target, value, .. } => {
            local_use(*target, local).merge(expr_use(value, local))
        }
        StmtKind::If {
            cond,
            then_body,
            else_body,
        } => expr_use(cond, local)
            .merge(stmts_use(then_body, local))
            .merge(stmts_use(else_body, local)),
        StmtKind::While {
            cond,
            body,
            else_body,
        } => expr_use(cond, local)
            .merge(stmts_use(body, local))
            .merge(stmts_use(else_body, local)),
        StmtKind::ForRange {
            var,
            start,
            stop,
            step,
            body,
            else_body,
        } => {
            let mut use_ = local_use(*var, local)
                .merge(expr_use(start, local))
                .merge(expr_use(stop, local));
            if let Some(step) = step {
                use_ = use_.merge(expr_use(step, local));
            }
            use_.merge(stmts_use(body, local))
                .merge(stmts_use(else_body, local))
        }
        StmtKind::ForEach {
            var,
            iterable,
            body,
            else_body,
        } => local_use(*var, local)
            .merge(expr_use(iterable, local))
            .merge(stmts_use(body, local))
            .merge(stmts_use(else_body, local)),
        StmtKind::Unpack { targets, value } => {
            unpack_targets_use(targets, local).merge(expr_use(value, local))
        }
        StmtKind::SetSubscript {
            object,
            index,
            value,
        } => expr_use(object, local)
            .merge(expr_use(index, local))
            .merge(expr_use(value, local)),
        StmtKind::SetSlice {
            target,
            start,
            stop,
            step,
            value,
        } => expr_use(target, local)
            .merge(expr_use(start, local))
            .merge(expr_use(stop, local))
            .merge(expr_use(step, local))
            .merge(expr_use(value, local)),
        StmtKind::Closure {
            target, captures, ..
        } => {
            // Capturing the local copies its current value, so it is both a
            // mention and a read; the capture list is the only way a nested
            // function can observe the local.
            let captured = if captures.contains(&local) {
                Use::read()
            } else {
                Use::none()
            };
            local_use(*target, local).merge(captured)
        }
        StmtKind::ClassDef {
            target,
            name: _,
            base,
            methods: _,
            attributes,
        } => {
            // Method bodies are separate functions; their only link to this
            // scope is a closure capture, which the enclosing statement list
            // records.
            let mut use_ = local_use(*target, local);
            if let Some(base) = base {
                use_ = use_.merge(expr_use(base, local));
            }
            for attribute in attributes {
                use_ = use_.merge(expr_use(&attribute.value, local));
            }
            use_
        }
        StmtKind::SetAttr {
            object,
            name: _,
            value,
        } => expr_use(object, local).merge(expr_use(value, local)),
        StmtKind::Return(value) => match value {
            Some(value) => expr_use(value, local),
            None => Use::none(),
        },
        StmtKind::Break | StmtKind::Continue | StmtKind::Pass => Use::none(),
        StmtKind::Yield { value } => match value {
            Some(value) => expr_use(value, local),
            None => Use::none(),
        },
        StmtKind::Raise { exc: _, msg, cause } => {
            let mut use_ = Use::none();
            if let Some(msg) = msg {
                use_ = use_.merge(expr_use(msg, local));
            }
            if let Some(cause) = cause {
                use_ = use_.merge(expr_use(cause, local));
            }
            use_
        }
        StmtKind::Assert { test, msg } => {
            let mut use_ = expr_use(test, local);
            if let Some(msg) = msg {
                use_ = use_.merge(expr_use(msg, local));
            }
            use_
        }
        StmtKind::Delete { targets } => targets.iter().fold(Use::none(), |acc, target| {
            acc.merge(delete_target_use(target, local))
        }),
        StmtKind::Try {
            body,
            handlers,
            else_body,
            finally_body,
        } => {
            let mut use_ = stmts_use(body, local);
            for handler in handlers {
                use_ = use_.merge(handler_use(handler, local));
            }
            use_.merge(stmts_use(else_body, local))
                .merge(stmts_use(finally_body, local))
        }
    }
}

/// The value a lone accumulator statement appends, when it is exactly
/// `s += value` or `s = s + value` and neither side reads `s` again.
fn append_value(stmt: &Stmt, local: LocalId) -> Option<Expr> {
    match &stmt.kind {
        StmtKind::AugAssign { target, op, value } if *target == local && *op == BinOp::Add => {
            if expr_use(value, local).mentioned {
                return None;
            }
            Some(value.clone())
        }
        StmtKind::Assign { target, value } if *target == local => {
            let ExprKind::Bin { op, lhs, rhs } = &value.kind else {
                return None;
            };
            if *op != BinOp::Add || !matches!(&lhs.kind, ExprKind::Local(id) if *id == local) {
                return None;
            }
            if expr_use(rhs, local).mentioned {
                return None;
            }
            Some((**rhs).clone())
        }
        _ => None,
    }
}

/// Validates a loop as the accumulator loop and returns the positions of its
/// append statements inside the body.
fn accumulator_loop(stmt: &Stmt, local: LocalId) -> Option<Vec<usize>> {
    let (header, body, else_body): (Vec<&Expr>, &[Stmt], &[Stmt]) = match &stmt.kind {
        StmtKind::While {
            cond,
            body,
            else_body,
        } => (vec![cond], body, else_body),
        StmtKind::ForRange {
            var,
            start,
            stop,
            step,
            body,
            else_body,
        } => {
            if *var == local {
                return None;
            }
            let mut header = vec![start, stop];
            if let Some(step) = step {
                header.push(step);
            }
            (header, body, else_body)
        }
        StmtKind::ForEach {
            var,
            iterable,
            body,
            else_body,
        } => {
            if *var == local {
                return None;
            }
            (vec![iterable], body, else_body)
        }
        _ => return None,
    };
    if header.iter().any(|expr| expr_use(expr, local).mentioned) {
        return None;
    }
    // The `else` clause runs before the builder is finished, so it must not
    // observe the accumulator.
    if stmts_use(else_body, local).mentioned {
        return None;
    }
    let mut appends = Vec::new();
    for (index, stmt) in body.iter().enumerate() {
        if append_value(stmt, local).is_some() {
            appends.push(index);
        } else if stmt_use(stmt, local).mentioned {
            return None;
        }
    }
    if appends.is_empty() {
        return None;
    }
    Some(appends)
}

struct Plan {
    /// Position of `s = <str>` in the function body.
    init: usize,
    /// Position of the accumulator loop in the function body.
    loop_index: usize,
    /// Positions of the append statements inside the loop body.
    appends: Vec<usize>,
    accumulator: LocalId,
    /// Hidden builder local, assigned by [`apply_function`].
    builder: LocalId,
}

/// One accumulator rewritten inside a shared loop body.
struct LoopPlan {
    accumulator: LocalId,
    builder: LocalId,
    appends: Vec<usize>,
}

/// Collects every provable accumulator of one function.
fn find_plans(function: &Function) -> Vec<Plan> {
    let body = &function.body;
    let mut plans = Vec::new();
    let mut planned: Vec<LocalId> = Vec::new();
    for (init, stmt) in body.iter().enumerate() {
        let StmtKind::Assign { target, value } = &stmt.kind else {
            continue;
        };
        let accumulator = *target;
        if planned.contains(&accumulator) {
            continue;
        }
        let Some(info) = function.locals.get(accumulator as usize) else {
            continue;
        };
        // Only a statically proved `str` local is a candidate; inference keeps
        // the type `Any` as soon as one assignment is not provably a string.
        if info.ty != Type::Str || value.ty != Type::Str {
            continue;
        }
        // The seed must be the first use: an earlier read or write (including
        // a loop over the local) rules the pattern out.
        if body[..init]
            .iter()
            .any(|stmt| stmt_use(stmt, accumulator).mentioned)
        {
            continue;
        }
        let mut found = None;
        for (index, candidate) in body.iter().enumerate().skip(init + 1) {
            if let Some(appends) = accumulator_loop(candidate, accumulator) {
                found = Some((index, appends));
                break;
            }
            // Anything that touches the local before the accumulator loop
            // would observe the invariant `s` is only appended in the loop.
            if stmt_use(candidate, accumulator).mentioned {
                break;
            }
        }
        let Some((loop_index, appends)) = found else {
            continue;
        };
        // The pass is only useful (and only applied) when the final value is
        // read afterwards; the builder is finished right after the loop.
        let read_after = body[loop_index + 1..]
            .iter()
            .any(|stmt| stmt_use(stmt, accumulator).read);
        if !read_after {
            continue;
        }
        plans.push(Plan {
            init,
            loop_index,
            appends,
            accumulator,
            builder: 0,
        });
        planned.push(accumulator);
    }
    plans
}

fn apply_function(function: &mut Function) {
    let mut plans = find_plans(function);
    if plans.is_empty() {
        return;
    }
    // One hidden builder local per accumulator; the name is only diagnostic.
    for (number, plan) in plans.iter_mut().enumerate() {
        let base = function.locals[plan.accumulator as usize].name.clone();
        plan.builder = function.locals.len() as LocalId;
        function.locals.push(LocalInfo {
            name: format!("{base}#builder{number}"),
            ty: Type::Any,
        });
    }

    let mut loops: HashMap<usize, Vec<LoopPlan>> = HashMap::new();
    let mut starts: HashMap<usize, Vec<(LocalId, LocalId)>> = HashMap::new();
    let mut finishes: HashMap<usize, Vec<(LocalId, LocalId)>> = HashMap::new();
    for plan in plans {
        starts
            .entry(plan.init)
            .or_default()
            .push((plan.accumulator, plan.builder));
        finishes
            .entry(plan.loop_index)
            .or_default()
            .push((plan.accumulator, plan.builder));
        loops.entry(plan.loop_index).or_default().push(LoopPlan {
            accumulator: plan.accumulator,
            builder: plan.builder,
            appends: plan.appends,
        });
    }

    let body = std::mem::take(&mut function.body);
    let inserts = starts.values().map(|entries| entries.len()).sum::<usize>()
        + finishes
            .values()
            .map(|entries| entries.len())
            .sum::<usize>();
    let mut rewritten = Vec::with_capacity(body.len() + inserts);
    for (index, mut stmt) in body.into_iter().enumerate() {
        if let Some(entries) = loops.get(&index) {
            rewrite_appends(&mut stmt, entries);
        }
        let line = stmt.line;
        rewritten.push(stmt);
        if let Some(entries) = starts.get(&index) {
            for &(accumulator, builder) in entries {
                rewritten.push(builder_new_stmt(accumulator, builder, line));
            }
        }
        if let Some(entries) = finishes.get(&index) {
            for &(accumulator, builder) in entries {
                rewritten.push(builder_finish_stmt(accumulator, builder, line));
            }
        }
    }
    function.body = rewritten;
}

fn builder_new_stmt(accumulator: LocalId, builder: LocalId, line: u32) -> Stmt {
    let init = Expr::new(
        ExprKind::StrBuilderNew {
            init: Box::new(Expr::new(ExprKind::Local(accumulator), Type::Str, line)),
        },
        Type::Any,
        line,
    );
    Stmt::new(
        StmtKind::Assign {
            target: builder,
            value: init,
        },
        line,
    )
}

fn builder_finish_stmt(accumulator: LocalId, builder: LocalId, line: u32) -> Stmt {
    let finish = Expr::new(
        ExprKind::StrBuilderFinish {
            builder: Box::new(Expr::new(ExprKind::Local(builder), Type::Any, line)),
        },
        Type::Str,
        line,
    );
    Stmt::new(
        StmtKind::Assign {
            target: accumulator,
            value: finish,
        },
        line,
    )
}

/// Replaces every append statement of the validated loop with a builder
/// append; the surrounding statements keep their order.
fn rewrite_appends(stmt: &mut Stmt, entries: &[LoopPlan]) {
    let body = match &mut stmt.kind {
        StmtKind::While { body, .. }
        | StmtKind::ForRange { body, .. }
        | StmtKind::ForEach { body, .. } => body,
        _ => return,
    };
    for entry in entries {
        for &index in &entry.appends {
            let Some(value) = append_value(&body[index], entry.accumulator) else {
                continue;
            };
            let line = body[index].line;
            let append = Expr::new(
                ExprKind::StrBuilderAppend {
                    builder: Box::new(Expr::new(ExprKind::Local(entry.builder), Type::Any, line)),
                    value: Box::new(value),
                },
                Type::Any,
                line,
            );
            body[index] = Stmt::new(
                StmtKind::Assign {
                    target: entry.builder,
                    value: append,
                },
                line,
            );
        }
    }
}
