//! Compiles the resolved syntax tree to closures.
//!
//! Each node becomes a closure that evaluates it, with what is known
//! statically (the operator, where a variable lives, whether a block needs
//! a scope) decided once here rather than on every evaluation. The closures
//! call into the [`Machine`] for everything else, so the semantics live
//! there and in [`value`](crate::value).

use std::fmt;
use std::rc::Rc;

use crate::ast::{self, Ann, Binding, Block, Expr, Loc, Op, Place, Ref, Stmt, Sym};
use crate::interp::{Machine, R, Step, apply_int};
use crate::value::{Lazy, LazyState, Scope, Slot, Value};

/// Evaluates an expression, or runs a block, in a scope.
pub type Code = Box<dyn Fn(&mut Machine, &Rc<Scope>) -> R<Value>>;

/// Runs a statement in a scope.
type Run = Box<dyn Fn(&mut Machine, &Rc<Scope>) -> R<()>>;

/// A compiled function.
pub struct Func {
    pub name: Sym,
    pub params: usize,
    /// Each parameter's slot in the body's scope.
    pub param_slots: Box<[u32]>,
    /// Slots in the body's scope.
    pub size: usize,
    /// Runs in a new scope of `size` slots with the parameters bound.
    pub body: Code,
}

impl fmt::Debug for Func {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Func({:?})", self.name)
    }
}

/// A compiled program, ready to [`run`](crate::Interpreter::run).
pub struct Program {
    stmts: Box<[Run]>,
}

impl Program {
    pub(crate) fn run(&self, m: &mut Machine, env: &Rc<Scope>) -> R<()> {
        for stmt in self.stmts.iter() {
            stmt(m, env)?;
        }
        Ok(())
    }
}

/// Compiles a resolved program (see [`resolve`](crate::resolve)).
pub fn program(stmts: ast::Program) -> Program {
    Program {
        stmts: stmts.into_vec().into_iter().map(stmt).collect(),
    }
}

// --- Blocks ---

/// A block run in the scope it is given. It evaluates to its last statement
/// if that is an expression.
fn block(block: Block) -> Code {
    let mut stmts = block.stmts.into_vec();
    let tail = match stmts.pop() {
        Some(Stmt::Expr(e)) => Some(expr(e)),
        Some(s) => {
            stmts.push(s);
            None
        }
        None => None,
    };
    let runs: Box<[Run]> = stmts.into_iter().map(stmt).collect();
    match (runs.is_empty(), tail) {
        (true, Some(tail)) => tail,
        (true, None) => Box::new(|_, _| Ok(Value::None)),
        (false, Some(tail)) => Box::new(move |m, env| {
            for run in runs.iter() {
                run(m, env)?;
            }
            tail(m, env)
        }),
        (false, None) => Box::new(move |m, env| {
            for run in runs.iter() {
                run(m, env)?;
            }
            Ok(Value::None)
        }),
    }
}

/// The body of an `if` or a loop: it runs in a scope of its own if it binds
/// anything.
struct Body {
    code: Code,
    /// The size of its scope, if it has one.
    size: Option<usize>,
}

impl Body {
    fn new(b: Block) -> Body {
        let size = (b.binds > 0).then_some(b.size);
        Body {
            code: block(b),
            size,
        }
    }

    fn run(&self, m: &mut Machine, env: &Rc<Scope>) -> R<Value> {
        match self.size {
            None => (self.code)(m, env),
            Some(size) => (self.code)(m, &Scope::with_size(Some(env.clone()), size)),
        }
    }

    /// Like [`run`](Self::run), for a loop. A scope nothing kept hold of
    /// is unobservable, so it is cleared and reused for the next iteration.
    fn run_again(
        &self,
        m: &mut Machine,
        env: &Rc<Scope>,
        spare: &mut Option<Rc<Scope>>,
    ) -> R<Value> {
        let Some(size) = self.size else {
            return (self.code)(m, env);
        };
        let scope = spare
            .take()
            .unwrap_or_else(|| Scope::with_size(Some(env.clone()), size));
        let result = (self.code)(m, &scope);
        if Rc::strong_count(&scope) == 1 && Rc::weak_count(&scope) == 0 {
            scope.clear();
            *spare = Some(scope);
        }
        result
    }
}

// --- Statements ---

fn stmt(stmt: Stmt) -> Run {
    match stmt {
        Stmt::Expr(e) => {
            let e = expr(e);
            Box::new(move |m, env| e(m, env).map(drop))
        }
        Stmt::Let(Binding {
            name,
            slot,
            ann,
            expr: e,
        }) => {
            let e = expr(e);
            if matches!(ann, Ann::Any) {
                Box::new(move |m, env| {
                    let v = e(m, env)?;
                    env.set(slot, v);
                    Ok(())
                })
            } else {
                Box::new(move |m, env| {
                    let v = e(m, env)?;
                    m.validate(name, &v, &ann)?;
                    env.set(slot, v);
                    Ok(())
                })
            }
        }
        Stmt::Pin(Binding {
            name,
            slot,
            ann,
            expr: e,
        }) => {
            let e = expr(e);
            Box::new(move |m, env| m.pin(name, slot, &e, &ann, env))
        }
        Stmt::Assign(var, e) => {
            let e = expr(e);
            Box::new(move |m, env| {
                let v = e(m, env)?;
                match m.find(env, &var) {
                    Some((scope, slot)) => {
                        scope.set(slot, v);
                        Ok(())
                    }
                    None => m.unassignable(var.name),
                }
            })
        }
        Stmt::AssignPath(var, path, e) => assign_path(var, path, e),
        Stmt::Reset(name) => Box::new(move |m, _| {
            m.reset(name);
            Ok(())
        }),
        Stmt::TypeDef(name, fields) => Box::new(move |m, _| m.define_type(name, &fields)),
        Stmt::Commit(var) => Box::new(move |m, env| {
            let branch = m.settle(&var, env, "commit")?;
            Scope::merge_chain(&branch.origin, &branch.env);
            Ok(())
        }),
        Stmt::Discard(var) => Box::new(move |m, env| m.settle(&var, env, "discard").map(drop)),
        Stmt::Given(cond) => {
            let cond = expr(cond);
            Box::new(move |m, env| {
                let v = cond(m, env)?;
                let holds = m.truthy(&v)?;
                m.given(holds)
            })
        }
        Stmt::Func(def) => {
            let def = Rc::try_unwrap(def).expect("a new function is not shared");
            let slot = def.slot;
            let f = Rc::new(Func {
                name: def.name,
                params: def.params.len(),
                param_slots: def.param_slots,
                size: def.body.size,
                body: block(def.body),
            });
            Box::new(move |_, env| {
                env.set_slot(slot, Slot::Fn(f.clone()));
                Ok(())
            })
        }
    }
}

enum PlaceCode {
    Index(Code),
    Field(Sym),
}

/// Paths longer than this have their steps collected on the heap.
const INLINE_STEPS: usize = 4;

/// `name[i].field[j] = e;`
fn assign_path(var: Ref, path: Box<[Place]>, e: Expr) -> Run {
    let e = expr(e);
    let path: Box<[PlaceCode]> = path
        .into_vec()
        .into_iter()
        .map(|p| match p {
            Place::Index(i) => PlaceCode::Index(expr(i)),
            Place::Field(f) => PlaceCode::Field(f),
        })
        .collect();
    Box::new(move |m, env| {
        // The value first, then the indices left to right
        let v = e(m, env)?;
        let long = path.len() > INLINE_STEPS;
        let mut inline = [Step::Index(0); INLINE_STEPS];
        let mut heap = Vec::new();
        for (k, place) in path.iter().enumerate() {
            let step = match place {
                PlaceCode::Index(i) => {
                    let i = i(m, env)?;
                    Step::Index(m.int_of(&i, "array index")?)
                }
                PlaceCode::Field(f) => Step::Field(*f),
            };
            if long {
                heap.push(step);
            } else {
                inline[k] = step;
            }
        }
        let steps = if long {
            &heap[..]
        } else {
            &inline[..path.len()]
        };
        let Some((scope, slot)) = m.find(env, &var) else {
            return m.unassignable(var.name);
        };
        // Taking the value out leaves it uniquely owned, so the write
        // happens in place unless someone else shares it.
        let mut target = scope.take(slot);
        let result = m.write_path(&mut target, steps, v);
        scope.set(slot, target);
        result
    })
}

// --- Expressions ---

fn expr(e: Expr) -> Code {
    match e {
        Expr::Int(n) => Box::new(move |_, _| Ok(Value::Int(n))),
        Expr::Float(x) => Box::new(move |_, _| Ok(Value::Float(x))),
        Expr::Str(s) => Box::new(move |_, _| Ok(Value::Str(s.clone()))),
        Expr::Var(var) => variable(var),
        Expr::Open(None) => Box::new(|_, _| Ok(Value::Lazy(Lazy::new(LazyState::Open(None))))),
        Expr::Open(Some(bounds)) => {
            let (lo, hi) = *bounds;
            let (lo, hi) = (expr(lo), expr(hi));
            Box::new(move |m, env| {
                let lo = lo(m, env)?;
                let hi = hi(m, env)?;
                Ok(Value::Lazy(Lazy::new(LazyState::Open(Some((lo, hi))))))
            })
        }
        Expr::Binary(op, l, r) => binary(op, operand(*l), operand(*r)),
        Expr::Call(var, args) => {
            let args: Box<[Code]> = args.into_vec().into_iter().map(expr).collect();
            Box::new(move |m, env| m.call(&var, &args, env))
        }
        Expr::StructInit(ty, fields) => {
            let fields: Box<[(Sym, Code)]> = fields
                .into_vec()
                .into_iter()
                .map(|(k, e)| (k, expr(e)))
                .collect();
            Box::new(move |m, env| m.struct_init(ty, &fields, env))
        }
        e @ (Expr::Member(..) | Expr::Index(..)) => access(e),
        Expr::Array(items) => {
            let items: Box<[Code]> = items.into_vec().into_iter().map(expr).collect();
            Box::new(move |m, env| {
                let mut values = Vec::with_capacity(items.len());
                for item in items.iter() {
                    values.push(item(m, env)?);
                }
                Ok(Value::Array(Rc::new(values)))
            })
        }
        Expr::Observe(inner) => {
            let inner = expr(*inner);
            Box::new(move |m, env| {
                let v = inner(m, env)?;
                m.collapse(&v)
            })
        }
        Expr::Fork(b) => {
            let body = block(b);
            Box::new(move |m, env| m.fork(&body, env))
        }
        Expr::If(cond, then_b, else_b) => {
            let cond = expr(*cond);
            let then_b = Body::new(then_b);
            let else_b = else_b.map(Body::new);
            Box::new(move |m, env| {
                let c = cond(m, env)?;
                if m.truthy(&c)? {
                    then_b.run(m, env)
                } else if let Some(else_b) = &else_b {
                    else_b.run(m, env)
                } else {
                    Ok(Value::None)
                }
            })
        }
        Expr::Repeat(count, b) => {
            let count = expr(*count);
            let body = Body::new(b);
            Box::new(move |m, env| {
                let n = count(m, env)?;
                let n = m.int_of(&n, "repeat count")?;
                let mut result = Value::None;
                let mut spare = None;
                for _ in 0..n {
                    result = body.run_again(m, env, &mut spare)?;
                }
                Ok(result)
            })
        }
        Expr::While(cond, b) => {
            let cond = expr(*cond);
            let body = Body::new(b);
            Box::new(move |m, env| {
                let mut result = Value::None;
                let mut spare = None;
                loop {
                    let c = cond(m, env)?;
                    if !m.truthy(&c)? {
                        break Ok(result);
                    }
                    result = body.run_again(m, env, &mut spare)?;
                }
            })
        }
        Expr::Multiverse(count, b) => {
            let count = expr(*count);
            let body = block(b);
            Box::new(move |m, env| m.multiverse(&count, &body, env))
        }
    }
}

fn variable(var: Ref) -> Code {
    if let [Loc { up: 0, slot }] = *var.locs {
        return Box::new(move |m, env| match env.read(slot) {
            Some(v) => Ok(v),
            None => m.lookup(env, &var),
        });
    }
    Box::new(move |m, env| match env.read_at(&var.locs) {
        Some(v) => Ok(v),
        None => m.lookup(env, &var),
    })
}

/// An operand of a binary operator. Constants and variables are evaluated
/// in place rather than by a call.
enum Operand {
    Int(i64),
    Float(f64),
    /// A variable in the current scope (its only location).
    Local(u32, Ref),
    /// A variable with one possible location further out.
    Outer(Loc, Ref),
    Code(Code),
}

fn operand(e: Expr) -> Operand {
    match e {
        Expr::Int(n) => Operand::Int(n),
        Expr::Float(x) => Operand::Float(x),
        Expr::Var(var) => match *var.locs {
            [Loc { up: 0, slot }] => Operand::Local(slot, var),
            [loc] => Operand::Outer(loc, var),
            _ => Operand::Code(variable(var)),
        },
        e => Operand::Code(expr(e)),
    }
}

impl Operand {
    #[inline(always)]
    fn eval(&self, m: &mut Machine, env: &Rc<Scope>) -> R<Value> {
        match self {
            Operand::Int(n) => Ok(Value::Int(*n)),
            Operand::Float(x) => Ok(Value::Float(*x)),
            Operand::Local(slot, var) => match env.read(*slot) {
                Some(v) => Ok(v),
                None => m.lookup(env, var),
            },
            Operand::Outer(loc, var) => match env.read_at(std::slice::from_ref(loc)) {
                Some(v) => Ok(v),
                None => m.lookup(env, var),
            },
            Operand::Code(code) => code(m, env),
        }
    }
}

fn binary(op: Op, l: Operand, r: Operand) -> Code {
    // One closure per operator, so `apply_int` is specialised to it
    macro_rules! with_op {
        ($($op:ident)*) => {
            match op {
                $(Op::$op => Box::new(move |m, env| {
                    let a = l.eval(m, env)?;
                    let b = r.eval(m, env)?;
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => apply_int(Op::$op, x, y).map(Value::Int),
                        (a, b) => m.lazy(Op::$op, &a, &b),
                    }
                }),)*
                Op::Abs | Op::Sqrt | Op::ToFloat | Op::ToInt => unreachable!("'{op}' is unary"),
            }
        };
    }
    with_op!(Or And Eq Gt Lt Add Sub Mul Div Min Max)
}

/// A step of a member or index access.
enum Access {
    Field(Sym),
    Index(Code),
}

/// `a.b[i]...`. When `a` is a variable and the indices can't change any
/// variable, the value is read where it lives, without copying the
/// containers on the way.
fn access(e: Expr) -> Code {
    let mut steps = Vec::new();
    let mut node = e;
    let mut pure = true;
    let root = loop {
        node = match node {
            Expr::Member(base, field) => {
                steps.push(Access::Field(field));
                *base
            }
            Expr::Index(base, index) => {
                pure &= no_writes(&index);
                steps.push(Access::Index(expr(*index)));
                *base
            }
            root => break root,
        };
    };
    steps.reverse();
    let steps: Box<[Access]> = steps.into();
    match root {
        Expr::Var(var) if pure => Box::new(move |m, env| {
            let Some((scope, slot)) = m.find(env, &var) else {
                return m.undefined(var.name);
            };
            let vars = scope.vars.borrow();
            match &vars[slot as usize].slot {
                Slot::Value(v) => walk(m, env, v, &steps),
                _ => {
                    drop(vars);
                    walk(m, env, &scope.get(slot), &steps)
                }
            }
        }),
        root => {
            let root = expr(root);
            Box::new(move |m, env| {
                // Each step owns its value: the next index may change any variable
                let mut cur = root(m, env)?;
                for step in steps.iter() {
                    cur = match step {
                        Access::Field(f) => m.member(&cur, *f)?,
                        Access::Index(index) => {
                            let i = index(m, env)?;
                            let i = m.int_of(&i, "array index")?;
                            m.element_ref(&cur, i)?.clone()
                        }
                    };
                }
                Ok(cur)
            })
        }
    }
}

fn walk(m: &mut Machine, env: &Rc<Scope>, mut cur: &Value, steps: &[Access]) -> R<Value> {
    for step in steps {
        cur = match step {
            Access::Field(f) => m.member_ref(cur, *f)?,
            Access::Index(index) => {
                let i = index(m, env)?;
                let i = m.int_of(&i, "array index")?;
                m.element_ref(cur, i)?
            }
        };
    }
    Ok(cur.clone())
}

/// Whether evaluating `e` can't bind or assign a variable: it calls nothing
/// and runs no block. Such an expression only reads scopes.
fn no_writes(e: &Expr) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Var(_) | Expr::Open(None) => true,
        Expr::Open(Some(b)) => no_writes(&b.0) && no_writes(&b.1),
        Expr::Binary(_, a, b) | Expr::Index(a, b) => no_writes(a) && no_writes(b),
        Expr::Member(e, _) | Expr::Observe(e) => no_writes(e),
        Expr::Array(items) => items.iter().all(no_writes),
        Expr::StructInit(_, fields) => fields.iter().all(|(_, e)| no_writes(e)),
        Expr::Call(..)
        | Expr::Fork(_)
        | Expr::If(..)
        | Expr::Repeat(..)
        | Expr::While(..)
        | Expr::Multiverse(..) => false,
    }
}
