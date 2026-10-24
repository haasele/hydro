# PyX Micro-Benchmark Baseline

_2026-10-24 20:29:58 · Linux-7.2.11-1-cachyos-x86_64-with-glibc2.44 · x86_64 · repeats=5_

| workload | impl | median ms | min ms | build ms | RSS MB | status |
|---|---|---:|---:|---:|---:|---|
| while_sum | cpython | 23.5 | 22.9 | - | - | ok |
| while_sum | pyx-jit | 3.2 | 2.7 | - | - | ok |
| while_sum | pyx-aot | 1.7 | 1.7 | 89 | - | ok |
| for_sum | cpython | 19.3 | 18.7 | - | - | ok |
| for_sum | pyx-jit | 2.4 | 2.4 | - | - | ok |
| for_sum | pyx-aot | 1.4 | 1.2 | 87 | - | ok |
| range_sum | cpython | 10.1 | 9.7 | - | - | ok |
| range_sum | pyx-jit | 3.2 | 3.1 | - | - | ok |
| range_sum | pyx-aot | 3.0 | 2.7 | 85 | - | ok |
| list_append | cpython | 16.7 | 16.2 | - | - | ok |
| list_append | pyx-jit | 4.5 | 4.1 | - | - | ok |
| list_append | pyx-aot | 4.0 | 3.8 | 82 | - | ok |
| dict_insert | cpython | 9.0 | 8.3 | - | - | ok |
| dict_insert | pyx-jit | 2.7 | 2.3 | - | - | ok |
| dict_insert | pyx-aot | 1.5 | 1.3 | 88 | - | ok |
| string_concat | cpython | 9.5 | 8.5 | - | - | ok |
| string_concat | pyx-jit | 7.5 | 7.1 | - | - | ok |
| string_concat | pyx-aot | 6.2 | 5.1 | 87 | - | ok |
| list_comp | cpython | 14.1 | 12.9 | - | - | ok |
| list_comp | pyx-jit | 3.7 | 3.4 | - | - | ok |
| list_comp | pyx-aot | 2.5 | 2.4 | 80 | - | ok |
| filter_comp | cpython | 12.2 | 10.8 | - | - | ok |
| filter_comp | pyx-jit | 3.8 | 3.6 | - | - | ok |
| filter_comp | pyx-aot | 3.0 | 2.6 | 91 | - | ok |
| sum_genexp | cpython | 12.0 | 11.9 | - | - | ok |
| sum_genexp | pyx-jit | 3.9 | 3.7 | - | - | ok |
| sum_genexp | pyx-aot | 3.2 | 3.0 | 88 | - | ok |
| map_lambda | cpython | 15.9 | 14.7 | - | - | ok |
| map_lambda | pyx-jit | 7.4 | 6.5 | - | - | ok |
| map_lambda | pyx-aot | 5.9 | 5.2 | 85 | - | ok |
| filter_lambda | cpython | 13.2 | 11.8 | - | - | ok |
| filter_lambda | pyx-jit | 6.0 | 5.6 | - | - | ok |
| filter_lambda | pyx-aot | 4.9 | 4.8 | 90 | - | ok |
| lambda_call | cpython | 15.2 | 14.6 | - | - | ok |
| lambda_call | pyx-jit | 4.6 | 4.4 | - | - | ok |
| lambda_call | pyx-aot | 3.9 | 3.6 | 92 | - | ok |
| reduce_add | cpython | 12.7 | 11.8 | - | - | ok |
| reduce_add | pyx-jit | 4.0 | 3.3 | - | - | ok |
| reduce_add | pyx-aot | 3.1 | 2.9 | 81 | - | ok |
| math_loop | cpython | 13.6 | 12.5 | - | - | ok |
| math_loop | pyx-jit | 7.4 | 6.8 | - | - | ok |
| math_loop | pyx-aot | 6.2 | 5.8 | 95 | - | ok |
| json_dumps | cpython | 15.0 | 14.4 | - | - | ok |
| json_dumps | pyx-jit | 3.3 | 3.0 | - | - | ok |
| json_dumps | pyx-aot | 1.9 | 1.8 | 88 | - | ok |
| json_loads | cpython | 15.4 | 14.7 | - | - | ok |
| json_loads | pyx-jit | 2.7 | 2.7 | - | - | ok |
| json_loads | pyx-aot | 1.9 | 1.6 | 89 | - | ok |
| threads_serial | cpython | 58.4 | 55.9 | - | - | ok |
| threads_serial | pyx-jit | 4.6 | 4.2 | - | - | ok |
| threads_serial | pyx-aot | 3.9 | 3.7 | 80 | - | ok |
| threads_parallel | cpython | 36.6 | 35.5 | - | - | ok |
| threads_parallel | pyx-jit | 6.1 | 5.6 | - | - | ok |
| threads_parallel | pyx-aot | 2.8 | 2.5 | 85 | - | ok |
| pool_map | cpython | - | - | - | - | error |
| pool_serial | cpython | - | - | - | - | error |
| pool_parallel | cpython | - | - | - | - | error |
| process_queue | cpython | - | - | - | - | error |
| startup_hello | cpython | 9.3 | 8.7 | - | - | ok |
| startup_hello | pyx-jit | 1.7 | 1.3 | - | - | ok |
| startup_hello | pyx-aot | 0.9 | 0.8 | 89 | - | ok |

Baseline for the import-free PyX subset; dicts use a hash index
(insertion order preserved), while string concatenation is still
O(n²), so that load stays small. Not comparable to the CPython/uv
harnesses in benchmarks/.
