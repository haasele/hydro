# PyX Micro-Benchmark Baseline

_2026-10-24 19:24:40 · Linux-7.2.11-1-cachyos-x86_64-with-glibc2.44 · x86_64 · repeats=5_

| workload | impl | median ms | min ms | build ms | RSS MB | status |
|---|---|---:|---:|---:|---:|---|
| while_sum | cpython | 28.9 | 27.6 | - | - | ok |
| while_sum | pyx-jit | 3.1 | 2.7 | - | - | ok |
| while_sum | pyx-aot | 2.4 | 2.1 | 110 | - | ok |
| for_sum | cpython | 27.5 | 26.1 | - | - | ok |
| for_sum | pyx-jit | 3.7 | 3.1 | - | - | ok |
| for_sum | pyx-aot | 2.0 | 1.8 | 112 | - | ok |
| range_sum | cpython | 13.4 | 12.8 | - | - | ok |
| range_sum | pyx-jit | 4.6 | 4.1 | - | - | ok |
| range_sum | pyx-aot | 4.0 | 3.1 | 118 | - | ok |
| list_append | cpython | 24.6 | 23.6 | - | - | ok |
| list_append | pyx-jit | 6.7 | 6.0 | - | - | ok |
| list_append | pyx-aot | 4.7 | 4.6 | 106 | - | ok |
| dict_insert | cpython | 12.7 | 11.3 | - | - | ok |
| dict_insert | pyx-jit | 3.3 | 2.9 | - | - | ok |
| dict_insert | pyx-aot | 2.1 | 2.0 | 106 | - | ok |
| string_concat | cpython | 12.3 | 10.7 | - | - | ok |
| string_concat | pyx-jit | 8.6 | 7.8 | - | - | ok |
| string_concat | pyx-aot | 7.7 | 6.9 | 110 | - | ok |
| list_comp | cpython | 17.9 | 17.0 | - | - | ok |
| list_comp | pyx-jit | 3.9 | 3.6 | - | - | ok |
| list_comp | pyx-aot | 3.7 | 2.9 | 101 | - | ok |
| filter_comp | cpython | 14.8 | 14.4 | - | - | ok |
| filter_comp | pyx-jit | 4.6 | 4.0 | - | - | ok |
| filter_comp | pyx-aot | 4.1 | 3.7 | 101 | - | ok |
| sum_genexp | cpython | 15.4 | 15.1 | - | - | ok |
| sum_genexp | pyx-jit | 5.2 | 4.6 | - | - | ok |
| sum_genexp | pyx-aot | 4.1 | 3.9 | 109 | - | ok |
| map_lambda | cpython | 19.4 | 17.6 | - | - | ok |
| map_lambda | pyx-jit | 7.4 | 6.9 | - | - | ok |
| map_lambda | pyx-aot | 6.9 | 6.0 | 104 | - | ok |
| filter_lambda | cpython | 18.2 | 17.1 | - | - | ok |
| filter_lambda | pyx-jit | 8.2 | 7.1 | - | - | ok |
| filter_lambda | pyx-aot | 6.0 | 5.8 | 118 | - | ok |
| lambda_call | cpython | 16.1 | 15.4 | - | - | ok |
| lambda_call | pyx-jit | 5.1 | 4.9 | - | - | ok |
| lambda_call | pyx-aot | 4.6 | 4.0 | 95 | - | ok |
| reduce_add | cpython | 14.3 | 13.1 | - | - | ok |
| reduce_add | pyx-jit | 3.4 | 3.3 | - | - | ok |
| reduce_add | pyx-aot | 2.6 | 2.6 | 85 | - | ok |
| math_loop | cpython | 13.0 | 12.1 | - | - | ok |
| math_loop | pyx-jit | 10.3 | 9.1 | - | - | ok |
| math_loop | pyx-aot | 8.4 | 8.0 | 86 | - | ok |
| json_dumps | cpython | 16.2 | 14.6 | - | - | ok |
| json_dumps | pyx-jit | 3.4 | 3.1 | - | - | ok |
| json_dumps | pyx-aot | 2.0 | 1.9 | 84 | - | ok |
| json_loads | cpython | 15.5 | 14.1 | - | - | ok |
| json_loads | pyx-jit | 2.9 | 2.7 | - | - | ok |
| json_loads | pyx-aot | 1.8 | 1.8 | 87 | - | ok |
| threads_serial | cpython | 58.6 | 55.1 | - | - | ok |
| threads_serial | pyx-jit | 5.1 | 5.0 | - | - | ok |
| threads_serial | pyx-aot | 3.9 | 3.7 | 86 | - | ok |
| threads_parallel | cpython | 45.2 | 42.0 | - | - | ok |
| threads_parallel | pyx-jit | 7.1 | 6.5 | - | - | ok |
| threads_parallel | pyx-aot | 4.1 | 3.7 | 91 | - | ok |
| pool_map | cpython | - | - | - | - | error |
| pool_serial | cpython | - | - | - | - | error |
| pool_parallel | cpython | - | - | - | - | error |
| process_queue | cpython | - | - | - | - | error |
| startup_hello | cpython | 11.6 | 11.2 | - | - | ok |
| startup_hello | pyx-jit | 2.1 | 1.7 | - | - | ok |
| startup_hello | pyx-aot | 1.0 | 0.8 | 111 | - | ok |

Baseline for the import-free PyX subset; dicts use a hash index
(insertion order preserved), while string concatenation is still
O(n²), so that load stays small. Not comparable to the CPython/uv
harnesses in benchmarks/.
