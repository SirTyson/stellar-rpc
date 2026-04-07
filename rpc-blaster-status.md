# RPC Blaster Status

## 2026-04-07

- **Optimization**: Skip debug-only response marshaling in `cmd/stellar-rpc/internal/jsonrpc.go` when `--log-level` is `info`
- **Benchmark basis**: full 10/25/50/75/100/150/200 RPS sweep plus warm-cache reruns at 50 and 100 RPS
- **Controlled 100 RPS result**: p50 `16.143 ms -> 15.175 ms` (**6.0%**), p95 `50.783 ms -> 44.863 ms` (**11.66%**), p99 `58.463 ms -> 52.735 ms` (**9.8%**)
- **Controlled 50 RPS result**: p50 `14.511 ms -> 13.343 ms` (**8.05%**), p95 `48.063 ms -> 41.567 ms` (**13.52%**), p99 `56.031 ms -> 48.991 ms` (**12.56%**)
- **Throughput ceiling**: `100 RPS -> 100 RPS` with zero errors under the objective's p50 step-up criterion
