/*
 * CPython-inline-macro layout probe.
 *
 * Compiled at test time by `tests/tuple_layout.rs` and loaded through the
 * runtime extension bridge. It is deliberately built *without*
 * `Py_LIMITED_API`, so the CPython headers compile the inline macros
 * (`PyTuple_GET_SIZE`, `PyTuple_GET_ITEM`, `PyTuple_SET_ITEM`,
 * `PyList_GET_SIZE`, `PyList_GET_ITEM`, `PyList_SET_ITEM`) into the extension.
 * That is the same memory access pyo3-ffi performs for PyO3 extensions such as
 * pydantic-core, so these probes reproduce the pydantic `SchemaValidator`
 * argument-count failure without needing the wheel.
 *
 * The probes verify the observation inside the extension and return `True`;
 * on a mismatch they raise `AssertionError` with the observed numbers, so a
 * layout regression is reported here with the exact bad value instead of
 * being hidden by the Hydro return-value conversion.
 *
 * Probes:
 *   tuple_probe2(tuple, a, b)         two-item tuple
 *   tuple_probe1(tuple, a)            one-item tuple (the SchemaValidator shape)
 *   tuple_probe0(tuple)               empty tuple
 *   list_probe(list, first, extra)    read, append through `PyList_Append`
 *   make_tuple(a, b)                  built through `PyTuple_SET_ITEM`
 *   make_list(a, b)                   built through `PyList_SET_ITEM`
 *
 * Every probe also reads `PyTuple_GET_SIZE(args)` of its own argument tuple,
 * matching the PyO3 code path that produced the invalid argument count.
 */
#include <Python.h>

static PyObject *fail(const char *what, long expected, long actual) {
    PyErr_Format(PyExc_AssertionError, "%s: expected %ld, observed %ld", what, expected, actual);
    return NULL;
}

static PyObject *tuple_probe2(PyObject *self, PyObject *args) {
    PyObject *tuple;
    long a;
    long b;
    long seen;
    (void)self;

    if (!PyArg_ParseTuple(args, "Oll", &tuple, &a, &b)) {
        return NULL;
    }
    if (!PyTuple_Check(tuple)) {
        PyErr_SetString(PyExc_TypeError, "tuple_probe2 requires a tuple");
        return NULL;
    }
    if (PyTuple_GET_SIZE(args) != 3) {
        return fail("PyTuple_GET_SIZE(args)", 3, (long)PyTuple_GET_SIZE(args));
    }
    if (PyTuple_GET_SIZE(tuple) != 2) {
        return fail("PyTuple_GET_SIZE(tuple)", 2, (long)PyTuple_GET_SIZE(tuple));
    }
    seen = PyLong_AsLong(PyTuple_GET_ITEM(tuple, 0));
    if (seen == -1 && PyErr_Occurred()) {
        return NULL;
    }
    if (seen != a) {
        return fail("PyTuple_GET_ITEM(tuple, 0)", a, seen);
    }
    seen = PyLong_AsLong(PyTuple_GET_ITEM(tuple, 1));
    if (seen == -1 && PyErr_Occurred()) {
        return NULL;
    }
    if (seen != b) {
        return fail("PyTuple_GET_ITEM(tuple, 1)", b, seen);
    }
    Py_RETURN_TRUE;
}

static PyObject *tuple_probe1(PyObject *self, PyObject *args) {
    PyObject *tuple;
    long a;
    long seen;
    (void)self;

    if (!PyArg_ParseTuple(args, "Ol", &tuple, &a)) {
        return NULL;
    }
    if (!PyTuple_Check(tuple)) {
        PyErr_SetString(PyExc_TypeError, "tuple_probe1 requires a tuple");
        return NULL;
    }
    if (PyTuple_GET_SIZE(args) != 2) {
        return fail("PyTuple_GET_SIZE(args)", 2, (long)PyTuple_GET_SIZE(args));
    }
    if (PyTuple_GET_SIZE(tuple) != 1) {
        return fail("PyTuple_GET_SIZE(tuple)", 1, (long)PyTuple_GET_SIZE(tuple));
    }
    seen = PyLong_AsLong(PyTuple_GET_ITEM(tuple, 0));
    if (seen == -1 && PyErr_Occurred()) {
        return NULL;
    }
    if (seen != a) {
        return fail("PyTuple_GET_ITEM(tuple, 0)", a, seen);
    }
    Py_RETURN_TRUE;
}

static PyObject *tuple_probe0(PyObject *self, PyObject *args) {
    PyObject *tuple;
    (void)self;

    if (!PyArg_ParseTuple(args, "O", &tuple)) {
        return NULL;
    }
    if (!PyTuple_Check(tuple)) {
        PyErr_SetString(PyExc_TypeError, "tuple_probe0 requires a tuple");
        return NULL;
    }
    if (PyTuple_GET_SIZE(args) != 1) {
        return fail("PyTuple_GET_SIZE(args)", 1, (long)PyTuple_GET_SIZE(args));
    }
    if (PyTuple_GET_SIZE(tuple) != 0) {
        return fail("PyTuple_GET_SIZE(tuple)", 0, (long)PyTuple_GET_SIZE(tuple));
    }
    Py_RETURN_TRUE;
}

static PyObject *list_probe(PyObject *self, PyObject *args) {
    PyObject *list;
    PyObject *extra;
    long first;
    long seen;
    (void)self;

    if (!PyArg_ParseTuple(args, "OOl", &list, &extra, &first)) {
        return NULL;
    }
    if (!PyList_Check(list)) {
        PyErr_SetString(PyExc_TypeError, "list_probe requires a list");
        return NULL;
    }
    if (PyTuple_GET_SIZE(args) != 3) {
        return fail("PyTuple_GET_SIZE(args)", 3, (long)PyTuple_GET_SIZE(args));
    }
    if (PyList_GET_SIZE(list) != 2) {
        return fail("PyList_GET_SIZE(list)", 2, (long)PyList_GET_SIZE(list));
    }
    seen = PyLong_AsLong(PyList_GET_ITEM(list, 0));
    if (seen == -1 && PyErr_Occurred()) {
        return NULL;
    }
    if (seen != first) {
        return fail("PyList_GET_ITEM(list, 0)", first, seen);
    }
    if (PyList_Append(list, extra) != 0) {
        return NULL;
    }
    if (PyList_GET_SIZE(list) != 3) {
        return fail("PyList_GET_SIZE(list) after append", 3, (long)PyList_GET_SIZE(list));
    }
    seen = PyLong_AsLong(PyList_GET_ITEM(list, 2));
    if (seen == -1 && PyErr_Occurred()) {
        return NULL;
    }
    if (seen != PyLong_AsLong(extra)) {
        return fail("PyList_GET_ITEM(list, -1) after append", PyLong_AsLong(extra), seen);
    }
    Py_RETURN_TRUE;
}

static PyObject *make_tuple(PyObject *self, PyObject *args) {
    long a;
    long b;
    PyObject *result;
    PyObject *first;
    PyObject *second;
    (void)self;

    if (!PyArg_ParseTuple(args, "ll", &a, &b)) {
        return NULL;
    }
    result = PyTuple_New(2);
    if (result == NULL) {
        return NULL;
    }
    first = PyLong_FromLong(a);
    second = PyLong_FromLong(b);
    if (first == NULL || second == NULL) {
        Py_XDECREF(first);
        Py_XDECREF(second);
        Py_DECREF(result);
        return NULL;
    }
    /* Both macros steal the reference. */
    PyTuple_SET_ITEM(result, 0, first);
    PyTuple_SET_ITEM(result, 1, second);
    return result;
}

static PyObject *make_list(PyObject *self, PyObject *args) {
    long a;
    long b;
    PyObject *result;
    PyObject *first;
    PyObject *second;
    (void)self;

    if (!PyArg_ParseTuple(args, "ll", &a, &b)) {
        return NULL;
    }
    result = PyList_New(2);
    if (result == NULL) {
        return NULL;
    }
    first = PyLong_FromLong(a);
    second = PyLong_FromLong(b);
    if (first == NULL || second == NULL) {
        Py_XDECREF(first);
        Py_XDECREF(second);
        Py_DECREF(result);
        return NULL;
    }
    /* Both macros steal the reference. The fixture is built with -DNDEBUG so
     * the debug-only `index < allocated` assertion in `PyList_SET_ITEM` does
     * not fire (this host does not mirror `PyListObject.allocated`; see the
     * `tuple_layout` module documentation). */
    PyList_SET_ITEM(result, 0, first);
    PyList_SET_ITEM(result, 1, second);
    return result;
}

static PyMethodDef module_methods[] = {
    {"tuple_probe2", tuple_probe2, METH_VARARGS, "verify a two-item tuple inline"},
    {"tuple_probe1", tuple_probe1, METH_VARARGS, "verify a one-item tuple inline"},
    {"tuple_probe0", tuple_probe0, METH_VARARGS, "verify an empty tuple inline"},
    {"list_probe", list_probe, METH_VARARGS, "read and grow a list inline"},
    {"make_tuple", make_tuple, METH_VARARGS, "build a tuple through PyTuple_SET_ITEM"},
    {"make_list", make_list, METH_VARARGS, "build a list through PyList_SET_ITEM"},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef module_definition = {
    PyModuleDef_HEAD_INIT,
    "tuple_layout_probe",
    "CPython inline tuple/list layout probe",
    -1,
    module_methods,
    NULL,
    NULL,
    NULL,
    NULL,
};

PyMODINIT_FUNC PyInit_tuple_layout_probe(void) {
    return PyModule_Create(&module_definition);
}
