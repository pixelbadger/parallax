import re
import sys
import random
import copy
import threading

# ==========================================
# 0. ERRORS
# ==========================================

class SPLError(Exception): pass
class SPLSyntaxError(SPLError): pass
class SPLRuntimeError(SPLError): pass

class Rejected(Exception):
    """Raised by a failed `given` to throw away the current universe."""

# ==========================================
# 1. RUNTIME STATE & VALUES
# ==========================================

class State:
    OPEN, RESOLVED, COLLAPSED = "OPEN", "RESOLVED", "COLLAPSED"
    BRANCH, STRUCT = "BRANCH", "STRUCT"

class Value:
    # OPEN:     val is None (0..99) or a (lo, hi) pair of Values
    # RESOLVED: val is an (op, [operand Values]) pair, evaluated on collapse
    def __init__(self, val, state=State.COLLAPSED, type_hint="Any", env=None, origin=None):
        self.val = val
        self.state = state
        self.type_hint = type_hint
        self.env = env        # For Branches: the forked timeline's scope
        self.origin = origin  # For Branches: the scope the fork was created in
        self.settled = False  # For Branches: set once committed or discarded

    def __deepcopy__(self, memo):
        # Collapsed values never change again, so timelines can share them
        if self.state == State.COLLAPSED: return self
        new = Value.__new__(Value)
        memo[id(self)] = new
        new.__dict__ = copy.deepcopy(self.__dict__, memo)
        return new

    def __repr__(self):
        if self.state == State.OPEN: return f"<{self.type_hint} (Open)>"
        if self.state == State.RESOLVED: return f"<{self.type_hint} (Future)>"
        if self.state == State.BRANCH: return f"<Branch: {self.val}>"
        if self.state == State.STRUCT: return f"Struct<{self.type_hint}>"
        return f"<{self.val}>"

def deref(v):
    # Branches transparently behave like the value their fork block produced
    while v.state == State.BRANCH: v = v.val
    return v

# Global Persistence for 'pin' logic. Lives for the lifetime of one
# interpreter, so pinned values survive re-seeding, forks and universes.
class GlobalMemo:
    def __init__(self): self.pinned_values = {}
    def get_pin(self, name): return self.pinned_values.get(name)
    def set_pin(self, name, val): self.pinned_values[name] = val
    def clear_pin(self, name): self.pinned_values.pop(name, None)

class Environment:
    def __init__(self, parent=None):
        self.vars = {}
        self.parent = parent
        self.written = set() # Names bound or assigned here since the last clone

    def get(self, name):
        if name in self.vars: return self.vars[name]
        if self.parent: return self.parent.get(name)
        raise SPLRuntimeError(f"Undefined variable '{name}'")

    def set(self, name, val):
        self.vars[name] = val
        self.written.add(name)

    def assign(self, name, val):
        if name in self.vars: return self.set(name, val)
        if self.parent: return self.parent.assign(name, val)
        raise SPLRuntimeError(f"Cannot assign to undefined variable '{name}'")

    def clone(self, memo=None):
        # Copy the whole scope chain so a fork cannot reach back and change
        # or collapse values that belong to its parent timeline. Registering
        # each copy in the memo lets closures follow their scope into the copy.
        memo = {} if memo is None else memo
        new_env = Environment(self.parent.clone(memo) if self.parent else None)
        memo[id(self)] = new_env
        for name, v in self.vars.items():
            shared = isinstance(v, NativeFunc) or (isinstance(v, Value) and v.state == State.COLLAPSED)
            new_env.vars[name] = v if shared else copy.deepcopy(v, memo)
        return new_env

    def update_from(self, other_env):
        # Merge only what the other timeline actually wrote, so changes made
        # here after the fork are not overwritten with stale copies.
        for name in other_env.written: self.set(name, other_env.vars[name])
        if self.parent and other_env.parent: self.parent.update_from(other_env.parent)

# ==========================================
# 2. LEXER & PARSER
# ==========================================

KEYWORDS = {
    'fn': 'FN', 'let': 'LET', 'pin': 'PIN', 'reset': 'RESET', 'type': 'TYPE_DEF',
    'commit': 'COMMIT', 'discard': 'DISCARD', 'fork': 'FORK', 'observe': 'OBSERVE',
    'open': 'OPEN', 'if': 'IF', 'else': 'ELSE', 'repeat': 'REPEAT', 'while': 'WHILE',
    'multiverse': 'MULTIVERSE', 'given': 'GIVEN',
}

TOKEN_TYPES = [
    # 1. Skipped: comments (hash until newline) and whitespace
    ('COMMENT', r'#[^\n]*'), ('WS', r'\s+'),

    # 2. Identifiers (keywords are split out after matching) & literals
    ('ID', r'[a-zA-Z_][a-zA-Z0-9_]*'), ('NUMBER', r'\d+'), ('STRING', r'"[^"\n]*"'),

    # 3. Operators (longest first) & punctuation
    ('OP', r'==|&&|\|\||[+\-*/<>]'), ('EQ', r'='),
    ('DOT', r'\.'), ('COLON', r':'), ('Q_MARK', r'\?'), ('TILDE', r'~'),
    ('LPAREN', r'\('), ('RPAREN', r'\)'),
    ('LBRACE', r'\{'), ('RBRACE', r'\}'), ('SEMI', r';'), ('COMMA', r','),
]
TOKEN_RE = [(name, re.compile(pattern)) for name, pattern in TOKEN_TYPES]

def lex(code):
    tokens = []
    pos, line = 0, 1
    while pos < len(code):
        for name, regex in TOKEN_RE:
            m = regex.match(code, pos)
            if m: break
        else:
            snippet = code[pos:pos+10].replace('\n', '\\n')
            raise SPLSyntaxError(f"Line {line}: Illegal char '{snippet}...'")
        text = m.group(0)
        if name == 'ID': name = KEYWORDS.get(text, 'ID')
        if name not in ('WS', 'COMMENT'): tokens.append((name, text, line))
        line += text.count('\n')
        pos = m.end()
    return tokens

class AST: pass
class BinOp(AST):
    def __init__(self, left, op, right): self.left, self.op, self.right = left, op, right
class Num(AST):
    def __init__(self, val): self.val = val
class Str(AST):
    def __init__(self, val): self.val = val
class Var(AST):
    def __init__(self, name): self.name = name
class Let(AST):
    def __init__(self, name, expr, type_ann="Any"): self.name, self.expr, self.type_ann = name, expr, type_ann
class Pin(AST):
    def __init__(self, name, expr, type_ann="Any"): self.name, self.expr, self.type_ann = name, expr, type_ann
class Assign(AST):
    def __init__(self, name, expr): self.name, self.expr = name, expr
class Reset(AST):
    def __init__(self, name): self.name = name
class Func(AST):
    def __init__(self, name, params, body): self.name, self.params, self.body = name, params, body
class Call(AST):
    def __init__(self, func, args): self.func, self.args = func, args
class Observe(AST):
    def __init__(self, expr): self.expr = expr
class Open(AST):
    def __init__(self, lo=None, hi=None): self.lo, self.hi = lo, hi
class Block(AST):
    def __init__(self, stmts): self.stmts = stmts
class If(AST):
    def __init__(self, cond, then_b, else_b): self.cond, self.then_b, self.else_b = cond, then_b, else_b
class Repeat(AST):
    def __init__(self, count, block): self.count, self.block = count, block
class While(AST):
    def __init__(self, cond, block): self.cond, self.block = cond, block
class Fork(AST):
    def __init__(self, block): self.block = block
class Multiverse(AST):
    def __init__(self, count, block): self.count, self.block = count, block
class Given(AST):
    def __init__(self, cond): self.cond = cond
class Commit(AST):
    def __init__(self, name): self.name = name
class Discard(AST):
    def __init__(self, name): self.name = name
class StructDef(AST):
    def __init__(self, name, fields): self.name, self.fields = name, fields
class StructInit(AST):
    def __init__(self, name, fields): self.name, self.fields = name, fields
class MemberAccess(AST):
    def __init__(self, obj, member): self.obj, self.member = obj, member

# Nodes that produce a value when used as a statement (see `block` in the EBNF)
BLOCK_EXPRS = (If, Fork, Repeat, While, Multiverse)
EXPR_NODES = (BinOp, Num, Str, Var, Call, Observe, Open, StructInit, MemberAccess) + BLOCK_EXPRS

# Binary operator precedence, loosest first (see `expr` in the EBNF)
PRECEDENCE = [['||'], ['&&'], ['==', '>', '<'], ['+', '-'], ['*', '/']]

class Parser:
    def __init__(self, tokens):
        self.tokens = tokens
        self.pos = 0
        self.no_struct = False # True while parsing an `if`/loop/multiverse head

    def peek(self, offset=0):
        if self.pos + offset >= len(self.tokens): return "EOF"
        return self.tokens[self.pos + offset][0]

    def peek_text(self):
        return self.tokens[self.pos][1] if self.pos < len(self.tokens) else None

    def error(self, msg):
        if self.pos >= len(self.tokens): return SPLSyntaxError(f"Unexpected end of file. {msg}")
        _, text, line = self.tokens[self.pos]
        return SPLSyntaxError(f"Line {line}: {msg}, got '{text}'")

    def consume(self, type_name):
        if self.peek() != type_name: raise self.error(f"Expected {type_name}")
        self.pos += 1
        return self.tokens[self.pos-1][1]

    def parse_program(self):
        stmts = []
        while self.pos < len(self.tokens): stmts.append(self.parse_stmt())
        return stmts

    def parse_type(self):
        prefix = ""
        if self.peek() in ['Q_MARK', 'TILDE']: prefix = self.consume(self.peek())
        return prefix + self.consume('ID')

    def with_structs(self, allowed, parse):
        saved, self.no_struct = self.no_struct, not allowed
        try: return parse()
        finally: self.no_struct = saved

    def parse_block(self):
        self.consume('LBRACE')
        stmts = self.with_structs(True, lambda: self.parse_stmts_until('RBRACE'))
        self.consume('RBRACE')
        return Block(stmts)

    def parse_stmts_until(self, closer):
        stmts = []
        while self.peek() != closer: stmts.append(self.parse_stmt())
        return stmts

    def parse_head(self):
        # The expression before a block: `if c {`, `repeat n {`, ...
        return self.with_structs(False, self.parse_expr)

    def parse_ident_list(self, closer):
        names = []
        while self.peek() != closer:
            names.append(self.consume('ID'))
            if self.peek() != closer: self.consume('COMMA')
        return names

    def parse_binding(self, node_cls, keyword):
        self.consume(keyword); name = self.consume('ID')
        ann = "Any"
        if self.peek() == 'COLON': self.consume('COLON'); ann = self.parse_type()
        self.consume('EQ'); expr = self.parse_expr(); self.consume('SEMI')
        return node_cls(name, expr, ann)

    def parse_stmt(self):
        t = self.peek()
        if t == 'LET': return self.parse_binding(Let, 'LET')
        if t == 'PIN': return self.parse_binding(Pin, 'PIN')
        if t == 'ID' and self.peek(1) == 'EQ':
            name = self.consume('ID'); self.consume('EQ')
            expr = self.parse_expr(); self.consume('SEMI')
            return Assign(name, expr)
        if t == 'GIVEN':
            self.consume('GIVEN'); cond = self.parse_expr(); self.consume('SEMI')
            return Given(cond)
        if t in ('RESET', 'COMMIT', 'DISCARD'):
            self.consume(t); name = self.consume('ID'); self.consume('SEMI')
            return {'RESET': Reset, 'COMMIT': Commit, 'DISCARD': Discard}[t](name)
        if t == 'TYPE_DEF':
            self.consume('TYPE_DEF'); name = self.consume('ID'); self.consume('EQ')
            self.consume('LBRACE'); fields = self.parse_ident_list('RBRACE')
            self.consume('RBRACE'); self.consume('SEMI')
            return StructDef(name, fields)
        if t == 'FN':
            self.consume('FN'); name = self.consume('ID'); self.consume('LPAREN')
            params = self.parse_ident_list('RPAREN')
            self.consume('RPAREN'); self.consume('EQ'); body = self.parse_block()
            return Func(name, params, body)

        # expr_stmt: block expressions need no ';', nor does a block's trailing expr
        expr = self.parse_expr()
        if self.peek() == 'SEMI': self.consume('SEMI')
        elif not isinstance(expr, BLOCK_EXPRS) and self.peek() != 'RBRACE':
            raise self.error("Expected ';'")
        return expr

    def parse_expr(self, level=0):
        if level == len(PRECEDENCE): return self.parse_postfix()
        left = self.parse_expr(level + 1)
        while self.peek() == 'OP' and self.peek_text() in PRECEDENCE[level]:
            op = self.consume('OP')
            left = BinOp(left, op, self.parse_expr(level + 1))
        return left

    def parse_postfix(self):
        node = self.parse_primary()
        while self.peek() == 'DOT':
            self.consume('DOT'); node = MemberAccess(node, self.consume('ID'))
        return node

    def parse_args(self):
        self.consume('LPAREN'); args = []
        if self.peek() != 'RPAREN':
            args.append(self.parse_expr())
            while self.peek() == 'COMMA': self.consume('COMMA'); args.append(self.parse_expr())
        self.consume('RPAREN')
        return args

    def parse_primary(self):
        t = self.peek()
        if t == 'NUMBER': return Num(int(self.consume('NUMBER')))
        if t == 'STRING': return Str(self.consume('STRING')[1:-1])
        if t == 'OPEN':
            self.consume('OPEN')
            if self.peek() != 'LPAREN': return Open()
            args = self.with_structs(True, self.parse_args)
            if len(args) != 2: raise self.error("open(...) takes exactly two bounds (lo, hi)")
            return Open(*args)
        if t == 'FORK': self.consume('FORK'); return Fork(self.parse_block())
        if t == 'OBSERVE': self.consume('OBSERVE'); return Observe(self.parse_expr())
        if t == 'IF':
            self.consume('IF')
            cond = self.parse_head(); then_b = self.parse_block()
            else_b = None
            if self.peek() == 'ELSE': self.consume('ELSE'); else_b = self.parse_block()
            return If(cond, then_b, else_b)
        if t in ('REPEAT', 'WHILE', 'MULTIVERSE'):
            self.consume(t)
            head = self.parse_head()
            return {'REPEAT': Repeat, 'WHILE': While, 'MULTIVERSE': Multiverse}[t](head, self.parse_block())
        if t == 'ID':
            name = self.consume('ID')
            if self.peek() == 'LBRACE' and not self.no_struct:
                self.consume('LBRACE')
                def fields():
                    out = {}
                    while self.peek() != 'RBRACE':
                        k = self.consume('ID'); self.consume('COLON'); out[k] = self.parse_expr()
                        if self.peek() != 'RBRACE': self.consume('COMMA')
                    return out
                f = self.with_structs(True, fields)
                self.consume('RBRACE')
                return StructInit(name, f)
            if self.peek() == 'LPAREN': return Call(name, self.with_structs(True, self.parse_args))
            return Var(name)
        if t == 'LPAREN':
            self.consume('LPAREN')
            expr = self.with_structs(True, self.parse_expr)
            self.consume('RPAREN')
            return expr
        raise self.error("Expected an expression")

# ==========================================
# 3. INTERPRETER ENGINE
# ==========================================

class NativeFunc:
    def __init__(self, func): self.func = func
    def __deepcopy__(self, memo): return self

class Closure:
    def __init__(self, name, params, body, env): self.name, self.params, self.body, self.env = name, params, body, env
    def __deepcopy__(self, memo):
        # A closure copied into a forked timeline sees that timeline's scope
        return Closure(self.name, self.params, self.body, memo.get(id(self.env), self.env))

COMPARISONS = ('==', '>', '<', '&&', '||')

def apply_op(op, vals):
    if op == '==' and all(isinstance(v, str) for v in vals): return int(vals[0] == vals[1])
    for v in vals:
        if not isinstance(v, int):
            raise SPLRuntimeError(f"'{op}' needs integers, got {', '.join(repr(x) for x in vals)}")
    if op == 'min': return min(vals)
    if op == 'max': return max(vals)
    if op == 'abs': return abs(vals[0])
    lv, rv = vals
    if op == '+': return lv + rv
    if op == '-': return lv - rv
    if op == '*': return lv * rv
    if op == '/':
        if rv == 0: raise SPLRuntimeError("Division by zero")
        return lv // rv
    if op == '>': return int(lv > rv)
    if op == '<': return int(lv < rv)
    if op == '==': return int(lv == rv)
    if op == '&&': return int(bool(lv) and bool(rv))
    if op == '||': return int(bool(lv) or bool(rv))
    raise SPLRuntimeError(f"Unknown operator '{op}'")

ENSEMBLE_FIELDS = ['n', 'rejected', 'total', 'mean', 'min', 'max', 'median', 'hits', 'rate']

def summarise(samples, rejected):
    """Statistics for one stream of integer universe results."""
    n = len(samples)
    stats = {'n': n, 'rejected': rejected}
    if n == 0:
        stats.update({k: None for k in ENSEMBLE_FIELDS[2:]})
    else:
        ordered = sorted(samples)
        hits = sum(1 for s in samples if s)
        stats.update(total=sum(samples), mean=round(sum(samples) / n), min=ordered[0],
                     max=ordered[-1], median=ordered[(n - 1) // 2], hits=hits,
                     rate=round(100 * hits / n))
    return Value({k: Value(v) for k, v in stats.items()}, State.STRUCT, type_hint="Ensemble")

def universe_seed(base, i):
    # Deterministic per (run seed, universe index): two multiverses of the same
    # size see the same random draws, so comparing them is a fair test.
    return random.Random(f"{base}/{i}").randrange(2**32)

class Interpreter:
    def __init__(self, seed=None):
        # Each interpreter owns its RNG, so runs are reproducible for a
        # given seed regardless of anything else using Python's `random`.
        self.rng = random.Random()
        self.memo = GlobalMemo()
        self.types = {'Ensemble': ENSEMBLE_FIELDS}
        self.env = Environment()
        self.universe_depth = 0
        self.dispatch = {}
        self.load_stdlib()
        self.reseed(seed)

    def reseed(self, seed):
        if seed is None: seed = random.SystemRandom().randrange(2**32)
        self.seed = seed
        self.rng.seed(seed)

    def load_stdlib(self):
        def n_print(args):
            print(*[self.format(self.collapse(a)) for a in args]); return Value(None)
        def n_seed(args):
            self.arity('seed', args, 1)
            self.reseed(self.int_of(args[0], "seed()")); print(f"[SYS] Seed: {self.seed}")
            return Value(None)
        def n_min(args): self.arity('min', args, 2); return self.lazy('min', args)
        def n_max(args): self.arity('max', args, 2); return self.lazy('max', args)
        def n_abs(args): self.arity('abs', args, 1); return self.lazy('abs', args)
        for name, f in [('print', n_print), ('seed', n_seed), ('min', n_min), ('max', n_max), ('abs', n_abs)]:
            self.env.set(name, NativeFunc(f))

    def arity(self, name, args, n):
        if len(args) != n: raise SPLRuntimeError(f"{name}() takes {n} argument(s), got {len(args)}")

    def int_of(self, v, what):
        v = self.collapse(v).val
        if not isinstance(v, int): raise SPLRuntimeError(f"{what} needs an integer, got {v!r}")
        return v

    def format(self, v):
        v = deref(v)
        if v.state == State.STRUCT:
            inner = ", ".join(f"{k}: {self.format(self.collapse(f))}" for k, f in v.val.items())
            return f"{v.type_hint} {{ {inner} }}"
        return "none" if v.val is None else str(v.val)

    def run(self, ast):
        try:
            for n in ast: self.visit(n, self.env)
            main = self.env.vars.get('main')
            if isinstance(main, Closure): self.visit(Call('main', []), self.env)
        except RecursionError:
            raise SPLRuntimeError("Recursion too deep")

    def visit(self, node, env):
        method = self.dispatch.get(type(node))
        if method is None:
            method = self.dispatch[type(node)] = getattr(self, f'visit_{type(node).__name__}')
        return method(node, env)

    def visit_Block(self, n, env):
        # A block evaluates to its last statement if that is an expression
        res = Value(None)
        for s in n.stmts:
            r = self.visit(s, env)
            res = r if isinstance(s, EXPR_NODES) else Value(None)
        return res

    def validate(self, name, val, ann):
        if ann == "Any": return
        state = deref(val).state
        if ann.startswith("?"):
            if state != State.OPEN: print(f"[WARN] {name}: Expected Open ({ann}), got {state}")
        elif ann.startswith("~"):
            if state != State.RESOLVED: print(f"[WARN] {name}: Expected Future ({ann}), got {state}")
        elif state in (State.OPEN, State.RESOLVED):
            print(f"[WARN] {name}: Expected Collapsed ({ann}), got {state}")

    def visit_Let(self, n, env):
        v = self.visit(n.expr, env)
        self.validate(n.name, v, n.type_ann)
        env.set(n.name, v); return v

    def visit_Assign(self, n, env):
        v = self.visit(n.expr, env)
        env.assign(n.name, v); return v

    def visit_Pin(self, n, env):
        saved = self.memo.get_pin(n.name)
        if saved is not None:
            print(f"[SYS] Pinned '{n.name}' retrieved.")
            env.set(n.name, saved); return saved
        v = self.collapse(self.visit(n.expr, env))
        self.validate(n.name, v, n.type_ann)
        self.memo.set_pin(n.name, v); env.set(n.name, v); return v

    def visit_Reset(self, n, env): self.memo.clear_pin(n.name); return Value(None)

    def visit_StructDef(self, n, env):
        if len(set(n.fields)) != len(n.fields): raise SPLRuntimeError(f"Duplicate field in type '{n.name}'")
        self.types[n.name] = n.fields; return Value(None)

    def visit_StructInit(self, n, env):
        if n.name not in self.types: raise SPLRuntimeError(f"Unknown type '{n.name}'")
        expected, given = set(self.types[n.name]), set(n.fields)
        if expected != given:
            missing, extra = sorted(expected - given), sorted(given - expected)
            raise SPLRuntimeError(f"Bad fields for '{n.name}': missing {missing}, unknown {extra}")
        fields = {k: self.visit(n.fields[k], env) for k in self.types[n.name]}
        return Value(fields, State.STRUCT, type_hint=n.name)

    def visit_MemberAccess(self, n, env):
        obj = self.visit(n.obj, env)
        target = deref(obj)
        if target.state == State.STRUCT and n.member in target.val: return target.val[n.member]
        raise SPLRuntimeError(f"Cannot access '{n.member}' on {obj}")

    def visit_Fork(self, n, env):
        # The fork draws from its own stream, derived from (but not advancing)
        # the parent's, so the parent timeline sees the same random draws
        # whether or not it forked.
        saved_state = self.rng.getstate()
        self.rng.seed(hash(saved_state[1])) # ints only: hash(None) varies by process before 3.12
        try:
            branch = env.clone()
            res = self.visit(n.block, branch)
        finally:
            self.rng.setstate(saved_state)
        return Value(res, State.BRANCH, env=branch, origin=env)

    def get_branch(self, name, env, action):
        h = env.get(name)
        if not isinstance(h, Value) or h.state != State.BRANCH:
            raise SPLRuntimeError(f"Cannot {action} '{name}': not a fork")
        if h.settled: raise SPLRuntimeError(f"Cannot {action} '{name}': fork already settled")
        h.settled = True
        return h

    def visit_Commit(self, n, env):
        h = self.get_branch(n.name, env, "commit")
        h.origin.update_from(h.env); return h.val

    def visit_Discard(self, n, env):
        return self.get_branch(n.name, env, "discard").val

    # --- Multiverse: run a block in many independent, reproducible universes ---

    def visit_Multiverse(self, n, env):
        count = self.int_of(self.visit(n.count, env), "multiverse count")
        base, saved_state = self.seed, self.rng.getstate()
        samples, rejected = [], 0
        self.universe_depth += 1
        try:
            for i in range(count):
                self.seed = universe_seed(base, i)
                self.rng.seed(self.seed)
                try: samples.append(self.sample(self.visit(n.block, env.clone())))
                except Rejected: rejected += 1
        finally:
            self.universe_depth -= 1
            self.seed = base
            self.rng.setstate(saved_state)
        return self.aggregate(samples, rejected)

    def sample(self, v):
        # Observe a universe's result fully: an int, or a struct of samples
        v = self.collapse(v)
        if v.state == State.STRUCT: return (v.type_hint, {k: self.sample(f) for k, f in v.val.items()})
        if not isinstance(v.val, int):
            raise SPLRuntimeError(f"A universe must produce an integer or a struct, got {self.format(v)}")
        return v.val

    def aggregate(self, samples, rejected):
        if not samples or all(isinstance(s, int) for s in samples): return summarise(samples, rejected)
        shape = samples[0]
        if not all(isinstance(s, tuple) and s[0] == shape[0] for s in samples):
            raise SPLRuntimeError("Every universe must produce the same kind of result")
        fields = {k: self.aggregate([s[1][k] for s in samples], rejected) for k in shape[1]}
        return Value(fields, State.STRUCT, type_hint=shape[0])

    def visit_Given(self, n, env):
        if not self.collapse(self.visit(n.cond, env)).val:
            if self.universe_depth == 0:
                raise SPLRuntimeError("'given' condition failed outside a multiverse")
            raise Rejected()
        return Value(None)

    # --- Values, operators and collapse ---

    def visit_Func(self, n, env):
        f = Closure(n.name, n.params, n.body, env)
        env.set(n.name, f); return Value(None)

    def visit_Num(self, n, env): return Value(n.val, type_hint="Int")
    def visit_Str(self, n, env): return Value(n.val, type_hint="String")

    def visit_Open(self, n, env):
        bounds = None if n.lo is None else (self.visit(n.lo, env), self.visit(n.hi, env))
        return Value(bounds, State.OPEN, type_hint="?Int")

    def visit_Var(self, n, env): return env.get(n.name)

    def lazy(self, op, operands):
        # If any operand is Open or a Future, the result is a Future too
        operands = [deref(o) for o in operands]
        if any(o.state in (State.OPEN, State.RESOLVED) for o in operands):
            return Value((op, operands), State.RESOLVED, type_hint="~Bool" if op in COMPARISONS else "~Int")
        return Value(apply_op(op, [o.val for o in operands]))

    def visit_BinOp(self, n, env):
        return self.lazy(n.op, [self.visit(n.left, env), self.visit(n.right, env)])

    def visit_Observe(self, n, env): return self.collapse(self.visit(n.expr, env))

    def collapse(self, v):
        v = deref(v)
        if v.state == State.OPEN:
            lo, hi = (0, 99) if v.val is None else (self.int_of(v.val[0], "open() bound"), self.int_of(v.val[1], "open() bound"))
            if lo > hi: raise SPLRuntimeError(f"open({lo}, {hi}): empty range")
            v.val = self.rng.randint(lo, hi)
            v.state = State.COLLAPSED
        elif v.state == State.RESOLVED:
            # Recursively collapse dependencies, then run the deferred operator
            op, operands = v.val
            v.val = apply_op(op, [self.collapse(o).val for o in operands])
            v.state = State.COLLAPSED
        return v

    def visit_Call(self, n, env):
        f = env.get(n.func)
        args = [self.visit(a, env) for a in n.args]
        if isinstance(f, NativeFunc): return f.func(args)
        if not isinstance(f, Closure): raise SPLRuntimeError(f"'{n.func}' is not a function")
        if len(args) != len(f.params):
            raise SPLRuntimeError(f"'{n.func}' expects {len(f.params)} argument(s), got {len(args)}")
        scope = Environment(f.env)
        for p, a in zip(f.params, args): scope.set(p, a)
        return self.visit(f.body, scope)

    # --- Control flow ---

    def visit_If(self, n, env):
        cond = self.collapse(self.visit(n.cond, env))
        if cond.val: return self.visit(n.then_b, Environment(env))
        if n.else_b: return self.visit(n.else_b, Environment(env))
        return Value(None)

    def visit_Repeat(self, n, env):
        res = Value(None)
        for _ in range(self.int_of(self.visit(n.count, env), "repeat count")):
            res = self.visit(n.block, Environment(env))
        return res

    def visit_While(self, n, env):
        res = Value(None)
        while self.collapse(self.visit(n.cond, env)).val:
            res = self.visit(n.block, Environment(env))
        return res

# ==========================================
# 4. RUNNERS
# ==========================================

def with_deep_stack(fn):
    # SPL recursion maps onto Python recursion, so give it room to breathe
    result = {}
    def target():
        try: result['value'] = fn()
        except BaseException as e: result['error'] = e
    sys.setrecursionlimit(max(sys.getrecursionlimit(), 200000))
    old = threading.stack_size(512 * 1024 * 1024)
    try:
        t = threading.Thread(target=target)
        t.start(); t.join()
    finally:
        threading.stack_size(old)
    if 'error' in result: raise result['error']
    return result.get('value')

def run_spl(code, seed=None):
    ast = Parser(lex(code)).parse_program()
    i = Interpreter(seed)
    with_deep_stack(lambda: i.run(ast))
    return i

def main(argv):
    import argparse
    ap = argparse.ArgumentParser(description="Run an SPL program.")
    ap.add_argument("filename")
    ap.add_argument("--seed", type=int, default=None,
                    help="initial RNG seed; omit for a random one (it is printed so the run can be replayed)")
    args = ap.parse_args(argv)

    with open(args.filename, 'r') as f:
        code = f.read()

    print(f"--- Executing {args.filename} ---")
    try:
        ast = Parser(lex(code)).parse_program()
        i = Interpreter(args.seed)
        if args.seed is None: print(f"[SYS] Initial seed: {i.seed} (replay with --seed {i.seed})")
        with_deep_stack(lambda: i.run(ast))
    except SPLError as e:
        sys.stdout.flush()
        print(f"Error: {e}", file=sys.stderr)
        return 1
    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
