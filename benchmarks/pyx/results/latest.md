# PyX Micro-Benchmark Baseline

_2026-10-24 15:45:52 · Linux-7.0.0-38-generic-x86_64-with-glibc2.39 · x86_64 · repeats=5_

| workload | impl | median ms | min ms | build ms | RSS MB | status |
|---|---|---:|---:|---:|---:|---|
| while_sum | cpython | 50.7 | 47.9 | - | 10.3 | ok |
| while_sum | pyx-jit | 13.1 | 13.0 | - | 7.8 | ok |
| while_sum | pyx-aot | 10.4 | 10.1 | 309 | 3.5 | ok |
| for_sum | cpython | 44.2 | 43.8 | - | 10.3 | ok |
| for_sum | pyx-jit | 17.8 | 17.3 | - | 7.8 | ok |
| for_sum | pyx-aot | 14.4 | 13.5 | 297 | 3.6 | ok |
| range_sum | cpython | 26.1 | 25.3 | - | 10.5 | ok |
| range_sum | pyx-jit | 10.0 | 9.7 | - | 7.9 | ok |
| range_sum | pyx-aot | 8.5 | 7.9 | 255 | 3.4 | ok |
| list_append | cpython | 38.8 | 37.5 | - | 14.2 | ok |
| list_append | pyx-jit | 18.8 | 18.2 | - | 8.7 | ok |
| list_append | pyx-aot | 16.6 | 16.1 | 247 | 4.4 | ok |
| dict_insert | cpython | 24.3 | 21.4 | - | 10.9 | ok |
| dict_insert | pyx-jit | 68.6 | 62.5 | - | 8.1 | ok |
| dict_insert | pyx-aot | 58.2 | 56.3 | 241 | 3.6 | ok |
| string_concat | cpython | 25.7 | 21.9 | - | 10.4 | ok |
| string_concat | pyx-jit | 17.1 | 15.0 | - | 21.2 | ok |
| string_concat | pyx-aot | 13.0 | 12.2 | 244 | 16.8 | ok |
| list_comp | cpython | 29.7 | 27.3 | - | 14.2 | ok |
| list_comp | pyx-jit | 11.2 | 10.1 | - | 8.7 | ok |
| list_comp | pyx-aot | 7.7 | 7.4 | 268 | 4.6 | ok |
| filter_comp | cpython | 29.2 | 28.0 | - | 11.7 | ok |
| filter_comp | pyx-jit | 12.5 | 11.9 | - | 8.2 | ok |
| filter_comp | pyx-aot | 10.4 | 10.1 | 261 | 4.0 | ok |
| sum_genexp | cpython | 28.3 | 26.5 | - | 10.5 | ok |
| sum_genexp | pyx-jit | 13.0 | 12.3 | - | 8.8 | ok |
| sum_genexp | pyx-aot | 8.6 | 8.1 | 287 | 4.4 | ok |
| map_lambda | cpython | 32.7 | 32.5 | - | 14.2 | ok |
| map_lambda | pyx-jit | 16.7 | 16.3 | - | 9.3 | ok |
| map_lambda | pyx-aot | 14.9 | 14.6 | 251 | 5.2 | ok |
| filter_lambda | cpython | 29.8 | 28.4 | - | 11.7 | ok |
| filter_lambda | pyx-jit | 17.1 | 16.7 | - | 8.4 | ok |
| filter_lambda | pyx-aot | 17.0 | 15.3 | 271 | 4.2 | ok |
| lambda_call | cpython | 35.6 | 35.3 | - | 10.4 | ok |
| lambda_call | pyx-jit | 19.5 | 17.8 | - | 7.9 | ok |
| lambda_call | pyx-aot | 16.0 | 15.7 | 245 | 3.5 | ok |
| reduce_add | cpython | 24.8 | 24.5 | - | 10.3 | ok |
| reduce_add | pyx-jit | 9.9 | 8.9 | - | 7.8 | ok |
| reduce_add | pyx-aot | 7.1 | 7.1 | 241 | 3.8 | ok |
| math_loop | cpython | 29.5 | 28.7 | - | 10.6 | ok |
| math_loop | pyx-jit | 25.3 | 23.8 | - | 9.4 | ok |
| math_loop | pyx-aot | 25.7 | 23.8 | 250 | 5.1 | ok |
| json_dumps | cpython | 28.8 | 26.6 | - | 11.3 | ok |
| json_dumps | pyx-jit | 6.0 | 5.6 | - | 8.7 | ok |
| json_dumps | pyx-aot | 3.5 | 3.3 | 236 | 4.7 | ok |
| json_loads | cpython | 29.6 | 29.1 | - | 11.4 | ok |
| json_loads | pyx-jit | 6.2 | 5.6 | - | 9.0 | ok |
| json_loads | pyx-aot | 3.8 | 3.4 | 269 | 4.8 | ok |
| threads_serial | cpython | 126.2 | 119.0 | - | 10.3 | ok |
| threads_serial | pyx-jit | 40.4 | 38.6 | - | 8.0 | ok |
| threads_serial | pyx-aot | 33.3 | 32.7 | 258 | 3.5 | ok |
| threads_parallel | cpython | 72.8 | 70.8 | - | 10.6 | ok |
| threads_parallel | pyx-jit | 18.7 | 17.3 | - | 8.4 | ok |
| threads_parallel | pyx-aot | 12.0 | 11.7 | 245 | 3.9 | ok |
| pool_map | cpython | 64.6 | 60.9 | - | 14.5 | ok |
| pool_map | pyx-jit | 11.5 | 9.7 | - | 8.2 | ok |
| pool_map | pyx-aot | 7.3 | 5.8 | 253 | 3.7 | ok |
| pool_serial | cpython | 116.3 | 112.6 | - | 14.5 | ok |
| pool_serial | pyx-jit | 51.7 | 48.9 | - | 8.2 | ok |
| pool_serial | pyx-aot | 50.5 | 47.2 | 238 | 3.8 | ok |
| pool_parallel | cpython | 78.8 | 75.2 | - | 14.3 | ok |
| pool_parallel | pyx-jit | 18.6 | 17.9 | - | 8.1 | ok |
| pool_parallel | pyx-aot | 15.2 | 14.6 | 237 | 3.7 | ok |
| process_queue | cpython | 64.7 | 63.5 | - | 13.9 | ok |
| process_queue | pyx-jit | 19.7 | 18.6 | - | 8.4 | ok |
| process_queue | pyx-aot | 14.6 | 13.4 | 255 | 3.8 | ok |
| startup_hello | cpython | 19.7 | 19.0 | - | 10.3 | ok |
| startup_hello | pyx-jit | 3.1 | 2.7 | - | 7.7 | ok |
| startup_hello | pyx-aot | 1.7 | 1.1 | 254 | 3.4 | ok |

Early baseline for the import-free PyX subset; dict insertion is
linear (O(n)) and string concatenation is O(n²), so both use small
loads. Not comparable to the CPython/uv harnesses in benchmarks/.
