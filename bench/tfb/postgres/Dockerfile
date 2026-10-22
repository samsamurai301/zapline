# The TechEmpower "hello_world" database: World (10,000 rows) and Fortune.
FROM postgres:17-bookworm
ENV POSTGRES_USER=benchmarkdbuser \
    POSTGRES_PASSWORD=benchmarkdbpass \
    POSTGRES_DB=hello_world
COPY create.sql /docker-entrypoint-initdb.d/
COPY postgresql.conf /etc/postgresql/postgresql.conf
CMD ["postgres", "-c", "config_file=/etc/postgresql/postgresql.conf"]
