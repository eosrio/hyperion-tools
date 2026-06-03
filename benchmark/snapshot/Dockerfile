FROM ubuntu:22.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates tar zstd coreutils && rm -rf /var/lib/apt/lists/*
COPY fetch.sh /fetch.sh
RUN chmod +x /fetch.sh
ENTRYPOINT ["bash", "/fetch.sh"]
