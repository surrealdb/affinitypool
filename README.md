# affinitypool

A threadpool for running blocking jobs on a dedicated thread pool. Blocking tasks can be sent asynchronously to the pool, where the task will be queued until a worker thread is free to process the task. Tasks are processed in a FIFO order.

For optimised workloads, the affinity of each thread can be specified, ensuring that each thread can request to be pinned to a certain CPU core, allowing for more parallelism, and better performance guarantees for blocking workloads.

#### Original

This code is heavily inspired by [threadpool](https://crates.io/crates/threadpool), licensed under the Apache License 2.0 and MIT licenses.
