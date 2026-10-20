| test | zapline | axum | actix-web |
|---|---:|---:|---:|
| plaintext | 238393 req/s (0.93ms) | 158897 req/s (1.59ms) | 232987 req/s (0.99ms) |
| json | 230154 req/s (1.00ms) | 142491 req/s (1.79ms) | 226023 req/s (1.04ms) |
| path-param | 238642 req/s (0.98ms) | 153838 req/s (1.76ms) | 179485 req/s (1.40ms) |
| plaintext-pipelined-16 | 1597907 req/s (1.47ms) | 92241 req/s (24.50ms) | 1221584 req/s (1.91ms) |

_2 server workers on CPUs 0,1; wrk 2 threads / 256 connections on CPUs 2,3; best of 3 × 10s; 4 CPUs total, Linux 6.18.44-fc-v139._
