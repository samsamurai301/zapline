# wrk, the load generator TechEmpower uses. Build context: bench/.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends wrk ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY pipeline.lua /pipeline.lua
ENTRYPOINT ["wrk"]
