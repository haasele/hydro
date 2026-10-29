# Hydro Micro-Benchmark Baseline

_2026-10-29 01:58:53 · Linux-7.2.11-1-cachyos-x86_64-with-glibc2.44 · x86_64 · repeats=5_

| workload | impl | median ms | min ms | build ms | RSS MB | status |
|---|---|---:|---:|---:|---:|---|
| while_sum | cpython | 22.6 | 22.2 | - | - | ok |
| while_sum | hydro-jit | 4.6 | 4.4 | - | - | ok |
| while_sum | hydro-aot | 2.6 | 2.3 | 229 | - | ok |
| for_sum | cpython | 18.1 | 17.9 | - | - | ok |
| for_sum | hydro-jit | 3.3 | 3.1 | - | - | ok |
| for_sum | hydro-aot | 2.0 | 1.6 | 225 | - | ok |
| range_sum | cpython | 10.1 | 9.6 | - | - | ok |
| range_sum | hydro-jit | 6.4 | 5.8 | - | - | ok |
| range_sum | hydro-aot | 3.3 | 3.0 | 224 | - | ok |
| list_append | cpython | 18.0 | 16.5 | - | - | ok |
| list_append | hydro-jit | 9.3 | 9.1 | - | - | ok |
| list_append | hydro-aot | 6.1 | 5.6 | 230 | - | ok |
| dict_insert | cpython | 8.8 | 8.2 | - | - | ok |
| dict_insert | hydro-jit | 2.9 | 2.8 | - | - | ok |
| dict_insert | hydro-aot | 1.6 | 1.6 | 246 | - | ok |
| string_concat | cpython | 9.2 | 8.4 | - | - | ok |
| string_concat | hydro-jit | 2.9 | 2.8 | - | - | ok |
| string_concat | hydro-aot | 1.9 | 1.7 | 219 | - | ok |
| list_comp | cpython | 13.3 | 12.0 | - | - | ok |
| list_comp | hydro-jit | 5.3 | 5.1 | - | - | ok |
| list_comp | hydro-aot | 2.9 | 2.8 | 222 | - | ok |
| filter_comp | cpython | 11.0 | 10.6 | - | - | ok |
| filter_comp | hydro-jit | 6.3 | 6.0 | - | - | ok |
| filter_comp | hydro-aot | 4.0 | 3.4 | 216 | - | ok |
| sum_genexp | cpython | 11.8 | 10.9 | - | - | ok |
| sum_genexp | hydro-jit | 9.3 | 8.8 | - | - | ok |
| sum_genexp | hydro-aot | 7.3 | 6.9 | 218 | - | ok |
| map_lambda | cpython | 13.4 | 13.0 | - | - | ok |
| map_lambda | hydro-jit | 13.8 | 13.5 | - | - | ok |
| map_lambda | hydro-aot | 11.0 | 10.3 | 221 | - | ok |
| filter_lambda | cpython | 12.6 | 12.3 | - | - | ok |
| filter_lambda | hydro-jit | 13.7 | 13.4 | - | - | ok |
| filter_lambda | hydro-aot | 10.4 | 9.8 | 223 | - | ok |
| lambda_call | cpython | 15.0 | 13.9 | - | - | ok |
| lambda_call | hydro-jit | 14.9 | 13.1 | - | - | ok |
| lambda_call | hydro-aot | 11.2 | 10.9 | 232 | - | ok |
| reduce_add | cpython | 12.8 | 11.8 | - | - | ok |
| reduce_add | hydro-jit | 5.5 | 5.4 | - | - | ok |
| reduce_add | hydro-aot | 3.3 | 3.2 | 229 | - | ok |
| math_loop | cpython | 13.0 | 12.3 | - | - | ok |
| math_loop | hydro-jit | 3.0 | 2.7 | - | - | ok |
| math_loop | hydro-aot | 1.5 | 1.3 | 221 | - | ok |
| json_dumps | cpython | 13.5 | 13.3 | - | - | ok |
| json_dumps | hydro-jit | 3.8 | 3.5 | - | - | ok |
| json_dumps | hydro-aot | 2.3 | 2.0 | 221 | - | ok |
| json_loads | cpython | 14.4 | 13.8 | - | - | ok |
| json_loads | hydro-jit | 3.9 | 3.7 | - | - | ok |
| json_loads | hydro-aot | 2.3 | 2.2 | 223 | - | ok |
| threads_serial | cpython | 56.9 | 56.4 | - | - | ok |
| threads_serial | hydro-jit | 10.0 | 9.7 | - | - | ok |
| threads_serial | hydro-aot | 7.2 | 6.9 | 216 | - | ok |
| threads_parallel | cpython | 35.4 | 34.0 | - | - | ok |
| threads_parallel | hydro-jit | 9.3 | 8.1 | - | - | ok |
| threads_parallel | hydro-aot | 4.5 | 4.0 | 221 | - | ok |
| pool_map | cpython | 108.6 | 104.7 | - | - | ok |
| pool_map | hydro-jit | 6.7 | 5.9 | - | - | ok |
| pool_map | hydro-aot | 3.3 | 2.8 | 241 | - | ok |
| pool_serial | cpython | 129.5 | 127.4 | - | - | ok |
| pool_serial | hydro-jit | 15.7 | 14.6 | - | - | ok |
| pool_serial | hydro-aot | 11.7 | 10.4 | 225 | - | ok |
| pool_parallel | cpython | 118.3 | 116.1 | - | - | ok |
| pool_parallel | hydro-jit | 9.4 | 8.9 | - | - | ok |
| pool_parallel | hydro-aot | 5.7 | 5.0 | 224 | - | ok |
| process_queue | cpython | 88.3 | 87.0 | - | - | ok |
| process_queue | hydro-jit | 8.1 | 7.3 | - | - | ok |
| process_queue | hydro-aot | 4.0 | 3.8 | 257 | - | ok |
| startup_hello | cpython | 8.8 | 8.1 | - | - | ok |
| startup_hello | hydro-jit | 2.0 | 1.8 | - | - | ok |

Baseline for the import-free Hydro subset; dicts use a hash index
(insertion order preserved), while string concatenation is still
O(n²), so that load stays small. Not comparable to the CPython/uv
harnesses in benchmarks/.
