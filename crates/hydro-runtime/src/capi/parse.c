/*
 * The variadic CPython entry points used by limited-API extensions.
 *
 * This file is compiled by the `cc` crate from the Hydro runtime build script.
 * It deliberately includes no CPython headers: the shim objects are opaque
 * here, and every conversion is forwarded to a Rust accessor exported by
 * `crates/hydro-runtime/src/capi/mod.rs`.
 *
 * Supported format characters (matching the subset implemented in Rust):
 *   i  int *            l  long *            n  Py_ssize_t *
 *   d  double *         s  const char **     O  PyObject **
 *   p  int * (truth)
 *
 * `|` starts the optional arguments; parsing stops successfully once an
 * optional argument is missing. `$` (keyword variant only) starts the
 * keyword-only arguments. `:` and `;` end the format (the trailing text is
 * ignored here; the keyword parser uses the text after `:` as the function
 * name in its messages). On any failure the Rust side records a thread-local
 * error and the entry point returns 0, exactly like CPython.
 *
 * `PyArg_ParseTupleAndKeywords` follows CPython's contract: `keywords` is a
 * NULL-terminated array with one entry per format unit, an empty string marks
 * a positional-only parameter, and keyword arguments are matched by name.
 * The implementation is allocation-free; the diagnostics are simplified (no
 * "did you mean" suggestions) but every rejection is a `TypeError` or a
 * `SystemError`, as in CPython.
 *
 * `PyErr_Format` formats with C99 `vsnprintf` semantics. The CPython-specific
 * conversions (`%R`, `%S`, `%V`, `%A`, `%U`, `%T`) would need Python objects
 * and `%n` would let a format string write through a caller pointer, so both
 * are rejected instead of being passed to `vsnprintf`.
 */

#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <math.h>

typedef struct _hydro_shim_object PyObject;
typedef ptrdiff_t Py_ssize_t;

/* Bound on the keyword-name array and on the stack message buffer. The
 * keyword parser needs no heap at all; `PyErr_Format` only falls back to the
 * heap when a formatted message does not fit. */
#define HYDRO_CAPI_MAX_KEYWORDS 256
#define HYDRO_CAPI_MESSAGE_SIZE 512

/*
 * Py_ssize_t is `long` on the supported x86_64 (LP64) target and therefore
 * representation-compatible with `ptrdiff_t`; the Rust accessor writes an
 * `isize`.
 */
extern size_t hydro_capi_arg_count(PyObject *args);
extern PyObject *hydro_capi_arg(PyObject *args, size_t index);
extern int hydro_capi_is_tuple(PyObject *object);
extern int hydro_capi_kw_is_dict(PyObject *keywords);
extern size_t hydro_capi_kw_count(PyObject *keywords);
extern const char *hydro_capi_kw_key(PyObject *keywords, size_t index);
extern PyObject *hydro_capi_kw_value(PyObject *keywords, size_t index);
extern int hydro_capi_parse_i(PyObject *object, int *out);
extern int hydro_capi_parse_l(PyObject *object, long *out);
extern int hydro_capi_parse_n(PyObject *object, ptrdiff_t *out);
extern int hydro_capi_parse_d(PyObject *object, double *out);
extern int hydro_capi_parse_s(PyObject *object, const char **out);
extern int hydro_capi_parse_o(PyObject *object, PyObject **out);
extern int hydro_capi_parse_p(PyObject *object, int *out);
extern void hydro_capi_set_error(const char *message);
extern void hydro_capi_set_type_error(const char *message);
extern void hydro_capi_set_exception_message(PyObject *exception, const char *message);
extern void *hydro_capi_vectorcall(void *callable, void *const *args,
                                   size_t nargs);

static int hydro_capi_is_space(char code)
{
    return code == ' ' || code == '\t' || code == '\n' || code == '\r' ||
           code == '\f' || code == '\v';
}

/* Records "<api_name>: NULL output pointer for '<code>'". */
static void hydro_capi_null_pointer(const char *api_name, char code)
{
    char message[160];

    snprintf(message, sizeof message, "%s: NULL output pointer for '%c'", api_name,
             code);
    hydro_capi_set_type_error(message);
}

/* Records "<api_name>: unsupported format character ..." */
static void hydro_capi_unsupported_character(const char *api_name, char code)
{
    char message[160];

    if (code >= 0x20 && code < 0x7f) {
        snprintf(message, sizeof message,
                 "%s: unsupported format character '%c' (0x%02x)", api_name, code,
                 (unsigned)(unsigned char)code);
    }
    else {
        snprintf(message, sizeof message,
                 "%s: unsupported format character 0x%02x", api_name,
                 (unsigned)(unsigned char)code);
    }
    hydro_capi_set_type_error(message);
}

/*
 * Converts one argument for `code`, storing through the pointer the caller
 * supplied for that format unit. Returns 1 on success, 0 with an error set.
 */
static int hydro_capi_convert(char code, PyObject *item, va_list *arguments,
                              const char *api_name)
{
    switch (code) {
    case 'i': {
        int *out = va_arg(*arguments, int *);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_i(item, out);
    }
    case 'l': {
        long *out = va_arg(*arguments, long *);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_l(item, out);
    }
    case 'n': {
        ptrdiff_t *out = va_arg(*arguments, ptrdiff_t *);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_n(item, out);
    }
    case 'd': {
        double *out = va_arg(*arguments, double *);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_d(item, out);
    }
    case 's': {
        const char **out = va_arg(*arguments, const char **);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_s(item, out);
    }
    case 'O': {
        PyObject **out = va_arg(*arguments, PyObject **);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_o(item, out);
    }
    case 'p': {
        int *out = va_arg(*arguments, int *);
        if (out == NULL) {
            hydro_capi_null_pointer(api_name, code);
            return 0;
        }
        return hydro_capi_parse_p(item, out);
    }
    default:
        hydro_capi_unsupported_character(api_name, code);
        return 0;
    }
}

int PyArg_ParseTuple(PyObject *args, const char *format, ...)
{
    va_list arguments;
    const char *cursor;
    size_t index = 0;
    size_t count;
    int optional = 0;
    int status = 1;

    if (format == NULL) {
        hydro_capi_set_type_error("PyArg_ParseTuple: format is NULL");
        return 0;
    }
    if (args == NULL) {
        hydro_capi_set_type_error("PyArg_ParseTuple: argument tuple is NULL");
        return 0;
    }

    count = hydro_capi_arg_count(args);

    va_start(arguments, format);
    for (cursor = format; *cursor != '\0'; cursor++) {
        char code = *cursor;
        PyObject *item;

        if (hydro_capi_is_space(code)) {
            continue;
        }
        if (code == '|') {
            optional = 1;
            continue;
        }
        if (code == ':' || code == ';') {
            break;
        }

        if (index >= count) {
            if (optional) {
                break;
            }
            hydro_capi_set_type_error("PyArg_ParseTuple: not enough arguments");
            status = 0;
            break;
        }
        item = hydro_capi_arg(args, index);
        index += 1;

        if (!hydro_capi_convert(code, item, &arguments, "PyArg_ParseTuple")) {
            status = 0;
            break;
        }
    }
    va_end(arguments);

    if (status && index < count) {
        hydro_capi_set_type_error("PyArg_ParseTuple: too many arguments");
        status = 0;
    }
    return status;
}

/* Number of value-consuming format units, stopping at ':' or ';'. */
static size_t hydro_capi_format_units(const char *format)
{
    const char *cursor;
    size_t units = 0;

    for (cursor = format; *cursor != '\0'; cursor++) {
        if (hydro_capi_is_space(*cursor) || *cursor == '|' || *cursor == '$') {
            continue;
        }
        if (*cursor == ':' || *cursor == ';') {
            break;
        }
        units += 1;
    }
    return units;
}

/* Records a TypeError for the keyword parser: "<function>: <detail>". */
static void hydro_capi_keyword_error(const char *function, const char *detail)
{
    char message[HYDRO_CAPI_MESSAGE_SIZE];

    snprintf(message, sizeof message, "%s: %s", function, detail);
    hydro_capi_set_type_error(message);
}

/* Records a SystemError for a malformed keyword list. */
static void hydro_capi_keyword_system_error(const char *function, const char *detail)
{
    char message[HYDRO_CAPI_MESSAGE_SIZE];

    snprintf(message, sizeof message, "SystemError: %s: %s", function, detail);
    hydro_capi_set_error(message);
}

/*
 * Looks a keyword up by name. A NULL return means "not supplied": shim dict
 * values are never NULL, so this is unambiguous inside this host.
 */
static PyObject *hydro_capi_kw_find(PyObject *keywords, const char *name)
{
    size_t count;
    size_t index;

    if (keywords == NULL || name == NULL || name[0] == '\0') {
        return NULL;
    }
    count = hydro_capi_kw_count(keywords);
    for (index = 0; index < count; index++) {
        const char *key = hydro_capi_kw_key(keywords, index);
        if (key != NULL && strcmp(key, name) == 0) {
            return hydro_capi_kw_value(keywords, index);
        }
    }
    return NULL;
}

/* True when `name` names a unit of the format. */
static int hydro_capi_kw_known(char *const *names, size_t start, size_t len,
                               const char *name)
{
    size_t index;

    for (index = start; index < len; index++) {
        if (strcmp(name, names[index]) == 0) {
            return 1;
        }
    }
    return 0;
}

static int hydro_capi_parse_keywords(PyObject *args, PyObject *keywords,
                                     const char *format, char *const *names,
                                     va_list *arguments)
{
    const char *cursor;
    const char *function = "PyArg_ParseTupleAndKeywords";
    char detail[HYDRO_CAPI_MESSAGE_SIZE];
    size_t units;
    size_t len = 0;
    size_t positionals_only = 0;
    size_t minimum = (size_t)-1; /* index where '|' appeared */
    size_t maximum = (size_t)-1; /* index where '$' appeared */
    size_t nargs;
    size_t nkwargs;
    size_t index;
    int status = 1;

    if (format == NULL) {
        hydro_capi_set_type_error(
            "PyArg_ParseTupleAndKeywords: format is NULL");
        return 0;
    }
    if (args == NULL) {
        hydro_capi_set_type_error(
            "PyArg_ParseTupleAndKeywords: argument tuple is NULL");
        return 0;
    }
    if (!hydro_capi_kw_is_dict(keywords)) {
        hydro_capi_set_type_error(
            "PyArg_ParseTupleAndKeywords: keywords must be a dict");
        return 0;
    }

    {
        const char *colon = strchr(format, ':');
        if (colon != NULL && colon[1] != '\0') {
            /* CPython uses the trailing text as the function name. */
            function = colon + 1;
        }
    }

    if (names == NULL) {
        hydro_capi_set_type_error(
            "PyArg_ParseTupleAndKeywords: keyword name array is NULL");
        return 0;
    }
    while (names[len] != NULL) {
        len += 1;
        if (len > HYDRO_CAPI_MAX_KEYWORDS) {
            hydro_capi_set_type_error(
                "PyArg_ParseTupleAndKeywords: too many keyword names");
            return 0;
        }
    }
    units = hydro_capi_format_units(format);
    if (len != units) {
        snprintf(detail, sizeof detail, "%zu keyword names for %zu format units",
                 len, units);
        hydro_capi_keyword_system_error(function, detail);
        return 0;
    }

    /* Leading empty names are positional-only parameters (PEP 570). */
    while (positionals_only < len && names[positionals_only][0] == '\0') {
        positionals_only += 1;
    }
    for (index = positionals_only; index < len; index++) {
        if (names[index][0] == '\0') {
            hydro_capi_keyword_system_error(
                function, "empty keyword parameter name after a named parameter");
            return 0;
        }
    }

    nargs = hydro_capi_arg_count(args);
    nkwargs = hydro_capi_kw_count(keywords);
    if (nargs + nkwargs > len) {
        snprintf(detail, sizeof detail, "takes at most %zu arguments (%zu given)",
                 len, nargs + nkwargs);
        hydro_capi_keyword_error(function, detail);
        return 0;
    }

    cursor = format;
    for (index = 0; index < len; index++) {
        char code;
        const char *name;
        PyObject *item;

        if (*cursor == '|') {
            if (minimum != (size_t)-1) {
                hydro_capi_keyword_system_error(
                    function, "invalid format string ('|' specified twice)");
                status = 0;
                break;
            }
            if (maximum != (size_t)-1) {
                hydro_capi_keyword_system_error(
                    function, "invalid format string ('|' after '$')");
                status = 0;
                break;
            }
            minimum = index;
            cursor += 1;
        }
        if (*cursor == '$') {
            if (maximum != (size_t)-1) {
                hydro_capi_keyword_system_error(
                    function, "invalid format string ('$' specified twice)");
                status = 0;
                break;
            }
            maximum = index;
            cursor += 1;
            if (maximum < positionals_only) {
                hydro_capi_keyword_system_error(
                    function, "empty keyword parameter name after '$'");
                status = 0;
                break;
            }
            if (index < nargs) {
                snprintf(detail, sizeof detail,
                         "takes at most %zu positional argument%s (%zu given)",
                         maximum, maximum == 1 ? "" : "s", nargs);
                hydro_capi_keyword_error(function, detail);
                status = 0;
                break;
            }
        }
        while (hydro_capi_is_space(*cursor)) {
            cursor += 1;
        }
        if (*cursor == '\0' || *cursor == ':' || *cursor == ';') {
            snprintf(detail, sizeof detail,
                     "format string is shorter than the %zu keyword names", len);
            hydro_capi_keyword_system_error(function, detail);
            status = 0;
            break;
        }
        code = *cursor;
        cursor += 1;
        name = names[index];

        if (index < nargs) {
            item = hydro_capi_arg(args, index);
            if (name[0] != '\0' &&
                hydro_capi_kw_find(keywords, name) != NULL) {
                snprintf(detail, sizeof detail,
                         "argument for '%s' given by name and position (%zu)",
                         name, index + 1);
                hydro_capi_keyword_error(function, detail);
                status = 0;
                break;
            }
        }
        else {
            item = NULL;
            if (index >= positionals_only && name[0] != '\0') {
                item = hydro_capi_kw_find(keywords, name);
            }
            if (item == NULL) {
                if (index >= minimum) {
                    /* Optional argument: leave the caller's variable alone. */
                    continue;
                }
                if (index < positionals_only) {
                    snprintf(detail, sizeof detail,
                             "takes at least %zu positional argument%s (%zu given)",
                             minimum, minimum == 1 ? "" : "s", nargs);
                }
                else {
                    snprintf(detail, sizeof detail,
                             "missing required argument '%s' (pos %zu)", name,
                             index + 1);
                }
                hydro_capi_keyword_error(function, detail);
                status = 0;
                break;
            }
        }

        if (!hydro_capi_convert(code, item, arguments, function)) {
            status = 0;
            break;
        }
    }
    if (!status) {
        return 0;
    }

    if (nkwargs > 0) {
        /* No argument may arrive both positionally and by name. */
        for (index = positionals_only; index < nargs; index++) {
            const char *name = names[index];
            if (hydro_capi_kw_find(keywords, name) != NULL) {
                snprintf(detail, sizeof detail,
                         "argument for '%s' given by name and position (%zu)",
                         name, index + 1);
                hydro_capi_keyword_error(function, detail);
                return 0;
            }
        }
        /* Every supplied keyword must name a parameter. */
        for (index = 0; index < nkwargs; index++) {
            const char *key = hydro_capi_kw_key(keywords, index);
            if (key == NULL) {
                hydro_capi_set_type_error(
                    "PyArg_ParseTupleAndKeywords: keywords must be strings");
                return 0;
            }
            if (!hydro_capi_kw_known(names, positionals_only, len, key)) {
                snprintf(detail, sizeof detail,
                         "got an unexpected keyword argument '%.200s'", key);
                hydro_capi_keyword_error(function, detail);
                return 0;
            }
        }
    }
    return 1;
}

int PyArg_ParseTupleAndKeywords(PyObject *args, PyObject *keywords,
                                const char *format, char *const *names, ...)
{
    va_list arguments;
    int status;

    va_start(arguments, names);
    status = hydro_capi_parse_keywords(args, keywords, format, names, &arguments);
    va_end(arguments);
    return status;
}

int PyArg_VaParseTupleAndKeywords(PyObject *args, PyObject *keywords,
                                  const char *format, char *const *names,
                                  va_list vargs)
{
    va_list copy;
    int status;

    va_copy(copy, vargs);
    status = hydro_capi_parse_keywords(args, keywords, format, names, &copy);
    va_end(copy);
    return status;
}

/*
 * `PyArg_UnpackTuple`: extracts borrowed references from a tuple without any
 * format string. The diagnostic wording follows CPython.
 */
int PyArg_UnpackTuple(PyObject *args, const char *name, Py_ssize_t min,
                      Py_ssize_t max, ...)
{
    va_list arguments;
    size_t nargs;
    Py_ssize_t index;

    if (args == NULL) {
        hydro_capi_set_error("SystemError: PyArg_UnpackTuple: args is NULL");
        return 0;
    }
    if (!hydro_capi_is_tuple(args)) {
        hydro_capi_set_type_error(
            "PyArg_UnpackTuple: argument list must be a tuple");
        return 0;
    }
    if (min < 0 || max < min) {
        hydro_capi_set_error("SystemError: PyArg_UnpackTuple: invalid bounds");
        return 0;
    }
    nargs = hydro_capi_arg_count(args);
    if ((Py_ssize_t)nargs < min || (Py_ssize_t)nargs > max) {
        char message[256];
        const char *prefix = name != NULL ? name : "function";

        if (min == max) {
            snprintf(message, sizeof message, "%s expected %zd argument%s, got %zu",
                     prefix, min, min == 1 ? "" : "s", nargs);
        }
        else if (min == 0) {
            snprintf(message, sizeof message, "%s expected at most %zd argument%s, got %zu",
                     prefix, max, max == 1 ? "" : "s", nargs);
        }
        else {
            snprintf(message, sizeof message, "%s expected at least %zd argument%s, got %zu",
                     prefix, min, min == 1 ? "" : "s", nargs);
        }
        hydro_capi_set_type_error(message);
        return 0;
    }
    va_start(arguments, max);
    for (index = 0; index < (Py_ssize_t)nargs; index++) {
        PyObject **slot = va_arg(arguments, PyObject **);
        if (slot == NULL) {
            va_end(arguments);
            hydro_capi_set_type_error("PyArg_UnpackTuple: NULL output pointer");
            return 0;
        }
        *slot = hydro_capi_arg(args, (size_t)index);
    }
    va_end(arguments);
    return 1;
}

/*
 * Returns the first conversion character `PyErr_Format` cannot support, or 0
 * when every conversion is plain C99. `%R`, `%S`, `%V`, `%A`, `%U` and `%T`
 * need Python objects, and `%n` would let the format string write through a
 * caller pointer, so the host refuses both.
 */
static char hydro_capi_unsupported_conversion(const char *format)
{
    const char *cursor;

    for (cursor = format; *cursor != '\0'; cursor++) {
        if (*cursor != '%') {
            continue;
        }
        cursor += 1;
        if (*cursor == '%') {
            continue;
        }
        /* Skip flags, width, precision and length modifiers. */
        while (*cursor == '-' || *cursor == '+' || *cursor == ' ' ||
               *cursor == '#' || *cursor == '0' || *cursor == '.' ||
               (*cursor >= '0' && *cursor <= '9')) {
            cursor += 1;
        }
        while (*cursor == 'h' || *cursor == 'l' || *cursor == 'L' ||
               *cursor == 'j' || *cursor == 'z' || *cursor == 't' ||
               *cursor == 'q') {
            cursor += 1;
        }
        switch (*cursor) {
        case 'R':
        case 'S':
        case 'V':
        case 'A':
        case 'U':
        case 'T':
        case 'n':
            return *cursor;
        case '\0':
            return 0;
        default:
            break;
        }
    }
    return 0;
}

PyObject *PyErr_Format(PyObject *exception, const char *format, ...)
{
    va_list arguments;
    va_list copy;
    char stack_buffer[HYDRO_CAPI_MESSAGE_SIZE];
    char *message = stack_buffer;
    int needed;

    if (format == NULL) {
        hydro_capi_set_type_error("PyErr_Format: format is NULL");
        return NULL;
    }
    {
        char unsupported = hydro_capi_unsupported_conversion(format);
        if (unsupported != 0) {
            char message[96];
            snprintf(message, sizeof message,
                     "PyErr_Format: conversion '%%%c' is not supported",
                     unsupported);
            hydro_capi_set_type_error(message);
            return NULL;
        }
    }

    va_start(arguments, format);
    va_copy(copy, arguments);
    needed = vsnprintf(stack_buffer, sizeof stack_buffer, format, arguments);
    va_end(arguments);
    if (needed < 0) {
        va_end(copy);
        hydro_capi_set_type_error("PyErr_Format: format string is invalid");
        return NULL;
    }
    if ((size_t)needed >= sizeof stack_buffer) {
        /* The message did not fit: format it again into an exact buffer. */
        message = malloc((size_t)needed + 1);
        if (message == NULL) {
            va_end(copy);
            hydro_capi_set_type_error("PyErr_Format: cannot allocate the message");
            return NULL;
        }
        vsnprintf(message, (size_t)needed + 1, format, copy);
    }
    va_end(copy);

    hydro_capi_set_exception_message(exception, message);
    if (message != stack_buffer) {
        free(message);
    }
    return NULL;
}

/*
 * The variadic convenience form of a positional call. `NULL` terminates the
 * argument list, exactly as CPython documents. The arguments are borrowed for
 * the duration of the call; the Rust helper builds the PEP 590 vector and
 * returns a new reference (or NULL with a pending error).
 *
 * Two passes over the `va_list` are used: the first counts, the second fills
 * either a stack buffer or an exact heap allocation. This keeps the common
 * small call allocation-free.
 */
PyObject *PyObject_CallFunctionObjArgs(PyObject *callable, ...)
{
    va_list arguments;
    size_t count = 0;
    size_t index;
    PyObject *stack_items[16];
    PyObject **items = stack_items;
    PyObject *result;

    if (callable == NULL) {
        hydro_capi_set_type_error(
            "PyObject_CallFunctionObjArgs: callable is NULL");
        return NULL;
    }

    va_start(arguments, callable);
    for (;;) {
        PyObject *item = va_arg(arguments, PyObject *);
        if (item == NULL) {
            break;
        }
        if (count == (size_t)-1) {
            va_end(arguments);
            hydro_capi_set_type_error(
                "PyObject_CallFunctionObjArgs: too many arguments");
            return NULL;
        }
        count += 1;
    }
    va_end(arguments);

    if (count > sizeof stack_items / sizeof stack_items[0]) {
        if (count > ((size_t)-1) / sizeof(PyObject *)) {
            hydro_capi_set_type_error(
                "PyObject_CallFunctionObjArgs: too many arguments");
            return NULL;
        }
        items = malloc(count * sizeof(PyObject *));
        if (items == NULL) {
            hydro_capi_set_type_error(
                "MemoryError: cannot allocate the argument vector");
            return NULL;
        }
    }

    va_start(arguments, callable);
    for (index = 0; index < count; index++) {
        items[index] = va_arg(arguments, PyObject *);
    }
    /* Consume the terminator so both passes walk the same list. */
    (void)va_arg(arguments, PyObject *);
    va_end(arguments);

    result = hydro_capi_vectorcall(callable, (void *const *)items, count);
    if (items != stack_items) {
        free(items);
    }
    return result;
}

/*
 * PyUnicodeWriter_Format: printf-style formatting into the opaque writer.
 * The writer itself lives in Rust; this entry point only needs the variadic
 * formatting step, which Rust cannot express.
 */
typedef struct PyUnicodeWriter PyUnicodeWriter;
extern int PyUnicodeWriter_WriteUTF8(PyUnicodeWriter *writer,
                                     const char *data,
                                     long length);

int PyUnicodeWriter_Format(PyUnicodeWriter *writer, const char *format, ...)
{
    va_list arguments;
    char stack_buffer[HYDRO_CAPI_MESSAGE_SIZE];
    char *heap_buffer = NULL;
    char *buffer = stack_buffer;
    int needed;
    int status;

    if (writer == NULL || format == NULL) {
        hydro_capi_set_error(
            "SystemError: PyUnicodeWriter_Format requires a writer and a format");
        return -1;
    }

    va_start(arguments, format);
    needed = vsnprintf(stack_buffer, sizeof stack_buffer, format, arguments);
    va_end(arguments);
    if (needed < 0) {
        hydro_capi_set_error("ValueError: invalid PyUnicodeWriter_Format format");
        return -1;
    }
    if ((size_t)needed >= sizeof stack_buffer) {
        heap_buffer = malloc((size_t)needed + 1);
        if (heap_buffer == NULL) {
            hydro_capi_set_error(
                "MemoryError: cannot allocate the formatted string");
            return -1;
        }
        va_start(arguments, format);
        (void)vsnprintf(heap_buffer, (size_t)needed + 1, format, arguments);
        va_end(arguments);
        buffer = heap_buffer;
    }

    status = PyUnicodeWriter_WriteUTF8(writer, buffer, (long)needed);
    free(heap_buffer);
    return status;
}

/*
 * ---------------------------------------------------------------------------
 * Py_BuildValue and the variadic object-building entry points.
 *
 * Values are built through the public constructors and assembled by a small
 * explicit stack, so nested tuples/lists/dicts are real containers and every
 * failure reports a Python exception. Unsupported format characters are
 * diagnosed instead of being skipped.
 * ---------------------------------------------------------------------------
 */

extern void Py_IncRef(PyObject *);
extern void Py_DecRef(PyObject *);
extern PyObject *PyLong_FromLongLong(long long);
extern PyObject *PyLong_FromUnsignedLongLong(unsigned long long);
extern PyObject *PyFloat_FromDouble(double);
extern PyObject *PyComplex_FromDoubles(double, double);
extern PyObject *PyUnicode_FromStringAndSize(const char *, ptrdiff_t);
extern PyObject *PyBytes_FromStringAndSize(const char *, ptrdiff_t);
extern PyObject *PyList_New(ptrdiff_t);
extern int PyList_Append(PyObject *, PyObject *);
extern PyObject *PyList_AsTuple(PyObject *);
extern PyObject *PyDict_New(void);
extern int PyDict_SetItem(PyObject *, PyObject *, PyObject *);
extern PyObject *PyTuple_New(Py_ssize_t);
extern int PyTuple_SetItem(PyObject *, Py_ssize_t, PyObject *);
extern PyObject *PyObject_Str(PyObject *);
extern PyObject *PyObject_Repr(PyObject *);
extern PyObject *PyObject_GetAttrString(PyObject *, const char *);
extern PyObject *PyObject_CallObject(PyObject *, PyObject *);
extern PyObject *PyObject_Call(PyObject *, PyObject *, PyObject *);
extern int PyObject_IsInstance(PyObject *, PyObject *);
extern const char *PyUnicode_AsUTF8(PyObject *);
extern PyObject *Py_GetConstant(unsigned int);
extern PyObject *hydro_capi_tuple_pack(ptrdiff_t, PyObject **);
extern PyObject *hydro_capi_ascii_repr(PyObject *);
extern int hydro_capi_is_tuple(PyObject *);
extern PyObject *hydro_capi_object_type_name(PyObject *);

typedef struct {
    double real;
    double imag;
} hydro_capi_py_complex;

static PyObject *hydro_capi_none(void)
{
    return Py_GetConstant(0); /* PY_CONSTANT_NONE */
}

/* Encodes one code point as UTF-8; returns 0 for an invalid scalar. */
static int hydro_capi_utf8_encode(unsigned long cp, char out[4])
{
    if (cp < 0x80) {
        out[0] = (char)cp;
        return 1;
    }
    if (cp < 0x800) {
        out[0] = (char)(0xC0 | (cp >> 6));
        out[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp < 0x10000) {
        if (cp >= 0xD800 && cp <= 0xDFFF) {
            return 0;
        }
        out[0] = (char)(0xE0 | (cp >> 12));
        out[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    if (cp <= 0x10FFFF) {
        out[0] = (char)(0xF0 | (cp >> 18));
        out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
        out[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[3] = (char)(0x80 | (cp & 0x3F));
        return 4;
    }
    return 0;
}

#define HYDRO_CAPI_BV_DEPTH 64
#define HYDRO_CAPI_BV_TUPLE 0
#define HYDRO_CAPI_BV_LIST 1
#define HYDRO_CAPI_BV_DICT 2

typedef struct {
    PyObject *container;
    PyObject *key;
    int kind;
    size_t count;
} hydro_capi_bv_frame;

typedef struct {
    hydro_capi_bv_frame frames[HYDRO_CAPI_BV_DEPTH];
    int depth;
    PyObject *result;
} hydro_capi_bv_state;

static void hydro_capi_bv_release(hydro_capi_bv_state *state)
{
    int index;

    for (index = state->depth - 1; index >= 0; index--) {
        hydro_capi_bv_frame *frame = &state->frames[index];
        if (frame->key != NULL) {
            Py_DecRef(frame->key);
        }
        if (frame->container != NULL) {
            Py_DecRef(frame->container);
        }
    }
    if (state->result != NULL) {
        Py_DecRef(state->result);
    }
    state->depth = 0;
    state->result = NULL;
}

/* Adds a value whose reference the state takes over. */
static int hydro_capi_bv_add(hydro_capi_bv_state *state, PyObject *value)
{
    hydro_capi_bv_frame *frame;

    if (state->depth == 0) {
        if (state->result != NULL) {
            Py_DecRef(value);
            hydro_capi_set_error(
                "TypeError: Py_BuildValue: more than one value in format");
            return -1;
        }
        state->result = value;
        return 0;
    }
    frame = &state->frames[state->depth - 1];
    if (frame->kind == HYDRO_CAPI_BV_DICT) {
        if (frame->count % 2 == 0) {
            frame->key = value;
        }
        else {
            int status = PyDict_SetItem(frame->container, frame->key, value);
            Py_DecRef(frame->key);
            frame->key = NULL;
            Py_DecRef(value);
            if (status != 0) {
                return -1;
            }
        }
    }
    else {
        if (PyList_Append(frame->container, value) != 0) {
            Py_DecRef(value);
            return -1;
        }
        Py_DecRef(value);
    }
    frame->count += 1;
    return 0;
}

static int hydro_capi_bv_open(hydro_capi_bv_state *state, char code)
{
    hydro_capi_bv_frame *frame;
    PyObject *container;

    if (state->depth >= HYDRO_CAPI_BV_DEPTH) {
        hydro_capi_set_error("SystemError: Py_BuildValue: nesting too deep");
        return -1;
    }
    if (code == '{') {
        container = PyDict_New();
        frame = &state->frames[state->depth];
        frame->kind = HYDRO_CAPI_BV_DICT;
    }
    else {
        container = PyList_New(0);
        frame = &state->frames[state->depth];
        frame->kind = code == '[' ? HYDRO_CAPI_BV_LIST : HYDRO_CAPI_BV_TUPLE;
    }
    if (container == NULL) {
        return -1;
    }
    frame->container = container;
    frame->key = NULL;
    frame->count = 0;
    state->depth += 1;
    return 0;
}

static int hydro_capi_bv_close(hydro_capi_bv_state *state, char code)
{
    hydro_capi_bv_frame *frame;
    PyObject *container;
    int expected;

    if (state->depth == 0) {
        hydro_capi_set_error("SystemError: Py_BuildValue: unbalanced closing bracket");
        return -1;
    }
    expected = code == '}' ? HYDRO_CAPI_BV_DICT
             : code == ']' ? HYDRO_CAPI_BV_LIST : HYDRO_CAPI_BV_TUPLE;
    frame = &state->frames[state->depth - 1];
    if (frame->kind != expected) {
        hydro_capi_set_error("SystemError: Py_BuildValue: mismatched closing bracket");
        return -1;
    }
    if (frame->kind == HYDRO_CAPI_BV_DICT) {
        if (frame->count % 2 != 0) {
            hydro_capi_set_error(
                "TypeError: Py_BuildValue: dictionary format needs an even number of values");
            return -1;
        }
        if (frame->key != NULL) {
            Py_DecRef(frame->key);
            frame->key = NULL;
        }
    }
    container = frame->container;
    state->depth -= 1;
    frame->container = NULL;
    if (frame->kind == HYDRO_CAPI_BV_TUPLE) {
        PyObject *tuple = PyList_AsTuple(container);
        Py_DecRef(container);
        if (tuple == NULL) {
            return -1;
        }
        container = tuple;
    }
    return hydro_capi_bv_add(state, container);
}

/* `O!`: validates that `object` is an instance of `type`. */
static int hydro_capi_bv_typecheck(PyObject *object, PyObject *type)
{
    int status = PyObject_IsInstance(object, type);

    if (status == 1) {
        return 1;
    }
    if (status == 0) {
        PyObject *type_name = hydro_capi_object_type_name(object);
        PyObject *expected = PyObject_Str(type);
        const char *actual = type_name != NULL ? PyUnicode_AsUTF8(type_name) : NULL;
        const char *wanted = expected != NULL ? PyUnicode_AsUTF8(expected) : NULL;
        char message[512];
        snprintf(message, sizeof message, "TypeError: argument must be %s, not %s",
                 wanted != NULL ? wanted : "the requested type",
                 actual != NULL ? actual : "the supplied object");
        hydro_capi_set_error(message);
        if (type_name != NULL) {
            Py_DecRef(type_name);
        }
        if (expected != NULL) {
            Py_DecRef(expected);
        }
        return 0;
    }
    return 0;
}

static PyObject *hydro_capi_build_value_va(const char *format, va_list *arguments)
{
    hydro_capi_bv_state state;
    const char *cursor;

    state.depth = 0;
    state.result = NULL;
    if (format == NULL) {
        return NULL;
    }
    cursor = format;
    while (*cursor != '\0') {
        char code = *cursor++;
        PyObject *value = NULL;

        if (hydro_capi_is_space(code) || code == ',' || code == ':' || code == '$') {
            continue;
        }
        if (code == '(' || code == '[' || code == '{') {
            if (hydro_capi_bv_open(&state, code) != 0) {
                goto fail;
            }
            continue;
        }
        if (code == ')' || code == ']' || code == '}') {
            if (hydro_capi_bv_close(&state, code) != 0) {
                goto fail;
            }
            continue;
        }

        if (code == 's' || code == 'y' || code == 'z' || code == 'u') {
            int has_length = cursor[0] == '#';
            if (has_length) {
                cursor += 1;
            }
            if (code == 's' || code == 'z') {
                const char *text = va_arg(*arguments, const char *);
                ptrdiff_t length;
                if (has_length) {
                    length = va_arg(*arguments, ptrdiff_t);
                    if (length < 0) {
                        hydro_capi_set_error(
                            "SystemError: negative string length passed to Py_BuildValue");
                        goto fail;
                    }
                }
                else if (text != NULL) {
                    length = (ptrdiff_t)strlen(text);
                }
                else {
                    length = 0;
                }
                if (text == NULL) {
                    if (length == 0) {
                        value = hydro_capi_none();
                    }
                    else {
                        hydro_capi_set_error(
                            "SystemError: NULL string with a positive length "
                            "passed to Py_BuildValue");
                        goto fail;
                    }
                }
                else {
                    value = PyUnicode_FromStringAndSize(text, length);
                }
            }
            else if (code == 'y') {
                const char *text = va_arg(*arguments, const char *);
                ptrdiff_t length;
                if (has_length) {
                    length = va_arg(*arguments, ptrdiff_t);
                    if (length < 0) {
                        hydro_capi_set_error(
                            "SystemError: negative bytes length passed to Py_BuildValue");
                        goto fail;
                    }
                }
                else if (text != NULL) {
                    length = (ptrdiff_t)strlen(text);
                }
                else {
                    length = 0;
                }
                if (text == NULL) {
                    if (length != 0) {
                        hydro_capi_set_error(
                            "SystemError: NULL bytes with a positive length "
                            "passed to Py_BuildValue");
                        goto fail;
                    }
                    value = hydro_capi_none();
                }
                else {
                    value = PyBytes_FromStringAndSize(text, length);
                }
            }
            else {
                const unsigned int *text =
                    va_arg(*arguments, const unsigned int *);
                ptrdiff_t length;
                ptrdiff_t index;
                char buffer[4];
                char *encoded;
                char *write;
                if (has_length) {
                    length = va_arg(*arguments, ptrdiff_t);
                    if (length < 0) {
                        hydro_capi_set_error(
                            "SystemError: negative UCS length passed to Py_BuildValue");
                        goto fail;
                    }
                }
                else if (text != NULL) {
                    length = 0;
                    while (text[length] != 0) {
                        length += 1;
                    }
                }
                else {
                    length = 0;
                }
                encoded = malloc((size_t)length * 4 + 1);
                if (encoded == NULL) {
                    hydro_capi_set_error(
                        "MemoryError: cannot allocate the UCS string");
                    goto fail;
                }
                write = encoded;
                for (index = 0; index < length; index++) {
                    int size = text != NULL
                        ? hydro_capi_utf8_encode(text[index], buffer) : 0;
                    if (size == 0) {
                        free(encoded);
                        hydro_capi_set_error(
                            "ValueError: invalid code point passed to Py_BuildValue 'u'");
                        goto fail;
                    }
                    memcpy(write, buffer, (size_t)size);
                    write += size;
                }
                value = PyUnicode_FromStringAndSize(encoded, write - encoded);
                free(encoded);
            }
        }
        else {
            switch (code) {
            case 'i':
                value = PyLong_FromLongLong(va_arg(*arguments, int));
                break;
            case 'I':
                value = PyLong_FromUnsignedLongLong(
                    va_arg(*arguments, unsigned int));
                break;
            case 'l':
                value = PyLong_FromLongLong(va_arg(*arguments, long));
                break;
            case 'k':
                value = PyLong_FromUnsignedLongLong(
                    va_arg(*arguments, unsigned long));
                break;
            case 'L':
                value = PyLong_FromLongLong(va_arg(*arguments, long long));
                break;
            case 'K':
                value = PyLong_FromUnsignedLongLong(
                    va_arg(*arguments, unsigned long long));
                break;
            case 'n':
                value = PyLong_FromLongLong(va_arg(*arguments, Py_ssize_t));
                break;
            case 'f':
            case 'd':
                value = PyFloat_FromDouble(va_arg(*arguments, double));
                break;
            case 'D': {
                hydro_capi_py_complex complex_value =
                    va_arg(*arguments, hydro_capi_py_complex);
                value = PyComplex_FromDoubles(complex_value.real,
                                              complex_value.imag);
                break;
            }
            case 'c': {
                char character = (char)va_arg(*arguments, int);
                value = PyUnicode_FromStringAndSize(&character, 1);
                break;
            }
            case 'C': {
                char buffer[4];
                int size = hydro_capi_utf8_encode(
                    (unsigned long)(unsigned int)va_arg(*arguments, int), buffer);
                if (size == 0) {
                    hydro_capi_set_error(
                        "ValueError: invalid code point passed to Py_BuildValue 'C'");
                    goto fail;
                }
                value = PyUnicode_FromStringAndSize(buffer, size);
                break;
            }
            case 'O':
            case 'N': {
                PyObject *object = va_arg(*arguments, PyObject *);
                if (object == NULL) {
                    hydro_capi_set_error(
                        "SystemError: NULL object passed to Py_BuildValue");
                    goto fail;
                }
                if (cursor[0] == '!') {
                    PyObject *type;
                    cursor += 1;
                    type = va_arg(*arguments, PyObject *);
                    if (type == NULL ||
                        !hydro_capi_bv_typecheck(object, type)) {
                        goto fail;
                    }
                }
                else if (cursor[0] == '&') {
                    hydro_capi_set_error(
                        "TypeError: Py_BuildValue: unsupported 'O&' conversion");
                    goto fail;
                }
                if (code == 'O') {
                    Py_IncRef(object);
                }
                value = object;
                break;
            }
            case 'S':
            case 'R':
            case 'A': {
                PyObject *object = va_arg(*arguments, PyObject *);
                if (object == NULL) {
                    hydro_capi_set_error(
                        "SystemError: NULL object passed to Py_BuildValue");
                    goto fail;
                }
                if (code == 'S') {
                    value = PyObject_Str(object);
                }
                else if (code == 'R') {
                    value = PyObject_Repr(object);
                }
                else {
                    value = hydro_capi_ascii_repr(object);
                }
                break;
            }
            case 'U': {
                PyObject *object = va_arg(*arguments, PyObject *);
                if (object == NULL) {
                    value = hydro_capi_none();
                }
                else {
                    Py_IncRef(object);
                    value = object;
                }
                break;
            }
            default: {
                if (code == '!') {
                    hydro_capi_set_error(
                        "SystemError: Py_BuildValue: '!' without 'O'");
                }
                else {
                    char message[160];
                    if (code >= 0x20 && code < 0x7f) {
                        snprintf(message, sizeof message,
                                 "TypeError: Py_BuildValue: unsupported format character '%c' (0x%02x)",
                                 code, (unsigned)(unsigned char)code);
                    }
                    else {
                        snprintf(message, sizeof message,
                                 "TypeError: Py_BuildValue: unsupported format character 0x%02x",
                                 (unsigned)(unsigned char)code);
                    }
                    hydro_capi_set_error(message);
                }
                goto fail;
            }
            }
        }
        if (value == NULL) {
            goto fail;
        }
        if (hydro_capi_bv_add(&state, value) != 0) {
            goto fail;
        }
    }
    if (state.depth != 0) {
        hydro_capi_set_error("SystemError: Py_BuildValue: unbalanced format");
        goto fail;
    }
    if (state.result == NULL) {
        state.result = hydro_capi_none();
        if (state.result == NULL) {
            goto fail;
        }
    }
    return state.result;

fail:
    hydro_capi_bv_release(&state);
    return NULL;
}

PyObject *Py_BuildValue(const char *format, ...)
{
    va_list arguments;
    PyObject *result;

    if (format == NULL) {
        return NULL;
    }
    va_start(arguments, format);
    result = hydro_capi_build_value_va(format, &arguments);
    va_end(arguments);
    return result;
}

/* `PyObject_CallFunction`: builds its arguments with Py_BuildValue. */
PyObject *PyObject_CallFunction(PyObject *callable, const char *format, ...)
{
    va_list arguments;
    PyObject *args = NULL;
    PyObject *result;

    if (callable == NULL) {
        hydro_capi_set_type_error("PyObject_CallFunction: callable is NULL");
        return NULL;
    }
    if (format != NULL && format[0] != '\0') {
        va_start(arguments, format);
        args = hydro_capi_build_value_va(format, &arguments);
        va_end(arguments);
        if (args == NULL) {
            return NULL;
        }
        if (!hydro_capi_is_tuple(args)) {
            PyObject *tuple = PyTuple_New(1);
            if (tuple == NULL) {
                Py_DecRef(args);
                return NULL;
            }
            if (PyTuple_SetItem(tuple, 0, args) != 0) {
                Py_DecRef(tuple);
                Py_DecRef(args);
                return NULL;
            }
            args = tuple;
        }
    }
    result = PyObject_CallObject(callable, args);
    if (args != NULL) {
        Py_DecRef(args);
    }
    return result;
}

/* `PyObject_CallMethod`: attribute lookup plus a positional call. */
PyObject *PyObject_CallMethod(PyObject *object, const char *name,
                              const char *format, ...)
{
    va_list arguments;
    PyObject *method;
    PyObject *args = NULL;
    PyObject *result;

    if (object == NULL || name == NULL) {
        hydro_capi_set_type_error("PyObject_CallMethod: object or name is NULL");
        return NULL;
    }
    method = PyObject_GetAttrString(object, name);
    if (method == NULL) {
        return NULL;
    }
    if (format != NULL && format[0] != '\0') {
        va_start(arguments, format);
        args = hydro_capi_build_value_va(format, &arguments);
        va_end(arguments);
        if (args == NULL) {
            Py_DecRef(method);
            return NULL;
        }
        if (!hydro_capi_is_tuple(args)) {
            PyObject *tuple = PyTuple_New(1);
            if (tuple == NULL) {
                Py_DecRef(args);
                Py_DecRef(method);
                return NULL;
            }
            if (PyTuple_SetItem(tuple, 0, args) != 0) {
                Py_DecRef(tuple);
                Py_DecRef(args);
                Py_DecRef(method);
                return NULL;
            }
            args = tuple;
        }
    }
    result = PyObject_CallObject(method, args);
    if (args != NULL) {
        Py_DecRef(args);
    }
    Py_DecRef(method);
    return result;
}

/* `PyTuple_Pack`: a new tuple of borrowed references. */
PyObject *PyTuple_Pack(Py_ssize_t count, ...)
{
    va_list arguments;
    PyObject **items;
    PyObject *result;
    Py_ssize_t index;

    if (count < 0) {
        hydro_capi_set_error("SystemError: negative PyTuple_Pack size");
        return NULL;
    }
    items = count > 0 ? malloc((size_t)count * sizeof(PyObject *)) : NULL;
    if (count > 0 && items == NULL) {
        hydro_capi_set_error("MemoryError: cannot allocate the PyTuple_Pack vector");
        return NULL;
    }
    va_start(arguments, count);
    for (index = 0; index < count; index++) {
        items[index] = va_arg(arguments, PyObject *);
    }
    va_end(arguments);
    result = hydro_capi_tuple_pack(count, items);
    free(items);
    return result;
}

/*
 * ---------------------------------------------------------------------------
 * PyUnicode_FromFormat / PyErr_WarnFormat and the PyOS helpers.
 *
 * The formatter supports the CPython conversions used by extensions: integer,
 * character, string, pointer and object conversions (%A %U %V %S %R %T),
 * including flags, width, precision and the l/ll/z/t/j/h length modifiers.
 * Unsupported conversions report a ValueError.
 * ---------------------------------------------------------------------------
 */

extern PyObject *PyType_GetQualName(PyObject *);
extern PyObject *hydro_capi_ascii_repr(PyObject *);
extern int PyErr_WarnEx(PyObject *category, const char *message,
                        Py_ssize_t stack_level);

typedef struct {
    char stack[256];
    char *data;
    size_t length;
    size_t capacity;
    int heap;
} hydro_capi_fmt_buffer;

static void hydro_capi_fmt_init(hydro_capi_fmt_buffer *buffer)
{
    buffer->data = buffer->stack;
    buffer->length = 0;
    buffer->capacity = sizeof buffer->stack;
    buffer->heap = 0;
}

static void hydro_capi_fmt_dispose(hydro_capi_fmt_buffer *buffer)
{
    if (buffer->heap) {
        free(buffer->data);
    }
    buffer->data = buffer->stack;
    buffer->length = 0;
    buffer->capacity = sizeof buffer->stack;
    buffer->heap = 0;
}

static int hydro_capi_fmt_reserve(hydro_capi_fmt_buffer *buffer, size_t extra)
{
    size_t needed;
    size_t capacity;
    char *data;

    if (extra <= buffer->capacity - buffer->length) {
        return 0;
    }
    needed = buffer->length + extra;
    capacity = buffer->capacity;
    while (capacity < needed) {
        if (capacity > ((size_t)-1) / 2) {
            hydro_capi_set_error("MemoryError: formatted string is too large");
            return -1;
        }
        capacity *= 2;
    }
    if (buffer->heap) {
        data = realloc(buffer->data, capacity);
    }
    else {
        data = malloc(capacity);
        if (data != NULL) {
            memcpy(data, buffer->data, buffer->length);
        }
    }
    if (data == NULL) {
        hydro_capi_set_error("MemoryError: cannot grow the formatted string");
        return -1;
    }
    buffer->data = data;
    buffer->capacity = capacity;
    buffer->heap = 1;
    return 0;
}

static int hydro_capi_fmt_append(hydro_capi_fmt_buffer *buffer, const char *text,
                                 size_t length)
{
    if (hydro_capi_fmt_reserve(buffer, length) != 0) {
        return -1;
    }
    memcpy(buffer->data + buffer->length, text, length);
    buffer->length += length;
    return 0;
}

static int hydro_capi_fmt_append_char(hydro_capi_fmt_buffer *buffer, char code)
{
    return hydro_capi_fmt_append(buffer, &code, 1);
}

/* Appends one snprintf-formatted value, growing the buffer when needed. */
static int hydro_capi_fmt_append_snprintf(hydro_capi_fmt_buffer *buffer,
                                          const char *spec, ...)
{
    va_list arguments;
    char local[256];
    char *target = local;
    int needed;
    int status;

    va_start(arguments, spec);
    needed = vsnprintf(local, sizeof local, spec, arguments);
    va_end(arguments);
    if (needed < 0) {
        hydro_capi_set_error("ValueError: invalid format string");
        return -1;
    }
    if ((size_t)needed >= sizeof local) {
        target = malloc((size_t)needed + 1);
        if (target == NULL) {
            hydro_capi_set_error("MemoryError: cannot format the value");
            return -1;
        }
        va_start(arguments, spec);
        (void)vsnprintf(target, (size_t)needed + 1, spec, arguments);
        va_end(arguments);
    }
    status = hydro_capi_fmt_append(buffer, target, (size_t)needed);
    if (target != local) {
        free(target);
    }
    return status;
}

/* Appends a rendered Python string, honouring an optional precision. */
static int hydro_capi_fmt_append_object(hydro_capi_fmt_buffer *buffer,
                                        PyObject *object, int precision)
{
    const char *utf8;
    size_t length;

    if (object == NULL) {
        return -1;
    }
    utf8 = PyUnicode_AsUTF8(object);
    if (utf8 == NULL) {
        Py_DecRef(object);
        hydro_capi_set_error(
            "SystemError: PyUnicode_FromFormat: object conversion failed");
        return -1;
    }
    length = strlen(utf8);
    if (precision >= 0 && (size_t)precision < length) {
        length = (size_t)precision;
    }
    if (hydro_capi_fmt_append(buffer, utf8, length) != 0) {
        Py_DecRef(object);
        return -1;
    }
    Py_DecRef(object);
    return 0;
}

static PyObject *hydro_capi_unicode_from_format_va(const char *format,
                                                   va_list *arguments)
{
    hydro_capi_fmt_buffer buffer;
    const char *cursor;

    if (format == NULL) {
        hydro_capi_set_error("SystemError: PyUnicode_FromFormat: format is NULL");
        return NULL;
    }
    hydro_capi_fmt_init(&buffer);
    cursor = format;
    while (*cursor != '\0') {
        const char *literal = cursor;
        size_t literal_length;
        char spec[96];
        size_t spec_length = 0;
        int width = -1;
        int precision = -1;
        int length_kind = 0; /* 0 int, 1 long, 2 long long, 3 ssize, 4 ptrdiff, 5 intmax */
        char conversion;
        int had_left_flag = 0;

        while (*cursor != '\0' && *cursor != '%') {
            cursor += 1;
        }
        literal_length = (size_t)(cursor - literal);
        if (literal_length != 0 &&
            hydro_capi_fmt_append(&buffer, literal, literal_length) != 0) {
            goto fail;
        }
        if (*cursor == '\0') {
            break;
        }
        cursor += 1; /* '%' */
        if (*cursor == '%') {
            if (hydro_capi_fmt_append_char(&buffer, '%') != 0) {
                goto fail;
            }
            cursor += 1;
            continue;
        }
        spec[spec_length++] = '%';
        while (*cursor == '-' || *cursor == '+' || *cursor == ' ' ||
               *cursor == '#' || *cursor == '0') {
            if (*cursor == '-') {
                had_left_flag = 1;
            }
            spec[spec_length++] = *cursor++;
        }
        if (*cursor == '*') {
            width = va_arg(*arguments, int);
            cursor += 1;
            if (width < 0) {
                if (!had_left_flag) {
                    spec[spec_length++] = '-';
                }
                width = -width;
            }
            spec_length += (size_t)snprintf(spec + spec_length,
                                            sizeof spec - spec_length,
                                            "%d", width);
        }
        else {
            while (*cursor >= '0' && *cursor <= '9') {
                spec[spec_length++] = *cursor++;
            }
        }
        if (*cursor == '.') {
            spec[spec_length++] = '.';
            cursor += 1;
            if (*cursor == '*') {
                precision = va_arg(*arguments, int);
                cursor += 1;
                if (precision < 0) {
                    precision = -1;
                }
                else {
                    spec_length += (size_t)snprintf(spec + spec_length,
                                                    sizeof spec - spec_length,
                                                    "%d", precision);
                }
            }
            else {
                const char *digits = cursor;
                while (*cursor >= '0' && *cursor <= '9') {
                    spec[spec_length++] = *cursor++;
                }
                if (cursor != digits) {
                    precision = atoi(digits);
                }
            }
        }
        if (cursor[0] == 'l' && cursor[1] == 'l') {
            length_kind = 2;
            cursor += 2;
        }
        else if (*cursor == 'l') {
            length_kind = 1;
            cursor += 1;
        }
        else if (*cursor == 'z') {
            length_kind = 3;
            cursor += 1;
        }
        else if (*cursor == 't') {
            length_kind = 4;
            cursor += 1;
        }
        else if (*cursor == 'j') {
            length_kind = 5;
            cursor += 1;
        }
        else if (*cursor == 'h') {
            length_kind = 0;
            cursor += 1;
            if (*cursor == 'h') {
                cursor += 1;
            }
        }
        conversion = *cursor;
        if (conversion == '\0') {
            hydro_capi_set_error(
                "ValueError: PyUnicode_FromFormat: format string ends after '%'");
            goto fail;
        }
        cursor += 1;
        if (conversion == 'd' || conversion == 'i' || conversion == 'u' ||
            conversion == 'o' || conversion == 'x' || conversion == 'X') {
            long long signed_value = 0;
            unsigned long long unsigned_value = 0;
            int is_signed = conversion == 'd' || conversion == 'i';
            switch (length_kind) {
            case 0:
                if (is_signed) {
                    signed_value = va_arg(*arguments, int);
                }
                else {
                    unsigned_value = va_arg(*arguments, unsigned int);
                }
                break;
            case 1:
                if (is_signed) {
                    signed_value = va_arg(*arguments, long);
                }
                else {
                    unsigned_value = va_arg(*arguments, unsigned long);
                }
                break;
            case 2:
            case 5:
                if (is_signed) {
                    signed_value = va_arg(*arguments, long long);
                }
                else {
                    unsigned_value = va_arg(*arguments, unsigned long long);
                }
                break;
            default:
                if (is_signed) {
                    signed_value = va_arg(*arguments, Py_ssize_t);
                }
                else {
                    unsigned_value = (unsigned long long)va_arg(
                        *arguments, Py_ssize_t);
                }
                break;
            }
            spec[spec_length++] = 'l';
            spec[spec_length++] = 'l';
            spec[spec_length++] = conversion;
            spec[spec_length] = '\0';
            if (is_signed) {
                if (hydro_capi_fmt_append_snprintf(&buffer, spec, signed_value) != 0) {
                    goto fail;
                }
            }
            else if (hydro_capi_fmt_append_snprintf(&buffer, spec,
                                                    unsigned_value) != 0) {
                goto fail;
            }
        }
        else if (conversion == 'c') {
            int character = va_arg(*arguments, int);
            spec[spec_length++] = 'c';
            spec[spec_length] = '\0';
            if (hydro_capi_fmt_append_snprintf(&buffer, spec, character) != 0) {
                goto fail;
            }
        }
        else if (conversion == 's') {
            const char *text = va_arg(*arguments, const char *);
            spec[spec_length++] = 's';
            spec[spec_length] = '\0';
            if (text == NULL) {
                text = "(null)";
            }
            if (hydro_capi_fmt_append_snprintf(&buffer, spec, text) != 0) {
                goto fail;
            }
        }
        else if (conversion == 'p') {
            void *pointer = va_arg(*arguments, void *);
            spec[spec_length++] = 'p';
            spec[spec_length] = '\0';
            if (hydro_capi_fmt_append_snprintf(&buffer, spec, pointer) != 0) {
                goto fail;
            }
        }
        else if (conversion == 'f' || conversion == 'F' || conversion == 'e' ||
                 conversion == 'E' || conversion == 'g' || conversion == 'G') {
            double number = va_arg(*arguments, double);
            spec[spec_length++] = conversion;
            spec[spec_length] = '\0';
            if (hydro_capi_fmt_append_snprintf(&buffer, spec, number) != 0) {
                goto fail;
            }
        }
        else {
            PyObject *rendered = NULL;
            if (conversion == 'V') {
                PyObject *object = va_arg(*arguments, PyObject *);
                const char *fallback = va_arg(*arguments, const char *);
                if (object == NULL) {
                    const char *text = fallback != NULL ? fallback : "(null)";
                    if (hydro_capi_fmt_append(&buffer, text, strlen(text)) != 0) {
                        goto fail;
                    }
                    continue;
                }
                rendered = PyObject_Str(object);
            }
            else if (conversion == 'A') {
                rendered = hydro_capi_ascii_repr(va_arg(*arguments, PyObject *));
            }
            else if (conversion == 'U' || conversion == 'S') {
                rendered = PyObject_Str(va_arg(*arguments, PyObject *));
            }
            else if (conversion == 'R') {
                rendered = PyObject_Repr(va_arg(*arguments, PyObject *));
            }
            else if (conversion == 'T') {
                PyObject *type = va_arg(*arguments, PyObject *);
                if (type == NULL) {
                    hydro_capi_set_error(
                        "SystemError: PyUnicode_FromFormat: %T needs a type");
                    goto fail;
                }
                rendered = PyType_GetQualName(type);
            }
            else {
                char message[160];
                snprintf(message, sizeof message,
                         "ValueError: PyUnicode_FromFormat: unsupported format character '%c'",
                         conversion);
                hydro_capi_set_error(message);
                goto fail;
            }
            if (hydro_capi_fmt_append_object(&buffer, rendered, precision) != 0) {
                goto fail;
            }
        }
    }
    {
        PyObject *result =
            PyUnicode_FromStringAndSize(buffer.data, (Py_ssize_t)buffer.length);
        hydro_capi_fmt_dispose(&buffer);
        return result;
    }

fail:
    hydro_capi_fmt_dispose(&buffer);
    return NULL;
}

PyObject *PyUnicode_FromFormatV(const char *format, va_list vargs)
{
    va_list copy;
    PyObject *result;

    va_copy(copy, vargs);
    result = hydro_capi_unicode_from_format_va(format, &copy);
    va_end(copy);
    return result;
}

PyObject *PyUnicode_FromFormat(const char *format, ...)
{
    va_list arguments;
    PyObject *result;

    va_start(arguments, format);
    result = hydro_capi_unicode_from_format_va(format, &arguments);
    va_end(arguments);
    return result;
}

/* `PyErr_WarnFormat`: formats a warning message and emits it. */
int PyErr_WarnFormat(PyObject *category, Py_ssize_t stack_level,
                     const char *format, ...)
{
    va_list arguments;
    PyObject *message;
    const char *text;
    int status;

    if (format == NULL) {
        hydro_capi_set_type_error("PyErr_WarnFormat: format is NULL");
        return -1;
    }
    va_start(arguments, format);
    message = hydro_capi_unicode_from_format_va(format, &arguments);
    va_end(arguments);
    if (message == NULL) {
        return -1;
    }
    text = PyUnicode_AsUTF8(message);
    if (text == NULL) {
        Py_DecRef(message);
        return -1;
    }
    status = PyErr_WarnEx(category, text, stack_level);
    Py_DecRef(message);
    return status;
}

/* ---- PyOS helpers ------------------------------------------------------- */

int PyOS_snprintf(char *buffer, size_t size, const char *format, ...)
{
    va_list arguments;
    int result;

    if (format == NULL || (buffer == NULL && size != 0)) {
        hydro_capi_set_error("SystemError: PyOS_snprintf: invalid arguments");
        return -1;
    }
    va_start(arguments, format);
    result = vsnprintf(buffer, size, format, arguments);
    va_end(arguments);
    return result;
}

double PyOS_string_to_double(const char *text, char **end, PyObject *overflow)
{
    char *local_end;
    double value;

    if (text == NULL) {
        hydro_capi_set_error(
            "ValueError: PyOS_string_to_double: text is NULL");
        return -1.0;
    }
    errno = 0;
    value = strtod(text, &local_end);
    if (end != NULL) {
        *end = local_end;
    }
    if (local_end == text) {
        char message[256];
        snprintf(message, sizeof message,
                 "ValueError: could not convert string to float: '%s'", text);
        hydro_capi_set_error(message);
        return -1.0;
    }
    if (errno == ERANGE) {
        if (value == 0.0) {
            return 0.0; /* underflow: CPython returns zero silently */
        }
        if (overflow != NULL) {
            hydro_capi_set_exception_message(
                overflow, "value too large to convert to float");
        }
        else {
            hydro_capi_set_error(
                "OverflowError: value too large to convert to float");
        }
        return -1.0;
    }
    return value;
}

long PyOS_strtol(const char *text, char **end, int base)
{
    return strtol(text, end, base);
}

unsigned long PyOS_strtoul(const char *text, char **end, int base)
{
    return strtoul(text, end, base);
}
