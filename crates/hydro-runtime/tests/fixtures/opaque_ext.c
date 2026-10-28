/*
 * Stable-ABI fixture for opaque extension handles.
 *
 * Compiled at test time by `tests/opaque_extension.rs` and loaded through the
 * runtime extension bridge. It exports:
 *
 *   - `Thing`:      a heap type whose instances carry a long value,
 *                   expose a `value` getset attribute and a `bump()` method,
 *                   implement `tp_repr` and value-based `tp_richcompare`.
 *   - `SINGLETON`:  one pre-built instance (`Thing(11)`).
 *   - `identity(o)`: returns the same object (exercises passing a handle back).
 *
 * The `store`/`stored`/`clear_stored` family keeps Hydro value handles alive
 * across calls; `attr`, `call0`/`call1`, `repr_of`, `str_of`, `truth`,
 * `isinstance`, `issubclass`, `equal`, `type_name`, `type_flags`, `length`,
 * `list_size` and `list_item` inspect and call those handles through the plain
 * C API.
 *
 * Built with `Py_LIMITED_API`, so it must resolve entirely through the host's
 * exported CPython C API.
 */
#define Py_LIMITED_API 0x030C0000
#include <Python.h>

typedef struct {
    PyObject_HEAD
    long value;
} ProbeObject;

static PyObject *ProbeType;

static PyObject *Probe_new(PyTypeObject *type, PyObject *args, PyObject *kwds) {
    ProbeObject *self;
    (void)args;
    (void)kwds;
    /* `PyTypeObject` is opaque under `Py_LIMITED_API`. */
    self = (ProbeObject *)PyType_GenericAlloc(type, 0);
    if (self != NULL) {
        self->value = 0;
    }
    return (PyObject *)self;
}

static int Probe_init(PyObject *self, PyObject *args, PyObject *kwds) {
    long value = 0;
    (void)kwds;
    if (!PyArg_ParseTuple(args, "|l:Thing", &value)) {
        return -1;
    }
    ((ProbeObject *)self)->value = value;
    return 0;
}

static PyObject *Probe_get_value(PyObject *self, void *closure) {
    (void)closure;
    return PyLong_FromLong(((ProbeObject *)self)->value);
}

static int Probe_set_value(PyObject *self, PyObject *value, void *closure) {
    long parsed;
    (void)closure;
    if (value == NULL) {
        PyErr_SetString(PyExc_AttributeError, "cannot delete value");
        return -1;
    }
    parsed = PyLong_AsLong(value);
    if (parsed == -1 && PyErr_Occurred()) {
        return -1;
    }
    ((ProbeObject *)self)->value = parsed;
    return 0;
}

static PyGetSetDef Probe_getset[] = {
    {"value", Probe_get_value, Probe_set_value, "the stored long", NULL},
    {NULL, NULL, NULL, NULL, NULL},
};

static PyObject *Probe_bump(PyObject *self, PyObject *args) {
    long delta = 1;
    if (!PyArg_ParseTuple(args, "|l:bump", &delta)) {
        return NULL;
    }
    ((ProbeObject *)self)->value += delta;
    return PyLong_FromLong(((ProbeObject *)self)->value);
}

static PyMethodDef Probe_methods[] = {
    {"bump", Probe_bump, METH_VARARGS, "bump(delta=1) -> new value"},
    {NULL, NULL, 0, NULL},
};

static PyObject *Probe_repr(PyObject *self) {
    return PyUnicode_FromFormat("<Thing %ld>", ((ProbeObject *)self)->value);
}

static PyObject *Probe_richcompare(PyObject *self, PyObject *other, int op) {
    long left = ((ProbeObject *)self)->value;
    long right;
    int result;

    if (!PyObject_IsInstance(other, ProbeType)) {
        Py_RETURN_NOTIMPLEMENTED;
    }
    right = ((ProbeObject *)other)->value;
    switch (op) {
    case Py_EQ:
        result = left == right;
        break;
    case Py_NE:
        result = left != right;
        break;
    case Py_LT:
        result = left < right;
        break;
    case Py_LE:
        result = left <= right;
        break;
    case Py_GT:
        result = left > right;
        break;
    case Py_GE:
        result = left >= right;
        break;
    default:
        Py_RETURN_NOTIMPLEMENTED;
    }
    return PyBool_FromLong(result);
}

static PyType_Slot Probe_slots[] = {
    {Py_tp_new, (void *)Probe_new},
    {Py_tp_init, (void *)Probe_init},
    {Py_tp_methods, (void *)Probe_methods},
    {Py_tp_getset, (void *)Probe_getset},
    {Py_tp_repr, (void *)Probe_repr},
    {Py_tp_richcompare, (void *)Probe_richcompare},
    {0, NULL},
};

static PyType_Spec Probe_spec = {
    "opaque_ext.Thing",
    (int)sizeof(ProbeObject),
    0,
    Py_TPFLAGS_DEFAULT,
    Probe_slots,
};

/* A type with no construction protocol: using it as a Hydro base must fail
 * with a clear TypeError instead of fabricating a layout. */
static PyType_Slot Opaque_slots[] = {
    {0, NULL},
};

static PyType_Spec Opaque_spec = {
    "opaque_ext.Opaque",
    (int)sizeof(ProbeObject),
    0,
    Py_TPFLAGS_DISALLOW_INSTANTIATION,
    Opaque_slots,
};

static PyObject *ext_identity(PyObject *module, PyObject *args) {
    PyObject *object;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:identity", &object)) {
        return NULL;
    }
    Py_INCREF(object);
    return object;
}

static PyObject *ext_same(PyObject *module, PyObject *args) {
    PyObject *left;
    PyObject *right;
    (void)module;
    if (!PyArg_ParseTuple(args, "OO:same", &left, &right)) {
        return NULL;
    }
    return PyBool_FromLong(left == right);
}

static PyObject *ext_make(PyObject *module, PyObject *args) {
    long value;
    PyObject *argument;
    PyObject *result;
    (void)module;
    if (!PyArg_ParseTuple(args, "l:make", &value)) {
        return NULL;
    }
    argument = PyLong_FromLong(value);
    if (argument == NULL) {
        return NULL;
    }
    {
        PyObject *call_args = PyTuple_Pack(1, argument);
        Py_DECREF(argument);
        if (call_args == NULL) {
            return NULL;
        }
        result = PyObject_Call(ProbeType, call_args, NULL);
        Py_DECREF(call_args);
    }
    return result;
}

/* --- Hydro value handles ------------------------------------------------
 *
 * Values the Hydro bridge passes to a C extension as real `PyObject *`
 * handles: a class, an instance, a function, a module and a container. The
 * fixture stores them across calls (taking its own references), inspects and
 * calls them through the plain C API, and releases them on demand.
 */

enum {
    HYDRO_SLOT_CLASS = 0,
    HYDRO_SLOT_INSTANCE = 1,
    HYDRO_SLOT_FUNCTION = 2,
    HYDRO_SLOT_MODULE = 3,
    HYDRO_SLOT_CONTAINER = 4,
    HYDRO_SLOT_COUNT = 5
};

static PyObject *hydro_slots[HYDRO_SLOT_COUNT];

static PyObject **hydro_slot(long which) {
    if (which < 0 || which >= HYDRO_SLOT_COUNT) {
        return NULL;
    }
    return &hydro_slots[which];
}

/* store(value, slot): keep a reference to a Hydro handle across calls. */
static PyObject *ext_hydro_store(PyObject *module, PyObject *args) {
    PyObject *value;
    PyObject **slot;
    long which;
    (void)module;
    if (!PyArg_ParseTuple(args, "Ol:store", &value, &which)) {
        return NULL;
    }
    slot = hydro_slot(which);
    if (slot == NULL) {
        PyErr_SetString(PyExc_ValueError, "store: slot out of range");
        return NULL;
    }
    Py_INCREF(value);
    Py_XDECREF(*slot);
    *slot = value;
    Py_RETURN_NONE;
}

/* stored(slot): the stored handle, or None when the slot is empty. */
static PyObject *ext_hydro_stored(PyObject *module, PyObject *args) {
    PyObject **slot;
    long which;
    (void)module;
    if (!PyArg_ParseTuple(args, "l:stored", &which)) {
        return NULL;
    }
    slot = hydro_slot(which);
    if (slot == NULL) {
        PyErr_SetString(PyExc_ValueError, "stored: slot out of range");
        return NULL;
    }
    if (*slot == NULL) {
        Py_RETURN_NONE;
    }
    Py_INCREF(*slot);
    return *slot;
}

/* clear_stored(): release every stored handle. */
static PyObject *ext_hydro_clear(PyObject *module, PyObject *args) {
    long which;
    (void)module;
    (void)args;
    for (which = 0; which < HYDRO_SLOT_COUNT; which++) {
        Py_CLEAR(hydro_slots[which]);
    }
    Py_RETURN_NONE;
}

/* attr(object, name): PyObject_GetAttrString on a Hydro handle. */
static PyObject *ext_hydro_attr(PyObject *module, PyObject *args) {
    PyObject *object;
    const char *name;
    (void)module;
    if (!PyArg_ParseTuple(args, "Os:attr", &object, &name)) {
        return NULL;
    }
    return PyObject_GetAttrString(object, name);
}

/* call0(object): call with no arguments. */
static PyObject *ext_hydro_call0(PyObject *module, PyObject *args) {
    PyObject *object;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:call0", &object)) {
        return NULL;
    }
    return PyObject_CallObject(object, NULL);
}

/* call1(object, argument): call with one positional argument. */
static PyObject *ext_hydro_call1(PyObject *module, PyObject *args) {
    PyObject *object;
    PyObject *argument;
    PyObject *call_args;
    PyObject *result;
    (void)module;
    if (!PyArg_ParseTuple(args, "OO:call1", &object, &argument)) {
        return NULL;
    }
    call_args = PyTuple_Pack(1, argument);
    if (call_args == NULL) {
        return NULL;
    }
    result = PyObject_CallObject(object, call_args);
    Py_DECREF(call_args);
    return result;
}

/* repr(object): the extension sees PyObject_Repr dispatched to Hydro. */
static PyObject *ext_hydro_repr(PyObject *module, PyObject *args) {
    PyObject *object;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:repr", &object)) {
        return NULL;
    }
    return PyObject_Repr(object);
}

/* str_value(object): PyObject_Str dispatched to Hydro. */
static PyObject *ext_hydro_str(PyObject *module, PyObject *args) {
    PyObject *object;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:str_value", &object)) {
        return NULL;
    }
    return PyObject_Str(object);
}

/* truth(object): PyObject_IsTrue on a Hydro handle. */
static PyObject *ext_hydro_truth(PyObject *module, PyObject *args) {
    PyObject *object;
    int truth;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:truth", &object)) {
        return NULL;
    }
    truth = PyObject_IsTrue(object);
    if (truth < 0) {
        return NULL;
    }
    return PyBool_FromLong(truth);
}

/* isinstance(object, class): PyObject_IsInstance on a Hydro handle. */
static PyObject *ext_hydro_isinstance(PyObject *module, PyObject *args) {
    PyObject *object;
    PyObject *class_object;
    int result;
    (void)module;
    if (!PyArg_ParseTuple(args, "OO:isinstance", &object, &class_object)) {
        return NULL;
    }
    result = PyObject_IsInstance(object, class_object);
    if (result < 0) {
        return NULL;
    }
    return PyBool_FromLong(result);
}

/* issubclass(a, b): PyObject_IsSubclass on Hydro class handles. */
static PyObject *ext_hydro_issubclass(PyObject *module, PyObject *args) {
    PyObject *derived;
    PyObject *class_object;
    int result;
    (void)module;
    if (!PyArg_ParseTuple(args, "OO:issubclass", &derived, &class_object)) {
        return NULL;
    }
    result = PyObject_IsSubclass(derived, class_object);
    if (result < 0) {
        return NULL;
    }
    return PyBool_FromLong(result);
}

/* equal(a, b): rich comparison between two Hydro handles. */
static PyObject *ext_hydro_equal(PyObject *module, PyObject *args) {
    PyObject *left;
    PyObject *right;
    int result;
    (void)module;
    if (!PyArg_ParseTuple(args, "OO:equal", &left, &right)) {
        return NULL;
    }
    result = PyObject_RichCompareBool(left, right, Py_EQ);
    if (result < 0) {
        return NULL;
    }
    return PyBool_FromLong(result);
}

/* type_name(class): PyType_GetName on a Hydro class handle. */
static PyObject *ext_hydro_type_name(PyObject *module, PyObject *args) {
    PyObject *class_object;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:type_name", &class_object)) {
        return NULL;
    }
    return PyType_GetName((PyTypeObject *)class_object);
}

/* type_flags(class): 1 when PyType_GetFlags reports a heap type. */
static PyObject *ext_hydro_type_flags(PyObject *module, PyObject *args) {
    PyObject *class_object;
    unsigned long flags;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:type_flags", &class_object)) {
        return NULL;
    }
    flags = (unsigned long)PyType_GetFlags((PyTypeObject *)class_object);
    if (flags == 0 && PyErr_Occurred()) {
        return NULL;
    }
    /* Py_TPFLAGS_HEAPTYPE (1 << 9) must be reported for a Hydro class. */
    return PyBool_FromLong((flags & 0x200UL) != 0);
}

/* length(object): PyObject_Length dispatched to Hydro. */
static PyObject *ext_hydro_length(PyObject *module, PyObject *args) {
    PyObject *object;
    Py_ssize_t length;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:length", &object)) {
        return NULL;
    }
    length = PyObject_Length(object);
    if (length < 0 && PyErr_Occurred()) {
        return NULL;
    }
    return PyLong_FromSsize_t(length);
}

/* list_size(list): size of a materialised Hydro list. */
static PyObject *ext_hydro_list_size(PyObject *module, PyObject *args) {
    PyObject *list;
    Py_ssize_t size;
    (void)module;
    if (!PyArg_ParseTuple(args, "O:list_size", &list)) {
        return NULL;
    }
    size = PyList_Size(list);
    if (size < 0) {
        return NULL;
    }
    return PyLong_FromSsize_t(size);
}

/* list_item(list, index): one element, with a new reference. */
static PyObject *ext_hydro_list_item(PyObject *module, PyObject *args) {
    PyObject *list;
    PyObject *item;
    long index;
    (void)module;
    if (!PyArg_ParseTuple(args, "Ol:list_item", &list, &index)) {
        return NULL;
    }
    item = PyList_GetItem(list, (Py_ssize_t)index);
    if (item == NULL) {
        return NULL;
    }
    Py_INCREF(item);
    return item;
}

static PyMethodDef module_methods[] = {
    {"identity", ext_identity, METH_VARARGS, "identity(object) -> object"},
    {"same", ext_same, METH_VARARGS, "same(a, b) -> bool"},
    {"make", ext_make, METH_VARARGS, "make(value) -> Thing"},
    {"store", ext_hydro_store, METH_VARARGS, "store(value, slot) -> None"},
    {"stored", ext_hydro_stored, METH_VARARGS, "stored(slot) -> object"},
    {"clear_stored", ext_hydro_clear, METH_VARARGS, "clear_stored() -> None"},
    {"attr", ext_hydro_attr, METH_VARARGS, "attr(object, name) -> object"},
    {"call0", ext_hydro_call0, METH_VARARGS, "call0(object) -> object"},
    {"call1", ext_hydro_call1, METH_VARARGS, "call1(object, argument) -> object"},
    {"repr_of", ext_hydro_repr, METH_VARARGS, "repr_of(object) -> str"},
    {"str_of", ext_hydro_str, METH_VARARGS, "str_of(object) -> str"},
    {"truth", ext_hydro_truth, METH_VARARGS, "truth(object) -> bool"},
    {"isinstance", ext_hydro_isinstance, METH_VARARGS, "isinstance(object, class) -> bool"},
    {"issubclass", ext_hydro_issubclass, METH_VARARGS, "issubclass(a, b) -> bool"},
    {"equal", ext_hydro_equal, METH_VARARGS, "equal(a, b) -> bool"},
    {"type_name", ext_hydro_type_name, METH_VARARGS, "type_name(class) -> str"},
    {"type_flags", ext_hydro_type_flags, METH_VARARGS, "type_flags(class) -> bool"},
    {"length", ext_hydro_length, METH_VARARGS, "length(object) -> int"},
    {"list_size", ext_hydro_list_size, METH_VARARGS, "list_size(list) -> int"},
    {"list_item", ext_hydro_list_item, METH_VARARGS, "list_item(list, index) -> object"},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef opaque_module = {
    PyModuleDef_HEAD_INIT,
    "opaque_ext",
    "Opaque-handle fixture for the Hydro extension bridge.",
    -1,
    module_methods,
    NULL,
    NULL,
    NULL,
    NULL,
};

PyMODINIT_FUNC PyInit_opaque_ext(void) {
    PyObject *module;
    PyObject *argument;
    PyObject *singleton;

    module = PyModule_Create(&opaque_module);
    if (module == NULL) {
        return NULL;
    }

    ProbeType = PyType_FromSpec(&Probe_spec);
    if (ProbeType == NULL) {
        Py_DECREF(module);
        return NULL;
    }
    /* `PyModule_AddObject` steals the reference. */
    if (PyModule_AddObject(module, "Thing", ProbeType) < 0) {
        Py_DECREF(ProbeType);
        Py_DECREF(module);
        return NULL;
    }

    {
        PyObject *opaque = PyType_FromSpec(&Opaque_spec);
        if (opaque == NULL) {
            Py_DECREF(module);
            return NULL;
        }
        if (PyModule_AddObject(module, "Opaque", opaque) < 0) {
            Py_DECREF(opaque);
            Py_DECREF(module);
            return NULL;
        }
    }

    argument = PyLong_FromLong(11);
    if (argument == NULL) {
        Py_DECREF(module);
        return NULL;
    }
    {
        PyObject *call_args = PyTuple_Pack(1, argument);
        Py_DECREF(argument);
        if (call_args == NULL) {
            Py_DECREF(module);
            return NULL;
        }
        singleton = PyObject_Call(ProbeType, call_args, NULL);
        Py_DECREF(call_args);
    }
    if (singleton == NULL) {
        Py_DECREF(module);
        return NULL;
    }
    if (PyModule_AddObject(module, "SINGLETON", singleton) < 0) {
        Py_DECREF(singleton);
        Py_DECREF(module);
        return NULL;
    }

    return module;
}
