#define Py_LIMITED_API 0x030C0000

#include <Python.h>

/*
 * Multi-phase limited-API probe (PEP 489).
 *
 * `PyInit_pyx_multiphase_probe` returns a `PyModuleDef` through
 * `PyModuleDef_Init` instead of creating the module itself. PyX must create
 * the module from the definition and run the `Py_mod_exec` slot, which adds
 * the `tag` constant. `answer()` is declared in the method table.
 */

static PyObject *pyx_multiphase_answer(PyObject *self, PyObject *args) {
    (void)self;
    (void)args;
    return PyLong_FromLong(42);
}

static PyMethodDef pyx_multiphase_methods[] = {
    {"answer", pyx_multiphase_answer, METH_NOARGS, "Return 42."},
    {NULL, NULL, 0, NULL},
};

static int pyx_multiphase_exec(PyObject *module) {
    if (PyModule_AddStringConstant(module, "tag", "multiphase") < 0) {
        return -1;
    }
    return 0;
}

static PyModuleDef_Slot pyx_multiphase_slots[] = {
    {Py_mod_exec, (void *)pyx_multiphase_exec},
    {0, NULL},
};

static struct PyModuleDef pyx_multiphase_def = {
    PyModuleDef_HEAD_INIT,
    "pyx_multiphase_probe",
    "PyX multi-phase abi3 probe.",
    0,
    pyx_multiphase_methods,
    pyx_multiphase_slots,
    NULL,
    NULL,
    NULL,
};

PyMODINIT_FUNC PyInit_pyx_multiphase_probe(void) {
    return PyModuleDef_Init(&pyx_multiphase_def);
}
