#define _POSIX_C_SOURCE 200809L

#include <Python.h>

#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#define CALL_COUNT 100000

static double now_ms(void) {
    struct timespec ts;

    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        perror("clock_gettime");
        exit(EXIT_FAILURE);
    }
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1000000.0;
}

static long read_vmhwm_kb(void) {
    FILE *status = fopen("/proc/self/status", "r");
    char line[256];
    long value = -1;

    if (status == NULL) {
        return -1;
    }

    while (fgets(line, sizeof(line), status) != NULL) {
        if (sscanf(line, "VmHWM: %ld kB", &value) == 1) {
            break;
        }
    }

    fclose(status);
    return value;
}

static double import_module_ms(const char *name, int *available) {
    double start = now_ms();
    PyObject *module = PyImport_ImportModule(name);
    double elapsed = now_ms() - start;

    if (module == NULL) {
        PyErr_Clear();
        *available = 0;
    } else {
        Py_DECREF(module);
        *available = 1;
    }
    return elapsed;
}

static double call_inc_100k_ms(int *available) {
    PyObject *main_module;
    PyObject *inc;
    PyObject *argument;
    double start;
    double elapsed;
    int i;

    main_module = PyImport_AddModule("__main__");
    if (main_module == NULL) {
        PyErr_Clear();
        *available = 0;
        return 0.0;
    }

    inc = PyObject_GetAttrString(main_module, "inc");
    if (inc == NULL) {
        PyErr_Clear();
        *available = 0;
        return 0.0;
    }

    argument = PyLong_FromLong(41);
    if (argument == NULL) {
        PyErr_Clear();
        Py_DECREF(inc);
        *available = 0;
        return 0.0;
    }

    start = now_ms();
    for (i = 0; i < CALL_COUNT; i++) {
        PyObject *result = PyObject_CallOneArg(inc, argument);

        if (result == NULL) {
            PyErr_Clear();
            Py_DECREF(argument);
            Py_DECREF(inc);
            *available = 0;
            return 0.0;
        }
        Py_DECREF(result);
    }
    elapsed = now_ms() - start;

    Py_DECREF(argument);
    Py_DECREF(inc);
    *available = 1;
    return elapsed;
}

int main(void) {
    double init_start;
    double init_ms;
    double pass_start;
    double pass_ms;
    double import_numpy_ms;
    double import_pydantic_ms;
    double call100k_ms;
    double finalize_start;
    double finalize_ms;
    int numpy_available;
    int pydantic_available;
    int call_available;
    int pass_ok;
    int define_ok;
    long rss_kb;

    init_start = now_ms();
    Py_Initialize();
    init_ms = now_ms() - init_start;

    pass_start = now_ms();
    pass_ok = PyRun_SimpleString("pass") == 0;
    pass_ms = now_ms() - pass_start;
    if (!pass_ok) {
        PyErr_Clear();
    }

    define_ok = PyRun_SimpleString("def inc(x):\n    return x + 1\n") == 0;
    if (!define_ok) {
        PyErr_Clear();
    }

    import_numpy_ms = import_module_ms("numpy", &numpy_available);
    import_pydantic_ms = import_module_ms("pydantic", &pydantic_available);

    if (define_ok) {
        call100k_ms = call_inc_100k_ms(&call_available);
    } else {
        call100k_ms = 0.0;
        call_available = 0;
    }

    finalize_start = now_ms();
    Py_FinalizeEx();
    finalize_ms = now_ms() - finalize_start;

    rss_kb = read_vmhwm_kb();

    printf("init_ms=%.3f\n", init_ms);
    if (pass_ok) {
        printf("pass_ms=%.3f\n", pass_ms);
    } else {
        printf("pass_ms=unavailable\n");
    }
    if (numpy_available) {
        printf("import_numpy_ms=%.3f\n", import_numpy_ms);
    } else {
        printf("import_numpy_ms=unavailable\n");
    }
    if (pydantic_available) {
        printf("import_pydantic_ms=%.3f\n", import_pydantic_ms);
    } else {
        printf("import_pydantic_ms=unavailable\n");
    }
    if (call_available) {
        printf("call100k_ms=%.3f\n", call100k_ms);
    } else {
        printf("call100k_ms=unavailable\n");
    }
    printf("finalize_ms=%.3f\n", finalize_ms);
    if (rss_kb >= 0) {
        printf("rss_kb=%ld\n", rss_kb);
    } else {
        printf("rss_kb=unavailable\n");
    }

    return 0;
}
