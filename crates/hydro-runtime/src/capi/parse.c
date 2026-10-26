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

typedef struct _hydro_shim_object PyObject;

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

int PyArg_ParseTupleAndKeywords(PyObject *args, PyObject *keywords,
                                const char *format, char *const *names, ...)
{
    va_list arguments;
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

    va_start(arguments, names);
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

        if (!hydro_capi_convert(code, item, &arguments, function)) {
            status = 0;
            break;
        }
    }
    va_end(arguments);

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
