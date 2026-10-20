| test | zapline | axum | actix-web |
|---|---:|---:|---:|
| plaintext | 228593 req/s (0.98ms) | 145847 req/s (1.79ms) | 220445 req/s (1.06ms) |
| json | 242566 req/s (0.87ms) | 151467 req/s (1.68ms) | 214218 req/s (1.13ms) |
| path-param | 204376 req/s (1.12ms) | 78381 req/s (3.30ms) | 92920 req/s (2.89ms) |
| plaintext-pipelined-16 | 1441062 req/s (1.86ms) | 90909 req/s (24.57ms) | 1091187 req/s (2.35ms) |

_2 server workers on CPUs 0,1; wrk 2 threads / 256 connections on CPUs 2,3; best of 3 × 10s; 4 CPUs total, Linux 6.18.44-fc-v139._
