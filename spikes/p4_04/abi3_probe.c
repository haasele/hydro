#define Py_LIMITED_API 0x030C0000

#include <Python.h>

static PyObject *hydro_capi_add(PyObject *self, PyObject *args) {
    int a;
    int b;

    (void)self;
    if (!PyArg_ParseTuple(args, "ii:add", &a, &b)) {
        return NULL;
    }
    return PyLong_FromLong((long)a + (long)b);
}

static PyMethodDef hydro_capi_methods[] = {
    {
        "add",
        hydro_capi_add,
        METH_VARARGS,
        "add(a, b) -> int\n\nReturn the sum of two integers.",
    },
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef hydro_capi_module = {
    .m_base = PyModuleDef_HEAD_INIT,
    .m_name = "hydro_capi_probe",
    .m_doc = "Probe extension for the CPython stable ABI.",
    .m_size = -1,
    .m_methods = hydro_capi_methods,
    .m_slots = NULL,
    .m_traverse = NULL,
    .m_clear = NULL,
    .m_free = NULL,
};

PyMODINIT_FUNC PyInit_hydro_capi_probe(void) {
    PyObject *module = PyModule_Create(&hydro_capi_module);

    if (module == NULL) {
        return NULL;
    }
    if (PyModule_AddStringConstant(module, "tag", "abi3") < 0) {
        Py_DECREF(module);
        return NULL;
    }
    return module;
}
