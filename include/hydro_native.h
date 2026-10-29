/*
 * hydro_native.h — Hydro Native ABI (HNA) v1 public contract.
 *
 * Normative specification: docs/NATIVE_BINDINGS.md §3 ("HNA v1").
 * Value encoding: docs/ARCHITECTURE.md §3.1.
 *
 * This header is C99, self-contained, and contains no CPython header or
 * symbol. A binding compiled against it MUST NOT link libpython; if it needs
 * CPython objects it belongs to the Tier 3 extension path instead.
 *
 * Division of ownership:
 *   - The binding implements the three module entry points below
 *     (hydro_native_abi, hydro_native_abi_build, hydro_native_module_v1) and
 *     exports them from its shared library.
 *   - The runtime services below are implemented by the hydro runtime and
 *     resolved by the process linker; a binding only calls them. HNA v1
 *     reserves hydro_hna_runtime_abi, hydro_root_hold and hydro_root_release
 *     as the stable service symbols for version negotiation and persistent
 *     rooting. They are future HNA symbols and are not implemented by a
 *     binding.
 *
 * Descriptor structures are static, read-only and valid for the process
 * lifetime. The runtime caches the module object and its constants, roots
 * them persistently and never calls init twice.
 */

#ifndef HYDRO_NATIVE_H
#define HYDRO_NATIVE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* HNA major version. One integer major; only additive changes within a
 * major (new tail fields guarded by struct_size, new optionally-suffixed
 * symbols, new reserved flag bits once defined). */
#define HYDRO_HNA_VERSION 1

/* Numeric revision of the hydro runtime ABI (hydro_runtime::abi) that this
 * header was written against. Descriptor runtime_abi_min is checked against
 * the runtime-supported range by the loader. */
#define HYDRO_HNA_RUNTIME_ABI 1

/* HydroNativeModule.flags bits. Bit 0 = thread_safe: reentrant/concurrent
 * calls are allowed. All other bits are reserved and MUST be zero. */
#define HYDRO_HNA_FLAG_THREAD_SAFE 0x00000001u
#define HYDRO_HNA_FLAG_RESERVED 0xFFFFFFFEu

/* A tagged value, encoded as docs/ARCHITECTURE.md §3.1 describes. Heap
 * layout is private: a binding never dereferences a HydroValue; it uses the
 * runtime services below. */
typedef int64_t HydroValue;

typedef struct HydroNativeModule HydroNativeModule;
typedef struct HydroFunction HydroFunction;
typedef struct HydroConstant HydroConstant;

struct HydroNativeModule {
    uint32_t struct_size;        /* size of this struct as built */
    uint32_t hna_version;        /* HYDRO_HNA_VERSION */
    uint64_t runtime_abi_min;    /* minimum hydro_runtime ABI revision */
    const char *module_name;     /* exact dotted import name, UTF-8, NUL-terminated */
    const char *provider;        /* "distribution version", for diagnostics */
    const char *platform;        /* "linux-x86_64", checked by the loader */
    uint32_t flags;              /* bit 0 thread_safe; other bits reserved, must be 0 */
    int32_t (*init)(void);       /* optional; 0 = ok, sets a pending exception on failure */
    uint32_t function_count;
    const HydroFunction *functions;
    uint32_t constant_count;
    const HydroConstant *constants;
};

struct HydroFunction {
    const char *name;
    uint32_t min_args;
    uint32_t max_args;
    uint32_t flags;              /* reserved, must be 0 */
    const char *params;          /* comma-separated keyword names, or NULL */
    HydroValue (*call)(HydroValue self, uint64_t argc, const HydroValue *argv);
};

struct HydroConstant {
    const char *name;
    HydroValue (*value)(void);   /* evaluated once by the runtime, then cached */
};

/* Module entry points, implemented and exported by the binding. */
uint32_t hydro_native_abi(void);                  /* must return HYDRO_HNA_VERSION */
uint32_t hydro_native_abi_build(void);            /* build counter, for reports */
const HydroNativeModule *hydro_native_module_v1(void);

/* Runtime services: implemented by the hydro runtime, resolved by the process
 * linker. Do not define these in a binding. */
uint32_t hydro_hna_runtime_abi(void);             /* runtime ABI revision */
uint64_t hydro_root_hold(HydroValue value);       /* persistent root token */
void hydro_root_release(uint64_t token);          /* releases a hold token */

/* Tagged-value services (hydro_runtime::abi). Fallible calls set a pending
 * exception and return the None immediate; the caller observes
 * hydro_exc_pending(). No call panics or unwinds across this boundary. */
HydroValue hydro_none(void);
HydroValue hydro_true(void);
HydroValue hydro_false(void);
HydroValue hydro_bool(HydroValue b);
HydroValue hydro_int(HydroValue v);
HydroValue hydro_float_new(double f);
HydroValue hydro_str_new(const uint8_t *ptr, size_t len);
HydroValue hydro_bytes_new(const uint8_t *ptr, size_t len);
HydroValue hydro_list_new(uint64_t argc, const HydroValue *items);
HydroValue hydro_tuple_new(uint64_t argc, const HydroValue *items);
HydroValue hydro_dict_new(uint64_t argc, const HydroValue *keys,
                          const HydroValue *values);
HydroValue hydro_list_append(HydroValue list, HydroValue value);
HydroValue hydro_dict_set(HydroValue dict, HydroValue key, HydroValue value);
HydroValue hydro_getattr(HydroValue obj, const uint8_t *name, size_t name_len);
HydroValue hydro_setattr(HydroValue obj, const uint8_t *name, size_t name_len,
                         HydroValue value);
HydroValue hydro_call(HydroValue function, uint64_t argc,
                      const HydroValue *argv);
HydroValue hydro_to_str(HydroValue v);
int64_t hydro_truthy(HydroValue v);               /* 1 truthy, 0 falsy */
HydroValue hydro_binop(uint32_t op, HydroValue a, HydroValue b);
int64_t hydro_exc_pending(void);                  /* 1 pending, 0 clear */
HydroValue hydro_exc_tag(void);                   /* pending exception tag */
HydroValue hydro_raise_msg(uint32_t tag, const uint8_t *ptr, size_t len);
size_t hydro_gc_scope_push(const HydroValue *base, size_t count);
void hydro_gc_scope_pop(size_t token);

/* Typed projections used by generated wrappers (HNA v1.0 service set).
 *
 * Every projection validates its value. On failure it returns 0 and leaves a
 * pending TypeError/OverflowError; the binding returns hydro_none() and the
 * caller observes hydro_exc_pending(). An integer projection accepts bool and
 * int (CPython's int-subclass rule); float accepts int and float; string and
 * bytes projections copy exactly, never NUL-sniff. */
enum HydroTypeTag {
    HYDRO_TYPE_NONE = 0,
    HYDRO_TYPE_BOOL = 1,
    HYDRO_TYPE_INT = 2,
    HYDRO_TYPE_FLOAT = 3,
    HYDRO_TYPE_STR = 4,
    HYDRO_TYPE_BYTES = 5,
    HYDRO_TYPE_LIST = 6,
    HYDRO_TYPE_TUPLE = 7,
    HYDRO_TYPE_DICT = 8,
    HYDRO_TYPE_OTHER = 9
};

int32_t hydro_type_tag(HydroValue value);
int32_t hydro_int_value(HydroValue value, int64_t *out);
int32_t hydro_float_value(HydroValue value, double *out);
int32_t hydro_bool_value(HydroValue value, int32_t *out);
/* 0 = type error, 1 = copied, 2 = buffer too small (needed is set). */
int32_t hydro_str_copy(HydroValue value, uint8_t *buffer, uint64_t capacity,
                       uint64_t *needed);
int32_t hydro_bytes_copy(HydroValue value, uint8_t *buffer, uint64_t capacity,
                         uint64_t *needed);
HydroValue hydro_raise_type_error(const uint8_t *message, size_t len);
HydroValue hydro_raise_value_error(const uint8_t *message, size_t len);
HydroValue hydro_raise_overflow_error(const uint8_t *message, size_t len);

/* Portable static-assertion helper: _Static_assert on C11 and later,
 * static_assert in C++, a compile-time typedef check on strict C99. */
#define HYDRO_HNA_CAT_(a, b) a##b
#define HYDRO_HNA_CAT(a, b) HYDRO_HNA_CAT_(a, b)

#if defined(__cplusplus)
#define HYDRO_HNA_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
#define HYDRO_HNA_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#else
#define HYDRO_HNA_STATIC_ASSERT(cond, msg) \
    typedef char HYDRO_HNA_CAT(hydro_hna_static_assert_, __LINE__)[(cond) ? 1 : -1]
#endif

HYDRO_HNA_STATIC_ASSERT(sizeof(HydroValue) == sizeof(int64_t),
                        "HydroValue is int64_t");
HYDRO_HNA_STATIC_ASSERT(HYDRO_HNA_VERSION == 1, "HNA v1 is the only major");
HYDRO_HNA_STATIC_ASSERT((HYDRO_HNA_FLAG_THREAD_SAFE & HYDRO_HNA_FLAG_RESERVED) == 0,
                        "thread_safe bit must not be reserved");
HYDRO_HNA_STATIC_ASSERT(offsetof(HydroNativeModule, struct_size) == 0,
                        "struct_size must be the leading field");
HYDRO_HNA_STATIC_ASSERT(offsetof(HydroFunction, name) == 0,
                        "HydroFunction must lead with name");
HYDRO_HNA_STATIC_ASSERT(offsetof(HydroConstant, name) == 0,
                        "HydroConstant must lead with name");

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* HYDRO_NATIVE_H */
