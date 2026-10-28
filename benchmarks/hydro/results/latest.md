# Hydro Micro-Benchmark Baseline

_2026-10-29 00:28:28 · Linux-7.2.11-1-cachyos-x86_64-with-glibc2.44 · x86_64 · repeats=5_

| workload | impl | median ms | min ms | build ms | RSS MB | status |
|---|---|---:|---:|---:|---:|---|
| while_sum | cpython | 25.0 | 24.5 | - | - | ok |
| while_sum | hydro-jit | 4.2 | 4.1 | - | - | ok |
| while_sum | hydro-aot | 2.5 | 2.4 | 233 | - | ok |
| for_sum | cpython | 19.3 | 17.7 | - | - | ok |
| for_sum | hydro-jit | 3.4 | 3.2 | - | - | ok |
| for_sum | hydro-aot | 2.2 | 1.9 | 231 | - | ok |
| range_sum | cpython | 9.9 | 9.6 | - | - | ok |
| range_sum | hydro-jit | 12.5 | 12.1 | - | - | ok |
| range_sum | hydro-aot | 9.0 | 8.9 | 227 | - | ok |
| list_append | cpython | 17.8 | 17.2 | - | - | ok |
| list_append | hydro-jit | 8.6 | 8.2 | - | - | ok |
| list_append | hydro-aot | 7.0 | 5.7 | 226 | - | ok |
| dict_insert | cpython | 9.3 | 8.7 | - | - | ok |
| dict_insert | hydro-jit | 2.9 | 2.8 | - | - | ok |
| dict_insert | hydro-aot | 1.8 | 1.6 | 224 | - | ok |
| string_concat | cpython | 9.0 | 8.4 | - | - | ok |
| string_concat | hydro-jit | 3.1 | 2.8 | - | - | ok |
| string_concat | hydro-aot | 1.8 | 1.5 | 227 | - | ok |
| list_comp | cpython | 11.5 | 11.3 | - | - | ok |
| list_comp | hydro-jit | 8.9 | 8.1 | - | - | ok |
| list_comp | hydro-aot | 6.9 | 6.0 | 222 | - | ok |
| filter_comp | cpython | 11.2 | 10.5 | - | - | ok |
| filter_comp | hydro-jit | 6.3 | 6.2 | - | - | ok |
| filter_comp | hydro-aot | 3.5 | 3.4 | 240 | - | ok |
| sum_genexp | cpython | 12.4 | 11.6 | - | - | ok |
| sum_genexp | hydro-jit | 26.1 | 25.5 | - | - | ok |
| sum_genexp | hydro-aot | 19.4 | 18.9 | 232 | - | ok |
| map_lambda | cpython | 13.4 | 12.8 | - | - | ok |
| map_lambda | hydro-jit | 28.1 | 27.3 | - | - | ok |
| map_lambda | hydro-aot | 25.8 | 24.9 | 228 | - | ok |
| filter_lambda | cpython | 12.6 | 12.4 | - | - | ok |
| filter_lambda | hydro-jit | 24.8 | 23.7 | - | - | ok |
| filter_lambda | hydro-aot | 20.8 | 20.2 | 223 | - | ok |
| lambda_call | cpython | 14.5 | 13.8 | - | - | ok |
| lambda_call | hydro-jit | 27.9 | 27.6 | - | - | ok |
| lambda_call | hydro-aot | 25.6 | 24.8 | 225 | - | ok |
| reduce_add | cpython | 12.6 | 11.9 | - | - | ok |
| reduce_add | hydro-jit | 9.1 | 8.9 | - | - | ok |
| reduce_add | hydro-aot | 6.9 | 6.6 | 228 | - | ok |
| math_loop | cpython | 14.0 | 13.1 | - | - | ok |
| math_loop | hydro-jit | 3.7 | 3.3 | - | - | ok |
| math_loop | hydro-aot | 1.9 | 1.4 | 225 | - | ok |
| json_dumps | cpython | 13.8 | 13.6 | - | - | ok |
| json_dumps | hydro-jit | 3.5 | 3.4 | - | - | ok |
| json_dumps | hydro-aot | 1.9 | 1.9 | 235 | - | ok |
| json_loads | cpython | 14.5 | 13.6 | - | - | ok |
| json_loads | hydro-jit | 3.8 | 3.5 | - | - | ok |
| json_loads | hydro-aot | 2.1 | 2.0 | 225 | - | ok |
| threads_serial | cpython | 56.3 | 55.6 | - | - | ok |
| threads_serial | hydro-jit | 8.4 | 8.0 | - | - | ok |
| threads_serial | hydro-aot | 6.6 | 6.0 | 217 | - | ok |
| threads_parallel | cpython | 35.2 | 34.7 | - | - | ok |
| threads_parallel | hydro-jit | 9.1 | 8.7 | - | - | ok |
| threads_parallel | hydro-aot | 4.6 | 3.9 | 227 | - | ok |
| pool_map | cpython | 110.2 | 108.3 | - | - | ok |
| pool_map | hydro-jit | 6.3 | 5.6 | - | - | ok |
| pool_map | hydro-aot | 3.5 | 3.3 | 236 | - | ok |
| pool_serial | cpython | 132.5 | 127.9 | - | - | ok |
| pool_serial | hydro-jit | 13.0 | 12.9 | - | - | ok |
| pool_serial | hydro-aot | 10.1 | 9.3 | 258 | - | ok |
| pool_parallel | cpython | 117.1 | 113.4 | - | - | ok |
| pool_parallel | hydro-jit | 8.3 | 7.8 | - | - | ok |
| pool_parallel | hydro-aot | 5.1 | 4.4 | 229 | - | ok |
| process_queue | cpython | 89.4 | 88.6 | - | - | ok |
| process_queue | hydro-jit | 8.7 | 8.0 | - | - | ok |
| process_queue | hydro-aot | 4.4 | 3.8 | 227 | - | ok |
| startup_hello | cpython | 8.3 | 7.7 | - | - | ok |
| startup_hello | hydro-jit | 2.0 | 2.0 | - | - | ok |
| startup_hello | hydro-aot | 1.0 | 0.9 | 234 | - | ok |

Baseline for the import-free Hydro subset; dicts use a hash index
(insertion order preserved), while string concatenation is still
O(n²), so that load stays small. Not comparable to the CPython/uv
harnesses in benchmarks/.
