import re
import random
import copy

# ==========================================
# 1. RUNTIME STATE & VALUES
# ==========================================

class State:
    OPEN, RESOLVED, COLLAPSED = "OPEN", "RESOLVED", "COLLAPSED"
    BRANCH, STRUCT = "BRANCH", "STRUCT"

class Value:
    def __init__(self, val, state=State.COLLAPSED, type_hint="Any", env=None):
        self.val = val
        self.state = state
        self.type_hint = type_hint
        self.env = env # For Branches (captures scope)
    
    def unbox(self):
        # Transparently allow Branches to behave like their return values
        if self.state == State.BRANCH: return self.val
        return self.val

    def __repr__(self):
        if self.state == State.OPEN: return f"<{self.type_hint} (Open)>"
        if self.state == State.RESOLVED: return f"<{self.type_hint} (Future)>"
        if self.state == State.BRANCH: return f"<Branch: {self.val}>"
        if self.state == State.STRUCT: return f"Struct<{self.type_hint}>"
        return f"<{self.val}>"

# Global Persistence for 'pin' logic
class GlobalMemo:
    def __init__(self): self.pinned_values = {}
    def get_pin(self, name): return self.pinned_values.get(name)
    def set_pin(self, name, val): self.pinned_values[name] = val
    def clear_pin(self, name): 
        if name in self.pinned_values: del self.pinned_values[name]

MEMO = GlobalMemo()

class Environment:
    def __init__(self, parent=None):
        self.vars = {}
        self.parent = parent

    def get(self, name):
        if name in self.vars: return self.vars[name]
        if self.parent: return self.parent.get(name)
        raise Exception(f"Undefined variable '{name}'")

    def set(self, name, val): self.vars[name] = val
    
    def clone(self):
        new_env = Environment(self.parent)
        new_env.vars = copy.deepcopy(self.vars) 
        return new_env
    
    def update_from(self, other_env):
        self.vars.update(other_env.vars)

# ==========================================
# 2. LEXER & PARSER
# ==========================================

TOKEN_TYPES = [
    # 1. Skip Comments (Hash followed by anything until newline)
    ('COMMENT', r'#[^\n]*'), 
    
    # 2. Keywords & Symbols
    ('FN', r'fn'), ('LET', r'let'), ('OBSERVE', r'observe'),
    ('IF', r'if'), ('ELSE', r'else'), ('OPEN', r'open'),
    ('FORK', r'fork'), ('COMMIT', r'commit'), ('DISCARD', r'discard'),
    ('PIN', r'pin'), ('RESET', r'reset'), ('TYPE_DEF', r'type'), 
    ('DOT', r'\.'), ('COLON', r':'), ('Q_MARK', r'\?'), ('TILDE', r'~'),
    ('ID', r'[a-zA-Z_][a-zA-Z0-9_]*'), ('NUMBER', r'\d+'),
    ('OP', r'[+\-*/><=]+'), ('LPAREN', r'\('), ('RPAREN', r'\)'),
    ('LBRACE', r'\{'), ('RBRACE', r'\}'), ('SEMI', r';'), ('COMMA', r','), 
    
    # 3. Whitespace
    ('WS', r'\s+')
]

def lex(code):
    tokens = []
    pos = 0
    while pos < len(code):
        match = None
        for name, pattern in TOKEN_TYPES:
            regex = re.compile(pattern)
            m = regex.match(code, pos)
            if m:
                match = (name, m.group(0))
                pos = m.end()
                break
        if not match: 
            # Show a helpful error snippet
            snippet = code[pos:pos+10].replace('\n', '\\n')
            raise Exception(f"Illegal char at {pos}: '{snippet}...'")
            
        # IGNORE both Whitespace AND Comments
        if match[0] != 'WS' and match[0] != 'COMMENT': 
            tokens.append(match)
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

class Parser:
    def __init__(self, tokens):
        self.tokens = tokens
        self.pos = 0

    # FIX 1: Safe peek that doesn't crash on EOF
    def peek(self):
        if self.pos >= len(self.tokens):
            return "EOF" # Return a special End-Of-File marker
        return self.tokens[self.pos][0]

    # FIX 2: consume now handles EOF gracefully
    def consume(self, type_name):
        if self.pos >= len(self.tokens):
            raise Exception(f"Unexpected End of File. Expected '{type_name}'")
            
        if self.tokens[self.pos][0] == type_name:
            self.pos += 1
            return self.tokens[self.pos-1][1]
        raise Exception(f"Expected {type_name}, got {self.tokens[self.pos]}")

    def parse_type(self):
        prefix = ""
        if self.peek() in ['Q_MARK', 'TILDE']: prefix = self.consume(self.peek())
        return prefix + self.consume('ID')

    def parse_block(self):
        self.consume('LBRACE')
        stmts = []
        while self.peek() != 'RBRACE': stmts.append(self.parse_stmt())
        self.consume('RBRACE')
        return Block(stmts)

    def parse_stmt(self):
        t = self.peek()
        if t == 'LET':
            self.consume('LET'); name = self.consume('ID')
            ann = "Any"
            if self.peek() == 'COLON': self.consume('COLON'); ann = self.parse_type()
            self.consume('OP'); expr = self.parse_expr(); self.consume('SEMI')
            return Let(name, expr, ann)
        elif t == 'PIN':
            self.consume('PIN'); name = self.consume('ID')
            ann = "Any"
            if self.peek() == 'COLON': self.consume('COLON'); ann = self.parse_type()
            self.consume('OP'); expr = self.parse_expr(); self.consume('SEMI')
            return Pin(name, expr, ann)
        elif t == 'RESET':
            self.consume('RESET'); name = self.consume('ID'); self.consume('SEMI')
            return Reset(name)
        elif t == 'COMMIT':
            self.consume('COMMIT'); name = self.consume('ID'); self.consume('SEMI')
            return Commit(name)
        elif t == 'DISCARD':
            self.consume('DISCARD'); name = self.consume('ID'); self.consume('SEMI')
            return Discard(name)
        elif t == 'TYPE_DEF':
            self.consume('TYPE_DEF'); name = self.consume('ID'); self.consume('OP')
            self.consume('LBRACE'); fields = []
            while self.peek() != 'RBRACE':
                fields.append(self.consume('ID'))
                if self.peek() == 'COMMA': self.consume('COMMA')
            self.consume('RBRACE'); self.consume('SEMI')
            return StructDef(name, fields)
        elif t == 'FN':
            self.consume('FN'); name = self.consume('ID'); self.consume('LPAREN')
            params = []
            if self.peek() != 'RPAREN':
                params.append(self.consume('ID'))
                while self.peek() == 'COMMA': self.consume('COMMA'); params.append(self.consume('ID'))
            self.consume('RPAREN'); self.consume('OP'); body = self.parse_block()
            return Func(name, params, body)
        else:
            expr = self.parse_expr()
            if self.pos < len(self.tokens) and self.peek() == 'SEMI': self.consume('SEMI')
            return expr

    def parse_expr(self): return self.parse_term()
    def parse_term(self):
        left = self.parse_factor()
        while self.pos < len(self.tokens) and self.peek() == 'DOT':
            self.consume('DOT'); member = self.consume('ID')
            left = MemberAccess(left, member)
        while self.pos < len(self.tokens) and self.tokens[self.pos][1] in ['+', '-', '>', '<', '==']:
            op = self.consume('OP'); right = self.parse_factor()
            left = BinOp(left, op, right)
        return left
    def parse_factor(self):
        t = self.peek()
        if t == 'NUMBER': return Num(int(self.consume('NUMBER')))
        if t == 'OPEN': self.consume('OPEN'); return Open()
        if t == 'FORK': self.consume('FORK'); return Fork(self.parse_block())
        if t == 'OBSERVE': self.consume('OBSERVE'); return Observe(self.parse_expr())
        if t == 'IF':
            self.consume('IF'); cond = self.parse_expr(); then_b = self.parse_block()
            else_b = None
            if self.pos < len(self.tokens) and self.peek() == 'ELSE':
                self.consume('ELSE'); else_b = self.parse_block()
            return If(cond, then_b, else_b)
        if t == 'ID':
            name = self.consume('ID')
            if self.pos+1 < len(self.tokens) and self.tokens[self.pos][0] == 'LBRACE':
                self.consume('LBRACE'); fields = {}
                while self.peek() != 'RBRACE':
                    k = self.consume('ID'); self.consume('COLON'); v = self.parse_expr()
                    fields[k] = v
                    if self.peek() == 'COMMA': self.consume('COMMA')
                self.consume('RBRACE')
                return StructInit(name, fields)
            if self.peek() == 'LPAREN':
                self.consume('LPAREN'); args = []
                if self.peek() != 'RPAREN':
                    args.append(self.parse_expr())
                    while self.peek() == 'COMMA': self.consume('COMMA'); args.append(self.parse_expr())
                self.consume('RPAREN')
                return Call(name, args)
            return Var(name)
        if t == 'LPAREN': self.consume('LPAREN'); expr = self.parse_expr(); self.consume('RPAREN'); return expr
        raise Exception(f"Unexpected token {t}")

# ==========================================
# 3. INTERPRETER ENGINE
# ==========================================

class NativeFunc:
    def __init__(self, func): self.func = func

class Interpreter:
    def __init__(self):
        self.env = Environment()
        self.load_stdlib()
    
    def load_stdlib(self):
        def n_print(args): print(*[a.val for a in args]); return Value(None)
        def n_seed(args): random.seed(args[0].val); print(f"[SYS] Seed: {args[0].val}"); return Value(None)
        self.env.set('print', NativeFunc(n_print))
        self.env.set('seed', NativeFunc(n_seed))

    def visit(self, node, env): return getattr(self, f'visit_{type(node).__name__}')(node, env)
    def visit_Block(self, n, env):
        res = None
        for s in n.stmts: res = self.visit(s, env)
        return res
    
    def validate(self, name, val, ann):
        if ann == "Any": return
        if ann.startswith("?") and val.state != State.OPEN:
            print(f"[WARN] {name}: Expected Open ({ann}), got {val.state}")
        elif ann.startswith("~") and val.state != State.RESOLVED:
            print(f"[WARN] {name}: Expected Future ({ann}), got {val.state}")

    def visit_Let(self, n, env):
        v = self.visit(n.expr, env)
        self.validate(n.name, v, n.type_ann)
        env.set(n.name, v); return v
    
    def visit_Pin(self, n, env):
        saved = MEMO.get_pin(n.name)
        if saved: 
            print(f"[SYS] Pinned '{n.name}' retrieved.")
            env.set(n.name, saved); return saved
        v = self.collapse(self.visit(n.expr, env))
        MEMO.set_pin(n.name, v); env.set(n.name, v); return v
    
    def visit_Reset(self, n, env): MEMO.clear_pin(n.name); return Value(None)
    def visit_StructDef(self, n, env): return Value(None)
    def visit_StructInit(self, n, env):
        fields = {k: self.visit(v, env) for k, v in n.fields.items()}
        return Value(fields, State.STRUCT, type_hint=n.name)
    
    def visit_MemberAccess(self, n, env):
        obj = self.visit(n.obj, env)
        # Transparently handle Branches returning Structs
        target = obj.val if obj.state == State.BRANCH else obj
        if target.state == State.STRUCT and n.member in target.val: return target.val[n.member]
        raise Exception(f"Cannot access {n.member} on {obj}")

    def visit_Fork(self, n, env):
        branch = env.clone()
        res = self.visit(n.block, branch)
        return Value(res, State.BRANCH, env=branch)
    
    def visit_Commit(self, n, env):
        h = env.get(n.name)
        env.update_from(h.env); return Value(h.val)
    
    def visit_Discard(self, n, env):
        h = env.get(n.name); return Value(h.val)

    def visit_Func(self, n, env): env.set(n.name, n); return n
    def visit_Num(self, n, env): return Value(n.val)
    def visit_Open(self, n, env): return Value(None, State.OPEN)
    def visit_Var(self, n, env): return env.get(n.name)
    
    def visit_BinOp(self, n, env):
        l, r = self.visit(n.left, env), self.visit(n.right, env)
        # Propagation Logic
        if l.state == State.OPEN or r.state == State.OPEN:
            return Value((l, n.op, r), State.RESOLVED, type_hint="~Int")
        val = 0
        lv, rv = l.unbox(), r.unbox()
        if n.op == '+': val = lv + rv
        elif n.op == '-': val = lv - rv
        elif n.op == '>': val = 1 if lv > rv else 0
        elif n.op == '<': val = 1 if lv < rv else 0
        elif n.op == '==': val = 1 if lv == rv else 0
        return Value(val)

    def visit_Observe(self, n, env): return self.collapse(self.visit(n.expr, env))
    def collapse(self, v):
        if v.state == State.OPEN: v.val = random.randint(0, 99); v.state = State.COLLAPSED; return v
        if v.state == State.RESOLVED:
            l = self.collapse(v.val[0]); r = self.collapse(v.val[2])
            # Re-run op
            op = v.val[1]
            if op == '+': v.val = l.val + r.val
            v.state = State.COLLAPSED; return v
        return v
    
    def visit_Call(self, n, env):
        f = env.get(n.func)
        args = [self.visit(a, env) for a in n.args]
        if isinstance(f, NativeFunc): return f.func(args)
        scope = Environment(env)
        for p, a in zip(f.params, args): scope.set(p, a)
        return self.visit(f.body, scope)
    
    def visit_If(self, n, env):
        cond = self.collapse(self.visit(n.cond, env))
        if cond.unbox(): return self.visit(n.then_b, env)
        elif n.else_b: return self.visit(n.else_b, env)
        return Value(None)

# Utility Runner
def run_spl(code):
    l = lex(code); p = Parser(l)
    ast = []
    while p.pos < len(p.tokens): ast.append(p.parse_stmt())
    i = Interpreter()
    for n in ast: 
        if isinstance(n, Func): i.visit(n, i.env)
    if 'main' in i.env.vars: i.visit(Call('main', []), i.env)

if __name__ == "__main__":
    import sys
    if len(sys.argv) < 2:
        print("Usage: python interpreter.py <filename.spl>")
        sys.exit(1)
        
    filename = sys.argv[1]
    with open(filename, 'r') as f:
        code = f.read()
        
    print(f"--- Executing {filename} ---")
    run_spl(code)
