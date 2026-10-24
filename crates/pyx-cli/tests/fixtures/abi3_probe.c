#define Py_LIMITED_API 0x030C0000

#include <stdlib.h>
#include <string.h>

#include <Python.h>

/*
 * With Py_LIMITED_API the installed CPython headers (3.14 at the time of
 * writing) no longer declare PyUnicode_AsUTF8 in the non-CPython header, even
 * though the symbol is exported by the host. Declare the stable-ABI entry
 * point explicitly so the compiler does not assume an `int` return value.
 */
extern const char *PyUnicode_AsUTF8(PyObject *);

static PyObject *pyx_capi_add(PyObject *self, PyObject *args) {
    int a;
    int b;

    (void)self;
    if (!PyArg_ParseTuple(args, "ii:add", &a, &b)) {
        return NULL;
    }
    return PyLong_FromLong((long)a + (long)b);
}

static PyObject *pyx_capi_sum_list(PyObject *self, PyObject *args) {
    PyObject *list;
    Py_ssize_t size;
    Py_ssize_t index;
    long total = 0;

    (void)self;
    if (!PyArg_ParseTuple(args, "O:sum_list", &list)) {
        return NULL;
    }
    size = PyList_Size(list);
    if (size < 0) {
        return NULL;
    }
    for (index = 0; index < size; index++) {
        PyObject *item = PyList_GetItem(list, index);
        long value;

        if (item == NULL) {
            return NULL;
        }
        value = PyLong_AsLong(item);
        if (value == -1 && PyErr_Occurred() != NULL) {
            return NULL;
        }
        total += value;
    }
    return PyLong_FromLong(total);
}

static PyObject *pyx_capi_make_list(PyObject *self, PyObject *argument) {
    long count = PyLong_AsLong(argument);
    PyObject *list;
    long index;

    (void)self;
    if (count == -1 && PyErr_Occurred() != NULL) {
        return NULL;
    }
    /* A negative count is rejected by PyList_New, which sets the error. */
    list = PyList_New((Py_ssize_t)count);
    if (list == NULL) {
        return NULL;
    }
    for (index = 0; index < count; index++) {
        PyObject *square = PyLong_FromLong(index * index);

        if (square == NULL) {
            Py_DECREF(list);
            return NULL;
        }
        if (PyList_SetItem(list, (Py_ssize_t)index, square) < 0) {
            Py_DECREF(square);
            Py_DECREF(list);
            return NULL;
        }
    }
    return list;
}

static PyObject *pyx_capi_make_tuple(PyObject *self, PyObject *args) {
    int a;
    int b;
    PyObject *tuple;
    PyObject *first;
    PyObject *second;

    (void)self;
    if (!PyArg_ParseTuple(args, "ii:make_tuple", &a, &b)) {
        return NULL;
    }
    tuple = PyTuple_New(2);
    if (tuple == NULL) {
        return NULL;
    }
    first = PyLong_FromLong(a);
    if (first == NULL) {
        Py_DECREF(tuple);
        return NULL;
    }
    if (PyTuple_SetItem(tuple, 0, first) < 0) {
        Py_DECREF(first);
        Py_DECREF(tuple);
        return NULL;
    }
    second = PyLong_FromLong(b);
    if (second == NULL) {
        Py_DECREF(tuple);
        return NULL;
    }
    if (PyTuple_SetItem(tuple, 1, second) < 0) {
        Py_DECREF(second);
        Py_DECREF(tuple);
        return NULL;
    }
    if (PyTuple_Size(tuple) != 2) {
        Py_DECREF(tuple);
        return NULL;
    }
    {
        PyObject *check = PyTuple_GetItem(tuple, 0);

        if (check == NULL || PyLong_AsLong(check) != (long)a) {
            Py_DECREF(tuple);
            return NULL;
        }
    }
    return tuple;
}

static PyObject *pyx_capi_make_dict(PyObject *self, PyObject *unused) {
    PyObject *dict;
    PyObject *key;

    (void)self;
    (void)unused;
    dict = PyDict_New();
    if (dict == NULL) {
        return NULL;
    }
    if (PyDict_SetItemString(dict, "a", PyLong_FromLong(1)) < 0) {
        Py_DECREF(dict);
        return NULL;
    }
    if (PyDict_SetItemString(dict, "b", PyUnicode_FromString("two")) < 0) {
        Py_DECREF(dict);
        return NULL;
    }
    key = PyUnicode_FromString("n");
    if (key == NULL) {
        Py_DECREF(dict);
        return NULL;
    }
    if (PyDict_SetItem(dict, key, PyLong_FromLong(3)) < 0) {
        Py_DECREF(key);
        Py_DECREF(dict);
        return NULL;
    }
    /* Exercise both lookup forms; a missing key would return NULL. */
    if (PyDict_GetItemString(dict, "a") == NULL || PyDict_GetItem(dict, key) == NULL) {
        Py_DECREF(key);
        Py_DECREF(dict);
        return NULL;
    }
    Py_DECREF(key);
    return dict;
}

static PyObject *pyx_capi_dict_get(PyObject *self, PyObject *args) {
    PyObject *dict;
    const char *key;
    PyObject *value;

    (void)self;
    if (!PyArg_ParseTuple(args, "Os:dict_get", &dict, &key)) {
        return NULL;
    }
    value = PyDict_GetItemString(dict, key);
    if (value == NULL) {
        return NULL;
    }
    Py_INCREF(value);
    return value;
}

static PyObject *pyx_capi_upper(PyObject *self, PyObject *argument) {
    const char *text = PyUnicode_AsUTF8(argument);
    size_t length;
    size_t index;
    char *buffer;
    PyObject *result;

    (void)self;
    if (text == NULL) {
        return NULL;
    }
    length = strlen(text);
    buffer = (char *)malloc(length + 1);
    if (buffer == NULL) {
        return NULL;
    }
    for (index = 0; index < length; index++) {
        char byte = text[index];

        if (byte >= 'a' && byte <= 'z') {
            byte = (char)(byte - ('a' - 'A'));
        }
        buffer[index] = byte;
    }
    buffer[length] = '\0';
    result = PyUnicode_FromString(buffer);
    free(buffer);
    return result;
}

static PyObject *pyx_capi_half(PyObject *self, PyObject *argument) {
    double value = PyFloat_AsDouble(argument);

    (void)self;
    if (value == -1.0 && PyErr_Occurred() != NULL) {
        return NULL;
    }
    return PyFloat_FromDouble(value / 2.0);
}

static PyObject *pyx_capi_reverse_bytes(PyObject *self, PyObject *argument) {
    char *data = PyBytes_AsString(argument);
    Py_ssize_t size;
    Py_ssize_t index;
    char *buffer;
    PyObject *result;

    (void)self;
    if (data == NULL) {
        return NULL;
    }
    size = PyBytes_Size(argument);
    if (size < 0) {
        return NULL;
    }
    buffer = (char *)malloc((size_t)size);
    if (buffer == NULL && size > 0) {
        return NULL;
    }
    for (index = 0; index < size; index++) {
        buffer[index] = data[size - index - 1];
    }
    result = PyBytes_FromStringAndSize(buffer, size);
    free(buffer);
    return result;
}

static PyMethodDef pyx_capi_methods[] = {
    {
        "add",
        pyx_capi_add,
        METH_VARARGS,
        "add(a, b) -> int\n\nReturn the sum of two integers.",
    },
    {
        "sum_list",
        pyx_capi_sum_list,
        METH_VARARGS,
        "sum_list(xs) -> int\n\nSum the integers in a list.",
    },
    {
        "make_list",
        pyx_capi_make_list,
        METH_O,
        "make_list(n) -> list\n\nReturn the squares of 0..n-1.",
    },
    {
        "make_tuple",
        pyx_capi_make_tuple,
        METH_VARARGS,
        "make_tuple(a, b) -> tuple\n\nReturn a two-item tuple.",
    },
    {
        "make_dict",
        pyx_capi_make_dict,
        METH_NOARGS,
        "make_dict() -> dict\n\nReturn a small dictionary.",
    },
    {
        "dict_get",
        pyx_capi_dict_get,
        METH_VARARGS,
        "dict_get(d, key) -> object\n\nLook up a string key in a dict.",
    },
    {
        "upper",
        pyx_capi_upper,
        METH_O,
        "upper(s) -> str\n\nUppercase the ASCII letters of a string.",
    },
    {
        "half",
        pyx_capi_half,
        METH_O,
        "half(x) -> float\n\nReturn half of a number.",
    },
    {
        "reverse_bytes",
        pyx_capi_reverse_bytes,
        METH_O,
        "reverse_bytes(b) -> bytes\n\nReturn the argument's bytes in reverse order.",
    },
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef pyx_capi_module = {
    .m_base = PyModuleDef_HEAD_INIT,
    .m_name = "pyx_capi_probe",
    .m_doc = "Probe extension for the CPython stable ABI.",
    .m_size = -1,
    .m_methods = pyx_capi_methods,
    .m_slots = NULL,
    .m_traverse = NULL,
    .m_clear = NULL,
    .m_free = NULL,
};

PyMODINIT_FUNC PyInit_pyx_capi_probe(void) {
    PyObject *module = PyModule_Create(&pyx_capi_module);

    if (module == NULL) {
        return NULL;
    }
    if (PyModule_AddStringConstant(module, "tag", "abi3") < 0) {
        Py_DECREF(module);
        return NULL;
    }
    return module;
}
