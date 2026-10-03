import re
import sys
import random
import copy

# ==========================================
# 0. ERRORS
# ==========================================

class SPLError(Exception): pass
class SPLSyntaxError(SPLError): pass
class SPLRuntimeError(SPLError): pass

# ==========================================
# 1. RUNTIME STATE & VALUES
# ==========================================

class State:
    OPEN, RESOLVED, COLLAPSED = "OPEN", "RESOLVED", "COLLAPSED"
    BRANCH, STRUCT = "BRANCH", "STRUCT"

class Value:
    def __init__(self, val, state=State.COLLAPSED, type_hint="Any", env=None, origin=None):
        self.val = val
        self.state = state
        self.type_hint = type_hint
        self.env = env        # For Branches: the forked timeline's scope
        self.origin = origin  # For Branches: the scope the fork was created in
        self.settled = False  # For Branches: set once committed or discarded

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
# interpreter, so pinned values survive re-seeding of the RNG.
class GlobalMemo:
    def __init__(self): self.pinned_values = {}
    def get_pin(self, name): return self.pinned_values.get(name)
    def set_pin(self, name, val): self.pinned_values[name] = val
    def clear_pin(self, name): self.pinned_values.pop(name, None)

class Environment:
    def __init__(self, parent=None):
        self.vars = {}
        self.parent = parent

    def get(self, name):
        if name in self.vars: return self.vars[name]
        if self.parent: return self.parent.get(name)
        raise SPLRuntimeError(f"Undefined variable '{name}'")

    def set(self, name, val): self.vars[name] = val

    def clone(self, memo=None):
        # Copy the whole scope chain so a fork cannot reach back and
        # collapse values that belong to its parent timeline.
        memo = {} if memo is None else memo
        new_env = Environment(self.parent.clone(memo) if self.parent else None)
        new_env.vars = copy.deepcopy(self.vars, memo)
        return new_env

    def update_from(self, other_env):
        self.vars.update(other_env.vars)
        if self.parent and other_env.parent: self.parent.update_from(other_env.parent)

# ==========================================
# 2. LEXER & PARSER
# ==========================================

KEYWORDS = {
    'fn': 'FN', 'let': 'LET', 'pin': 'PIN', 'reset': 'RESET', 'type': 'TYPE_DEF',
    'commit': 'COMMIT', 'discard': 'DISCARD', 'fork': 'FORK', 'observe': 'OBSERVE',
    'open': 'OPEN', 'if': 'IF', 'else': 'ELSE',
}

TOKEN_TYPES = [
    # 1. Skipped: comments (hash until newline) and whitespace
    ('COMMENT', r'#[^\n]*'), ('WS', r'\s+'),

    # 2. Identifiers (keywords are split out after matching) & literals
    ('ID', r'[a-zA-Z_][a-zA-Z0-9_]*'), ('NUMBER', r'\d+'),

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
class Var(AST):
    def __init__(self, name): self.name = name
class Let(AST):
    def __init__(self, name, expr, type_ann="Any"): self.name, self.expr, self.type_ann = name, expr, type_ann
class Pin(AST):
    def __init__(self, name, expr, type_ann="Any"): self.name, self.expr, self.type_ann = name, expr, type_ann
class Reset(AST):
    def __init__(self, name): self.name = name
class Func(AST):
    def __init__(self, name, params, body): self.name, self.params, self.body = name, params, body
class Call(AST):
    def __init__(self, func, args): self.func, self.args = func, args
class Observe(AST):
    def __init__(self, expr): self.expr = expr
class Open(AST): pass
class Block(AST):
    def __init__(self, stmts): self.stmts = stmts
class If(AST):
    def __init__(self, cond, then_b, else_b): self.cond, self.then_b, self.else_b = cond, then_b, else_b
class Fork(AST):
    def __init__(self, block): self.block = block
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
EXPR_NODES = (BinOp, Num, Var, Call, Observe, Open, If, Fork, StructInit, MemberAccess)
BLOCK_EXPRS = (If, Fork)

# Binary operator precedence, loosest first (see `expr` in the EBNF)
PRECEDENCE = [['||'], ['&&'], ['==', '>', '<'], ['+', '-'], ['*', '/']]

class Parser:
    def __init__(self, tokens):
        self.tokens = tokens
        self.pos = 0
        self.no_struct = False # True while parsing an `if` condition

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

    def parse_block(self):
        self.consume('LBRACE')
        saved, self.no_struct = self.no_struct, False
        stmts = []
        while self.peek() != 'RBRACE': stmts.append(self.parse_stmt())
        self.consume('RBRACE')
        self.no_struct = saved
        return Block(stmts)

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

        # expr_stmt: `if`/`fork` need no ';', nor does a block's trailing expr
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

    def parse_primary(self):
        t = self.peek()
        if t == 'NUMBER': return Num(int(self.consume('NUMBER')))
        if t == 'OPEN': self.consume('OPEN'); return Open()
        if t == 'FORK': self.consume('FORK'); return Fork(self.parse_block())
        if t == 'OBSERVE': self.consume('OBSERVE'); return Observe(self.parse_expr())
        if t == 'IF':
            self.consume('IF')
            saved, self.no_struct = self.no_struct, True
            cond = self.parse_expr()
            self.no_struct = saved
            then_b = self.parse_block()
            else_b = None
            if self.peek() == 'ELSE': self.consume('ELSE'); else_b = self.parse_block()
            return If(cond, then_b, else_b)
        if t == 'ID':
            name = self.consume('ID')
            if self.peek() == 'LBRACE' and not self.no_struct:
                self.consume('LBRACE')
                saved, self.no_struct = self.no_struct, False
                fields = {}
                while self.peek() != 'RBRACE':
                    k = self.consume('ID'); self.consume('COLON'); fields[k] = self.parse_expr()
                    if self.peek() != 'RBRACE': self.consume('COMMA')
                self.consume('RBRACE')
                self.no_struct = saved
                return StructInit(name, fields)
            if self.peek() == 'LPAREN':
                self.consume('LPAREN'); args = []
                saved, self.no_struct = self.no_struct, False
                if self.peek() != 'RPAREN':
                    args.append(self.parse_expr())
                    while self.peek() == 'COMMA': self.consume('COMMA'); args.append(self.parse_expr())
                self.consume('RPAREN')
                self.no_struct = saved
                return Call(name, args)
            return Var(name)
        if t == 'LPAREN':
            self.consume('LPAREN')
            saved, self.no_struct = self.no_struct, False
            expr = self.parse_expr()
            self.consume('RPAREN')
            self.no_struct = saved
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
    def __deepcopy__(self, memo): return self

def apply_op(op, lv, rv):
    if not isinstance(lv, int) or not isinstance(rv, int):
        raise SPLRuntimeError(f"Operator '{op}' needs integers, got {lv!r} and {rv!r}")
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

class Interpreter:
    def __init__(self, seed=None):
        # Each interpreter owns its RNG, so runs are reproducible for a
        # given seed regardless of anything else using Python's `random`.
        self.rng = random.Random()
        self.memo = GlobalMemo()
        self.types = {}
        self.env = Environment()
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
            if len(args) != 1: raise SPLRuntimeError("seed() takes exactly one argument")
            s = deref(self.collapse(args[0])).val
            if not isinstance(s, int): raise SPLRuntimeError(f"seed() needs an integer, got {s!r}")
            self.reseed(s); print(f"[SYS] Seed: {s}"); return Value(None)
        self.env.set('print', NativeFunc(n_print))
        self.env.set('seed', NativeFunc(n_seed))

    def format(self, v):
        v = deref(v)
        if v.state == State.STRUCT:
            inner = ", ".join(f"{k}: {self.format(self.collapse(f))}" for k, f in v.val.items())
            return f"{v.type_hint} {{ {inner} }}"
        return "none" if v.val is None else str(v.val)

    def run(self, ast):
        for n in ast: self.visit(n, self.env)
        main = self.env.vars.get('main')
        if isinstance(main, Closure): self.visit(Call('main', []), self.env)

    def visit(self, node, env): return getattr(self, f'visit_{type(node).__name__}')(node, env)

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
        branch = env.clone()
        res = self.visit(n.block, branch)
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

    def visit_Func(self, n, env):
        f = Closure(n.name, n.params, n.body, env)
        env.set(n.name, f); return Value(None)

    def visit_Num(self, n, env): return Value(n.val, type_hint="Int")
    def visit_Open(self, n, env): return Value(None, State.OPEN, type_hint="?Int")
    def visit_Var(self, n, env): return env.get(n.name)

    def visit_BinOp(self, n, env):
        l, r = deref(self.visit(n.left, env)), deref(self.visit(n.right, env))
        # If either side is Open or a Future, the result is a Future too
        if l.state in (State.OPEN, State.RESOLVED) or r.state in (State.OPEN, State.RESOLVED):
            hint = "~Int" if n.op in '+-*/' else "~Bool"
            return Value((l, n.op, r), State.RESOLVED, type_hint=hint)
        return Value(apply_op(n.op, l.val, r.val))

    def visit_Observe(self, n, env): return self.collapse(self.visit(n.expr, env))

    def collapse(self, v):
        v = deref(v)
        if v.state == State.OPEN:
            v.val = self.rng.randint(0, 99)
            v.state = State.COLLAPSED
        elif v.state == State.RESOLVED:
            # Recursively collapse dependencies, then run the deferred operator
            l, op, r = v.val
            v.val = apply_op(op, self.collapse(l).val, self.collapse(r).val)
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

    def visit_If(self, n, env):
        cond = self.collapse(self.visit(n.cond, env))
        if cond.val: return self.visit(n.then_b, Environment(env))
        if n.else_b: return self.visit(n.else_b, Environment(env))
        return Value(None)

# Utility Runner
def run_spl(code, seed=None):
    ast = Parser(lex(code)).parse_program()
    i = Interpreter(seed)
    i.run(ast)
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
        i.run(ast)
    except SPLError as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1
    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
