/*
 * The only variadic CPython entry point used by limited-API extensions.
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
 * optional argument is missing. `:` and `;` end the format (the trailing text
 * is ignored). On any failure the Rust side records a thread-local error and
 * this function returns 0, exactly like CPython.
 */

#include <stdarg.h>
#include <stddef.h>

typedef struct _hydro_shim_object PyObject;

/*
 * Py_ssize_t is `long` on the supported x86_64 (LP64) target and therefore
 * representation-compatible with `ptrdiff_t`; the Rust accessor writes an
 * `isize`.
 */
extern size_t hydro_capi_arg_count(PyObject *args);
extern PyObject *hydro_capi_arg(PyObject *args, size_t index);
extern int hydro_capi_parse_i(PyObject *object, int *out);
extern int hydro_capi_parse_l(PyObject *object, long *out);
extern int hydro_capi_parse_n(PyObject *object, ptrdiff_t *out);
extern int hydro_capi_parse_d(PyObject *object, double *out);
extern int hydro_capi_parse_s(PyObject *object, const char **out);
extern int hydro_capi_parse_o(PyObject *object, PyObject **out);
extern int hydro_capi_parse_p(PyObject *object, int *out);
extern void hydro_capi_set_error(const char *message);
extern void hydro_capi_set_type_error(const char *message);

static int hydro_capi_is_space(char code)
{
    return code == ' ' || code == '\t' || code == '\n' || code == '\r' ||
           code == '\f' || code == '\v';
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

        switch (code) {
        case 'i': {
            int *out = va_arg(arguments, int *);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 'i'");
                status = 0;
            } else {
                status = hydro_capi_parse_i(item, out);
            }
            break;
        }
        case 'l': {
            long *out = va_arg(arguments, long *);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 'l'");
                status = 0;
            } else {
                status = hydro_capi_parse_l(item, out);
            }
            break;
        }
        case 'n': {
            ptrdiff_t *out = va_arg(arguments, ptrdiff_t *);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 'n'");
                status = 0;
            } else {
                status = hydro_capi_parse_n(item, out);
            }
            break;
        }
        case 'd': {
            double *out = va_arg(arguments, double *);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 'd'");
                status = 0;
            } else {
                status = hydro_capi_parse_d(item, out);
            }
            break;
        }
        case 's': {
            const char **out = va_arg(arguments, const char **);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 's'");
                status = 0;
            } else {
                status = hydro_capi_parse_s(item, out);
            }
            break;
        }
        case 'O': {
            PyObject **out = va_arg(arguments, PyObject **);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 'O'");
                status = 0;
            } else {
                status = hydro_capi_parse_o(item, out);
            }
            break;
        }
        case 'p': {
            int *out = va_arg(arguments, int *);
            if (out == NULL) {
                hydro_capi_set_type_error("PyArg_ParseTuple: NULL output pointer for 'p'");
                status = 0;
            } else {
                status = hydro_capi_parse_p(item, out);
            }
            break;
        }
        default:
            hydro_capi_set_type_error("PyArg_ParseTuple: unsupported format character");
            status = 0;
            break;
        }

        if (!status) {
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
