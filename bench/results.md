| test | zapline | axum | actix-web |
|---|---:|---:|---:|
| plaintext | 277311 req/s (741.13us) | 165496 req/s (1.50ms) | 244669 req/s (0.93ms) |
| json | 242054 req/s (0.96ms) | 175400 req/s (1.43ms) | 225455 req/s (1.03ms) |
| path-param | 229457 req/s (1.04ms) | 152483 req/s (1.67ms) | 192076 req/s (1.26ms) |
| plaintext-pipelined-16 | 1672525 req/s (1.42ms) | 92298 req/s (24.46ms) | 1215920 req/s (1.91ms) |

_2 server workers on CPUs 0,1; wrk 2 threads / 256 connections on CPUs 2,3; best of 3 × 10s; 4 CPUs total, Linux 6.18.44-fc-v139._
