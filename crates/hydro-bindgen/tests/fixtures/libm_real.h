/* Real-libm fixture for hydro-bindgen. Unlike libm.h (which only exercises
 * descriptor rendering with dm_-prefixed names), this header declares actual
 * libm symbols so the generated binding can be built, loaded and called
 * end-to-end by the Tea pilot in `tea/tests/bindgen.rs`. */
#ifndef HYDRO_BINDGEN_TESTS_LIBM_REAL_H
#define HYDRO_BINDGEN_TESTS_LIBM_REAL_H

double sqrt(double x);
double hypot(double x, double y);

#endif /* HYDRO_BINDGEN_TESTS_LIBM_REAL_H */
