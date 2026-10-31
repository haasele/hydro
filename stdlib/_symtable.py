# Hydro-native emulation of CPython's ``_symtable`` module.
#
# CPython builds symbol tables from its parser's AST.  Hydro cannot use the
# CPython parser at runtime (``ast.parse``/``compile`` are unavailable in the
# native pipeline), so this module is a bounded, pure-Python source scanner.
# It recognises the declarations that the public ``symtable`` API needs:
# module/function/class blocks, parameters, assignments, annotated
# assignments, imports, ``global``/``nonlocal``, loop/comprehension targets,
# ``with``/``except`` targets and name references.  The flag values, scope
# encoding and raw table attributes match CPython 3.14.9 so the vendored
# ``symtable.py`` wrapper can consume them unchanged.
#
# Limitations: the scanner is line/bracket based rather than a real parser.
# It does not evaluate type-parameter scopes, does not create the PEP 649
# ``__annotate__``/type-alias/type-variable blocks, and treats generator
# expressions as part of the enclosing block.  Dotted targets and pattern
# matching are handled best-effort.  Line numbers are the physical source
# lines of the declarations that are actually seen.

USE = 16
DEF_GLOBAL = 1
DEF_LOCAL = 2
DEF_PARAM = 4
DEF_NONLOCAL = 8
DEF_FREE_CLASS = 64
DEF_IMPORT = 128
DEF_ANNOT = 256
DEF_COMP_ITER = 512
DEF_TYPE_PARAM = 1024
DEF_COMP_CELL = 2048
DEF_BOUND = DEF_LOCAL | DEF_PARAM | DEF_IMPORT

SCOPE_OFF = 12
SCOPE_MASK = DEF_GLOBAL | DEF_LOCAL | DEF_PARAM | DEF_NONLOCAL

LOCAL = 1
GLOBAL_EXPLICIT = 2
GLOBAL_IMPLICIT = 3
FREE = 4
CELL = 5

TYPE_FUNCTION = 0
TYPE_CLASS = 1
TYPE_MODULE = 2
TYPE_ANNOTATION = 3
TYPE_TYPE_ALIAS = 4
TYPE_TYPE_PARAMETERS = 5
TYPE_TYPE_VARIABLE = 6

_next_id = [1]

_KEYWORDS = {
    "False", "None", "True", "and", "as", "assert", "async", "await",
    "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try",
    "while", "with", "yield",
}

_AUG_OPS = ("**=", "//=", "<<=", ">>=", "+=", "-=", "*=", "/=", "%=",
            "&=", "|=", "^=", "@=")


class _Table:
    """Raw symbol-table block consumed by the public ``symtable.py``."""

    def __init__(self, kind, name, lineno, parent):
        self.type = kind
        self.name = name
        self.lineno = lineno
        self.id = _next_id[0]
        _next_id[0] = _next_id[0] + 1
        self.children = []
        self.symbols = {}
        self.varnames = []
        self.nested = 0
        self.parent = parent
        if parent is not None:
            parent.children.append(self)
            if parent.type == TYPE_FUNCTION or parent.nested:
                self.nested = 1

    def __repr__(self):
        return "<symtable entry %s(%s), line %s>" % (
            self.name, self.id, self.lineno)


def _new_table(kind, name, lineno, parent):
    return _Table(kind, name, lineno, parent)


# ---------------------------------------------------------------------------
# Source masking: remove comments and string literals while preserving line
# structure, so the scanner never mistakes their contents for Python code.
# ---------------------------------------------------------------------------

def _mask_source(source):
    out = []
    i = 0
    n = len(source)
    quote = ""
    triple = False
    prefixes = "rRbBuUfF"
    while i < n:
        ch = source[i]
        if quote:
            if ch == "\\":
                out.append(" ")
                if i + 1 < n:
                    if source[i + 1] == "\n":
                        out.append("\n")
                    else:
                        out.append(" ")
                    i += 2
                else:
                    i += 1
                continue
            if triple:
                if source[i:i + len(quote) * 3] == quote * 3:
                    out.append("   ")
                    i += 3
                    quote = ""
                    triple = False
                    continue
                if ch == "\n":
                    out.append("\n")
                else:
                    out.append(" ")
                i += 1
                continue
            if ch == quote:
                out.append(" ")
                quote = ""
                i += 1
                continue
            if ch == "\n":
                # Unterminated single-line string: recover at the newline.
                out.append("\n")
                quote = ""
                i += 1
                continue
            out.append(" ")
            i += 1
            continue

        if ch == "#":
            while i < n and source[i] != "\n":
                out.append(" ")
                i += 1
            continue
        if ch == "'" or ch == '"':
            # Blank a string prefix such as b, r, f, u, br or fr immediately
            # before the quote; otherwise it would be scanned as a name.
            while out and out[-1] in prefixes:
                out.pop()
            if source[i:i + 3] == ch * 3:
                out.append("   ")
                i += 3
                quote = ch
                triple = True
            else:
                out.append(" ")
                i += 1
                quote = ch
                triple = False
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def _indent_width(line):
    width = 0
    for ch in line:
        if ch == " ":
            width += 1
        elif ch == "\t":
            width += 8 - (width % 8)
        else:
            break
    return width


def _has_line_continuation(text):
    stripped = text.rstrip("\r")
    if not stripped.endswith("\\"):
        return False
    # An odd number of trailing backslashes continues the line.
    count = 0
    j = len(stripped) - 1
    while j >= 0 and stripped[j] == "\\":
        count += 1
        j -= 1
    return count % 2 == 1


def _update_depth(text, depth):
    for ch in text:
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
        elif ch == ")" or ch == "]" or ch == "}":
            depth -= 1
            if depth < 0:
                depth = 0
    return depth


def _logical_lines(masked):
    raw = masked.split("\n")
    result = []
    buf = []
    start = 0
    indent = 0
    depth = 0
    for index in range(len(raw)):
        line = raw[index]
        if not buf:
            if not line.strip():
                continue
            start = index + 1
            indent = _indent_width(line)
        buf.append(line)
        depth = _update_depth(line, depth)
        if depth > 0 or _has_line_continuation(line):
            continue
        text = " ".join(part.rstrip() for part in buf)
        text = text.strip()
        if text:
            result.append((start, indent, text))
        buf = []
        depth = 0
    if buf:
        text = " ".join(part.rstrip() for part in buf).strip()
        if text:
            result.append((start, indent, text))
    return result


def _split_statements(text):
    parts = []
    start = 0
    depth = 0
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
        elif ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
        elif ch == ";" and depth == 0:
            parts.append(text[start:i])
            start = i + 1
        i += 1
    parts.append(text[start:])
    result = []
    for part in parts:
        part = part.strip()
        if part:
            result.append(part)
    return result


def _split_top_level(text, separator):
    parts = []
    start = 0
    depth = 0
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
        elif ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
        elif ch == separator and depth == 0:
            parts.append(text[start:i])
            start = i + 1
        i += 1
    parts.append(text[start:])
    return parts


def _matching(text, start, opener, closer):
    depth = 0
    i = start
    while i < len(text):
        ch = text[i]
        if ch == opener:
            depth += 1
        elif ch == closer:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def _read_identifier(text, start):
    i = start
    n = len(text)
    while i < n and (text[i] == " " or text[i] == "\t"):
        i += 1
    if i >= n:
        return "", i
    ch = text[i]
    if not (ch.isalpha() or ch == "_"):
        return "", i
    j = i + 1
    while j < n and (text[j].isalnum() or text[j] == "_"):
        j += 1
    return text[i:j], j


def _starts_word(text, index, word):
    if text[index:index + len(word)] != word:
        return False
    before = text[index - 1] if index > 0 else ""
    after_index = index + len(word)
    after = text[after_index] if after_index < len(text) else ""
    if before and (before.isalnum() or before == "_"):
        return False
    if after and (after.isalnum() or after == "_"):
        return False
    return True


def _find_word(text, word, start):
    i = start
    while i < len(text):
        if (text[i].isalpha() or text[i] == "_") and _starts_word(text, i, word):
            return i
        i += 1
    return -1


def _find_top_level_colon(text, start=0):
    depth = 0
    i = start
    while i < len(text):
        ch = text[i]
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
        elif ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
        elif ch == ":" and depth == 0:
            if i + 1 < len(text) and text[i + 1] == "=":
                i += 1
            else:
                return i
        i += 1
    return -1


def _find_equals(text):
    positions = []
    depth = 0
    i = 0
    while i < len(text):
        ch = text[i]
        if depth == 0 and _starts_word(text, i, "lambda"):
            break
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
        elif ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
        elif ch == "=" and depth == 0:
            before = text[i - 1] if i > 0 else ""
            after = text[i + 1] if i + 1 < len(text) else ""
            if before not in "<>=!:" and after != "=":
                positions.append(i)
        i += 1
    return positions


def _find_augmented(text):
    depth = 0
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
        elif ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
        elif depth == 0:
            for op in _AUG_OPS:
                if text[i:i + len(op)] == op:
                    return i, op
        i += 1
    return -1, ""


# ---------------------------------------------------------------------------
# Symbol flag helpers
# ---------------------------------------------------------------------------

def _add(table, name, flags, scope):
    old = table.symbols.get(name, 0)
    effective = scope
    if flags & DEF_NONLOCAL:
        effective = FREE
    elif flags & DEF_GLOBAL:
        effective = GLOBAL_EXPLICIT
    elif old & DEF_NONLOCAL:
        effective = FREE
    elif old & DEF_GLOBAL:
        effective = GLOBAL_EXPLICIT
    if effective is not None:
        old = old & ~(SCOPE_MASK << SCOPE_OFF)
        old = old | ((effective & SCOPE_MASK) << SCOPE_OFF)
    old = old | flags
    table.symbols[name] = old


def _mark_use(table, name):
    table.symbols[name] = table.symbols.get(name, 0) | USE


def _bind(table, name, flags, scope):
    _add(table, name, flags, scope)


def _bind_name(table, name, flags):
    _add(table, name, flags, LOCAL)


def _scan_uses(table, text):
    if not text:
        return
    depth = 0
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
            i += 1
            continue
        if ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
            i += 1
            continue
        if ch.isalpha() or ch == "_":
            j = i + 1
            while j < n and (text[j].isalnum() or text[j] == "_"):
                j += 1
            word = text[i:j]
            k = i - 1
            while k >= 0 and (text[k] == " " or text[k] == "\t" or text[k] == "\r" or text[k] == "\n"):
                k -= 1
            before = text[k] if k >= 0 else ""
            m = j
            while m < n and (text[m] == " " or text[m] == "\t" or text[m] == "\r" or text[m] == "\n"):
                m += 1
            after = text[m] if m < n else ""
            skip = False
            if word == "for":
                # Inlined comprehension targets bind in the current block
                # (CPython 3.14 inlines list/set/dict comprehensions).
                name, end = _read_identifier(text, j)
                if name:
                    _bind_name(table, name, DEF_LOCAL | DEF_COMP_ITER)
                skip = True
            elif word in _KEYWORDS:
                skip = True
            elif before == ".":
                skip = True
            elif after == "=" and (m + 1 >= n or text[m + 1] != "="):
                # Keyword argument or default, not a name reference.
                skip = True
            if not skip:
                _mark_use(table, word)
            i = j
            continue
        i += 1


def _bind_target(text, table, extra):
    text = text.strip()
    if not text:
        return
    while text.startswith("*"):
        text = text[1:].lstrip()
    if not text:
        return
    # Unwrap one matching pair of grouping brackets.
    if (text.startswith("(") and text.endswith(")")) or (text.startswith("[") and text.endswith("]")):
        text = text[1:-1].strip()
    parts = _split_top_level(text, ",")
    if len(parts) > 1:
        for part in parts:
            _bind_target(part, table, extra)
        return
    name, end = _read_identifier(text, 0)
    if name and end == len(text):
        if extra:
            _bind_name(table, name, DEF_LOCAL | extra)
        else:
            _bind_name(table, name, DEF_LOCAL)
        return
    # Attribute/subscript target: the whole target is a reference (the
    # attribute name itself is skipped by _scan_uses because it follows a
    # dot); the indices of a subscript are references too.
    _scan_uses(table, text)


def _bind_annotated_target(text, table):
    colon = _find_top_level_colon(text)
    if colon >= 0:
        target = text[:colon]
        _bind_target(target, table, DEF_ANNOT)
    else:
        _bind_target(text, table, 0)


def _mark_walrus(text, table):
    i = 0
    while i + 1 < len(text):
        if text[i] == ":" and text[i + 1] == "=":
            j = i - 1
            while j >= 0 and (text[j] == " " or text[j] == "\t"):
                j -= 1
            end = j + 1
            while j >= 0 and (text[j].isalnum() or text[j] == "_"):
                j -= 1
            name = text[j + 1:end]
            if name and (name[0].isalpha() or name[0] == "_") and name not in _KEYWORDS:
                _bind_name(table, name, DEF_LOCAL)
            i += 2
        else:
            i += 1


# ---------------------------------------------------------------------------
# Statement handlers
# ---------------------------------------------------------------------------

def _handle_one_genexpr(text, lineno, table, tables):
    search = 0
    while True:
        pos = _find_word(text, "for", search)
        if pos < 0:
            return None
        stack = []
        i = 0
        while i < pos:
            ch = text[i]
            if ch == "(":
                stack.append(i)
            elif ch == ")" and stack:
                stack.pop()
            i += 1
        if not stack:
            search = pos + 3
            continue
        open_index = stack[-1]
        depth = 1
        j = open_index + 1
        while j < pos:
            ch = text[j]
            if ch == "(" or ch == "[" or ch == "{":
                depth += 1
            elif ch == ")" or ch == "]" or ch == "}":
                depth -= 1
            j += 1
        if depth != 1:
            search = pos + 3
            continue
        close_index = _matching(text, open_index, "(", ")")
        if close_index < 0:
            return None
        inside = text[open_index + 1:close_index]
        relative = pos - open_index - 1
        in_index = _find_word(inside, "in", relative + 3)
        if in_index < 0:
            search = pos + 3
            continue
        element = inside[:relative]
        target = inside[relative + 3:in_index]
        iterable = inside[in_index + 2:]
        child = _new_table(TYPE_FUNCTION, "genexpr", lineno, table)
        tables.append(child)
        _bind_name(child, ".0", DEF_PARAM)
        child.varnames.append(".0")
        _bind_target(target, child, DEF_COMP_ITER)
        _scan_uses(child, target)
        element = _handle_lambdas(element, lineno, child, tables)
        element = _handle_genexprs(element, lineno, child, tables)
        _scan_uses(child, element)
        iterable = _handle_lambdas(iterable, lineno, table, tables)
        iterable = _handle_genexprs(iterable, lineno, table, tables)
        _scan_uses(table, iterable)
        return text[:open_index] + " " + text[close_index + 1:]


def _handle_genexprs(text, lineno, table, tables):
    while True:
        result = _handle_one_genexpr(text, lineno, table, tables)
        if result is None:
            return text
        text = result


def _prepare_expr(text, lineno, table, tables):
    _mark_walrus(text, table)
    text = _handle_lambdas(text, lineno, table, tables)
    return _handle_genexprs(text, lineno, table, tables)


def _handle_assignment(text, table, lineno, tables):
    pos, op = _find_augmented(text)
    if pos >= 0:
        left = text[:pos]
        right = text[pos + len(op):]
        _bind_target(left, table, 0)
        _scan_uses(table, left)
        right = _prepare_expr(right, lineno, table, tables)
        _scan_uses(table, right)
        return True

    _mark_walrus(text, table)
    positions = _find_equals(text)
    if positions:
        start = 0
        targets = []
        for position in positions:
            targets.append(text[start:position])
            start = position + 1
        right = text[start:]
        for target in targets:
            _bind_annotated_target(target, table)
        right = _prepare_expr(right, lineno, table, tables)
        _scan_uses(table, right)
        return True

    colon = _find_top_level_colon(text)
    if colon >= 0:
        _bind_annotated_target(text, table)
        return True
    return False


def _handle_lambdas(text, lineno, table, tables):
    """Create a function table for a lambda and return the text before it."""
    pos = _find_word(text, "lambda", 0)
    if pos < 0:
        return text
    rest = text[pos + len("lambda"):]
    colon = _find_top_level_colon(rest)
    if colon < 0:
        return text[:pos]
    params = rest[:colon]
    body = rest[colon + 1:]
    child = _new_table(TYPE_FUNCTION, "lambda", lineno, table)
    tables.append(child)
    _parse_params(params, child, table)
    body = _handle_lambdas(body, lineno, child, tables)
    body = _handle_genexprs(body, lineno, child, tables)
    _scan_uses(child, body)
    # The extent of a lambda body inside a larger expression is ambiguous
    # without a parser.  Keep the prefix (calls/other operands before the
    # lambda) and let the lambda's own table own the body.
    return text[:pos]


def _handle_import(text, table):
    rest = text[len("import "):]
    for part in _split_top_level(rest, ","):
        part = part.strip()
        if not part:
            continue
        alias_pos = _find_word(part, "as", 0)
        if alias_pos >= 0:
            alias, _ = _read_identifier(part, alias_pos + 2)
            if alias:
                _bind_name(table, alias, DEF_IMPORT)
        else:
            name, _ = _read_identifier(part, 0)
            if name:
                _bind_name(table, name, DEF_IMPORT)


def _handle_from_import(text, table):
    pos = _find_word(text, "import", len("from "))
    if pos < 0:
        return
    rest = text[pos + len("import"):].strip()
    if rest.startswith("(") and rest.endswith(")"):
        rest = rest[1:-1]
    for part in _split_top_level(rest, ","):
        part = part.strip()
        if not part or part == "*":
            continue
        alias_pos = _find_word(part, "as", 0)
        if alias_pos >= 0:
            alias, _ = _read_identifier(part, alias_pos + 2)
            if alias:
                _bind_name(table, alias, DEF_IMPORT)
        else:
            name, _ = _read_identifier(part, 0)
            if name:
                _bind_name(table, name, DEF_IMPORT)


def _handle_global(text, table):
    rest = text[len("global "):]
    for part in _split_top_level(rest, ","):
        name, _ = _read_identifier(part, 0)
        if name:
            _add(table, name, DEF_GLOBAL, GLOBAL_EXPLICIT)


def _handle_nonlocal(text, table):
    rest = text[len("nonlocal "):]
    for part in _split_top_level(rest, ","):
        name, _ = _read_identifier(part, 0)
        if name:
            _add(table, name, DEF_NONLOCAL, FREE)


def _handle_del(text, table):
    rest = text[len("del "):]
    for part in _split_top_level(rest, ","):
        _bind_target(part, table, 0)


def _parse_params(text, child, parent):
    for part in _split_top_level(text, ","):
        part = part.strip()
        if not part or part == "/" or part == "*":
            continue
        while part.startswith("*"):
            part = part[1:].lstrip()
        if not part:
            continue
        equals = _find_equals(part)
        default = ""
        annotation = ""
        target = part
        if equals:
            target = part[:equals[0]]
            default = part[equals[0] + 1:]
        colon = _find_top_level_colon(target)
        if colon >= 0:
            annotation = target[colon + 1:]
            target = target[:colon]
        name, _ = _read_identifier(target, 0)
        if not name:
            continue
        _bind_name(child, name, DEF_PARAM)
        child.varnames.append(name)
        if default:
            _scan_uses(parent, default)


def _process_inline_body(body, lineno, table, tables):
    for statement in _split_statements(body):
        result = _process_statement(statement, lineno, table, tables)
        # A compound statement cannot start a simple suite; ignore any
        # result rather than pushing a bogus block.
        if result is not None:
            pass


def _handle_def(text, lineno, table, tables):
    rest = text.strip()
    if rest.startswith("async "):
        rest = rest[6:].lstrip()
    rest = rest[len("def "):].lstrip()
    name, end = _read_identifier(rest, 0)
    if not name:
        return None
    _bind_name(table, name, DEF_LOCAL)
    child = _new_table(TYPE_FUNCTION, name, lineno, table)
    tables.append(child)
    open_paren = rest.find("(", end)
    after = ""
    if open_paren >= 0:
        close = _matching(rest, open_paren, "(", ")")
        if close >= 0:
            _parse_params(rest[open_paren + 1:close], child, table)
            after = rest[close + 1:]
        else:
            _parse_params(rest[open_paren + 1:], child, table)
    else:
        after = rest[end:]
    colon = _find_top_level_colon(after)
    body = ""
    if colon >= 0:
        body = after[colon + 1:]
    if body.strip():
        _process_inline_body(body, lineno, child, tables)
    return child, True


def _handle_class(text, lineno, table, tables):
    rest = text.strip()[len("class "):].lstrip()
    name, end = _read_identifier(rest, 0)
    if not name:
        return None
    _bind_name(table, name, DEF_LOCAL)
    child = _new_table(TYPE_CLASS, name, lineno, table)
    tables.append(child)
    open_paren = rest.find("(", end)
    after = ""
    if open_paren >= 0:
        close = _matching(rest, open_paren, "(", ")")
        if close >= 0:
            bases = _prepare_expr(rest[open_paren + 1:close], lineno, table, tables)
            _scan_uses(table, bases)
            after = rest[close + 1:]
        else:
            bases = _prepare_expr(rest[open_paren + 1:], lineno, table, tables)
            _scan_uses(table, bases)
    else:
        after = rest[end:]
    colon = _find_top_level_colon(after)
    body = ""
    if colon >= 0:
        body = after[colon + 1:]
    if body.strip():
        _process_inline_body(body, lineno, child, tables)
    return child, True


def _handle_for(text, lineno, table, tables, async_prefix):
    rest = text.strip()
    if async_prefix:
        rest = rest[6:].lstrip()
    rest = rest[len("for "):]
    colon = _find_top_level_colon(rest)
    header = rest[:colon] if colon >= 0 else rest
    body = rest[colon + 1:] if colon >= 0 else ""
    in_pos = _find_word(header, "in", 0)
    if in_pos >= 0:
        target = header[:in_pos]
        iterable = header[in_pos + 2:]
        _bind_target(target, table, 0)
        iterable = _prepare_expr(iterable, lineno, table, tables)
        _scan_uses(table, iterable)
        for name in _comprehension_targets(iterable):
            _bind_name(table, name, DEF_LOCAL | DEF_COMP_ITER)
            _mark_use(table, name)
    else:
        _scan_uses(table, header)
    if body.strip():
        _process_inline_body(body, lineno, table, tables)
    return table, False


def _handle_with(text, lineno, table, tables, async_prefix):
    rest = text.strip()
    if async_prefix:
        rest = rest[6:].lstrip()
    rest = rest[len("with "):]
    colon = _find_top_level_colon(rest)
    header = rest[:colon] if colon >= 0 else rest
    body = rest[colon + 1:] if colon >= 0 else ""
    for context in _split_top_level(header, ","):
        as_pos = _find_word(context, "as", 0)
        if as_pos >= 0:
            expression = _prepare_expr(context[:as_pos], lineno, table, tables)
            _scan_uses(table, expression)
            _bind_target(context[as_pos + 2:], table, 0)
        else:
            expression = _prepare_expr(context, lineno, table, tables)
            _scan_uses(table, expression)
    if body.strip():
        _process_inline_body(body, lineno, table, tables)
    return table, False


def _handle_except(text, lineno, table, tables):
    rest = text.strip()[len("except "):]
    colon = _find_top_level_colon(rest)
    header = rest[:colon] if colon >= 0 else rest
    body = rest[colon + 1:] if colon >= 0 else ""
    as_pos = _find_word(header, "as", 0)
    if as_pos >= 0:
        expression = _prepare_expr(header[:as_pos], lineno, table, tables)
        _scan_uses(table, expression)
        _bind_target(header[as_pos + 2:], table, 0)
    elif header.strip() and header.strip() != "*":
        expression = _prepare_expr(header, lineno, table, tables)
        _scan_uses(table, expression)
    if body.strip():
        _process_inline_body(body, lineno, table, tables)
    return table, False


def _handle_condition(text, lineno, table, tables, keyword):
    rest = text.strip()[len(keyword):]
    colon = _find_top_level_colon(rest)
    condition = rest[:colon] if colon >= 0 else rest
    body = rest[colon + 1:] if colon >= 0 else ""
    condition = _prepare_expr(condition, lineno, table, tables)
    _scan_uses(table, condition)
    if body.strip():
        _process_inline_body(body, lineno, table, tables)
    return table, False


def _bind_pattern(text, table):
    guard = _find_word(text, "if", 0)
    if guard >= 0:
        _scan_uses(table, text[guard + 2:])
        text = text[:guard]
    depth = 0
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "(" or ch == "[" or ch == "{":
            depth += 1
            i += 1
            continue
        if ch == ")" or ch == "]" or ch == "}":
            if depth > 0:
                depth -= 1
            i += 1
            continue
        if ch.isalpha() or ch == "_":
            j = i + 1
            while j < n and (text[j].isalnum() or text[j] == "_"):
                j += 1
            word = text[i:j]
            k = i - 1
            while k >= 0 and text[k] in " \t\r\n":
                k -= 1
            before = text[k] if k >= 0 else ""
            m = j
            while m < n and text[m] in " \t\r\n":
                m += 1
            after = text[m] if m < n else ""
            if word not in _KEYWORDS and word != "_" and before != ".":
                if after == "(":
                    # Class pattern: the name is a value reference.
                    _mark_use(table, word)
                elif not (after == ":" or (after == "=" and depth > 0)):
                    _bind_name(table, word, DEF_LOCAL)
            i = j
            continue
        i += 1


def _handle_case(text, lineno, table, tables):
    rest = text.strip()[len("case "):]
    colon = _find_top_level_colon(rest)
    pattern = rest[:colon] if colon >= 0 else rest
    body = rest[colon + 1:] if colon >= 0 else ""
    _bind_pattern(pattern, table)
    if body.strip():
        _process_inline_body(body, lineno, table, tables)
    return table, False


def _comprehension_targets(text):
    result = []
    i = 0
    while True:
        pos = _find_word(text, "for", i)
        if pos < 0:
            break
        name, end = _read_identifier(text, pos + 3)
        if name:
            result.append(name)
        i = pos + 3
    return result


def _process_statement(text, lineno, table, tables):
    text = text.strip()
    if not text:
        return None
    if text.startswith("@"):
        expression = _prepare_expr(text[1:], lineno, table, tables)
        _scan_uses(table, expression)
        return None
    if text.startswith("async "):
        rest = text[6:].lstrip()
        if rest.startswith("def "):
            return _handle_def(text, lineno, table, tables)
        if rest.startswith("for "):
            return _handle_for(text, lineno, table, tables, True)
        if rest.startswith("with "):
            return _handle_with(text, lineno, table, tables, True)
    if text.startswith("def "):
        return _handle_def(text, lineno, table, tables)
    if text.startswith("class "):
        return _handle_class(text, lineno, table, tables)
    if text.startswith("import "):
        _handle_import(text, table)
        return None
    if text.startswith("from "):
        _handle_from_import(text, table)
        return None
    if text.startswith("global "):
        _handle_global(text, table)
        return None
    if text.startswith("nonlocal "):
        _handle_nonlocal(text, table)
        return None
    if text.startswith("del "):
        _handle_del(text, table)
        return None
    if text.startswith("for "):
        return _handle_for(text, lineno, table, tables, False)
    if text.startswith("with "):
        return _handle_with(text, lineno, table, tables, False)
    if text.startswith("except ") or text.startswith("except:") or text.startswith("except*"):
        return _handle_except(text, lineno, table, tables)
    if text.startswith("if "):
        return _handle_condition(text, lineno, table, tables, "if ")
    if text.startswith("elif "):
        return _handle_condition(text, lineno, table, tables, "elif ")
    if text.startswith("while "):
        return _handle_condition(text, lineno, table, tables, "while ")
    if text == "else:":
        return table, False
    if text.startswith("else:"):
        _process_inline_body(text[len("else:"):], lineno, table, tables)
        return table, False
    if text == "try:":
        return table, False
    if text == "finally:":
        return table, False
    if text.startswith("finally:"):
        _process_inline_body(text[len("finally:"):], lineno, table, tables)
        return table, False
    if text.startswith("match "):
        return _handle_condition(text, lineno, table, tables, "match ")
    if text.startswith("case "):
        return _handle_case(text, lineno, table, tables)
    if text.startswith("type ") and _find_equals(text):
        equals = _find_equals(text)
        name, _ = _read_identifier(text, len("type "))
        if name:
            _bind_name(table, name, DEF_LOCAL)
        if equals:
            _scan_uses(table, text[equals[0] + 1:])
        return None
    if _handle_assignment(text, table, lineno, tables):
        return None
    text = _prepare_expr(text, lineno, table, tables)
    _scan_uses(table, text)
    return None


# ---------------------------------------------------------------------------
# Scope resolution
# ---------------------------------------------------------------------------

def _find_enclosing(table, name):
    parent = table.parent
    while parent is not None:
        if parent.type == TYPE_FUNCTION:
            flags = parent.symbols.get(name, 0)
            if flags & DEF_BOUND or ((flags >> SCOPE_OFF) & SCOPE_MASK) == CELL:
                return parent
        elif parent.type == TYPE_MODULE:
            return None
        parent = parent.parent
    return None


def _resolve_scopes(tables):
    # A ``global`` declaration makes the name explicit at module scope too.
    module = tables[0]
    for table in tables:
        if table.type == TYPE_MODULE:
            continue
        for name in table.symbols.keys():
            if table.symbols[name] & DEF_GLOBAL:
                module.symbols[name] = _with_scope(
                    module.symbols.get(name, 0) | DEF_GLOBAL, GLOBAL_EXPLICIT)
    # Resolve genuinely free variables in nested functions to enclosing
    # function cells.  Class scopes never act as closures.
    for table in tables:
        for name in list(table.symbols.keys()):
            flags = table.symbols[name]
            if flags & DEF_GLOBAL:
                table.symbols[name] = _with_scope(flags, GLOBAL_EXPLICIT)
                continue
            if flags & DEF_NONLOCAL:
                enclosing = _find_enclosing(table, name)
                if enclosing is not None:
                    enclosing.symbols[name] = _with_scope(
                        enclosing.symbols.get(name, 0), CELL)
                table.symbols[name] = _with_scope(flags, FREE)
                continue
            if flags & DEF_BOUND:
                table.symbols[name] = _with_scope(flags, LOCAL)
                continue
            if not (flags & USE):
                continue
            if table.type == TYPE_MODULE:
                table.symbols[name] = _with_scope(flags, GLOBAL_IMPLICIT)
                continue
            enclosing = _find_enclosing(table, name)
            if enclosing is not None:
                table.symbols[name] = _with_scope(flags, FREE)
                enclosing.symbols[name] = _with_scope(
                    enclosing.symbols.get(name, 0), CELL)
            else:
                table.symbols[name] = _with_scope(flags, GLOBAL_IMPLICIT)


def _with_scope(flags, scope):
    flags = flags & ~(SCOPE_MASK << SCOPE_OFF)
    return flags | ((scope & SCOPE_MASK) << SCOPE_OFF)


# ---------------------------------------------------------------------------
# Public entry point (same shape as CPython's builtin)
# ---------------------------------------------------------------------------

def symtable(source, filename, compile_type):
    """Return the top-level raw symbol table for *source*."""
    if isinstance(source, bytes):
        source = source.decode("utf-8")
    elif not isinstance(source, str):
        raise TypeError("symtable() argument 1 must be str or bytes")
    if not isinstance(compile_type, str):
        raise TypeError("symtable() argument 3 must be str")
    if compile_type == "exec":
        pass
    elif compile_type == "eval":
        pass
    elif compile_type == "single":
        pass
    else:
        raise ValueError("symtable() arg 3 must be 'exec' or 'eval' or 'single'")
    if isinstance(filename, bytes):
        filename = filename.decode("utf-8")
    top = _new_table(TYPE_MODULE, "top", 0, None)
    tables = [top]
    masked = _mask_source(source)
    lines = _logical_lines(masked)
    stack = [(-1, top, True)]
    for lineno, indent, text in lines:
        while len(stack) > 1 and indent <= stack[-1][0]:
            stack.pop()
        current = stack[-1][1]
        for statement in _split_statements(text):
            result = _process_statement(statement, lineno, current, tables)
            if result is not None:
                push_table, is_scope = result
                stack.append((indent, push_table, is_scope))
    _resolve_scopes(tables)
    return top
