/* libm fixture for hydro-bindgen: two scalar declarations in the supported
 * subset. Kept dependency-free; the generated crate is not committed. */
#ifndef HYDRO_BINDGEN_TESTS_LIBM_H
#define HYDRO_BINDGEN_TESTS_LIBM_H

double dm_sqrt(double);
double dm_hypot(double, double);

#endif /* HYDRO_BINDGEN_TESTS_LIBM_H */
