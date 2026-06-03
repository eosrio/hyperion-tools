# Spring 1.2.2 nodeos for the cc32d9 reference stack: loads the Libre snapshot, peers to advance a
# few blocks, and serves SHiP (state_history) so Chronicle can read the snapshot's full-state delta.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates net-tools procps && rm -rf /var/lib/apt/lists/*

ARG SPRING_DEB=https://github.com/AntelopeIO/spring/releases/download/v1.2.2/antelope-spring_1.2.2_amd64.deb
RUN curl -fL -o /tmp/spring.deb "$SPRING_DEB" \
    && apt-get update && apt-get install -y /tmp/spring.deb \
    && rm /tmp/spring.deb && rm -rf /var/lib/apt/lists/*

COPY config.ini /etc/nodeos/config.ini.template
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 8080 8888 9876
ENTRYPOINT ["/entrypoint.sh"]
