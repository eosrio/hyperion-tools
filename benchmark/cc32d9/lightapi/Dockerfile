# cc32d9 eosio_light_api reference stack in one container (mirrors their single-LXC deployment):
# MariaDB + Chronicle receiver + Perl DB writer + Starman HTTP API. Chronicle reaches nodeos SHiP
# over the compose network.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
      curl ca-certificates git procps netcat-openbsd \
      gcc make cpanminus \
      libmariadb-dev libmariadb-dev-compat libdbi-perl \
      libjson-xs-perl libjson-perl libdatetime-format-iso8601-perl \
      mariadb-server \
    && rm -rf /var/lib/apt/lists/*

# Perl deps not packaged: DB driver, HTTP server, websocket server, key checksum.
RUN for m in DBD::MariaDB Starman Net::WebSocket::Server Crypt::Digest::RIPEMD160; do \
      cpanm --notest "$m"; \
    done

# Chronicle 3.3 (ubuntu 22.04 build).
ARG CHRONICLE_DEB=https://github.com/EOSChronicleProject/eos-chronicle/releases/download/v3.3/antelope-chronicle-3.3-Clang-11.0.1-ubuntu22.04-x86_64.deb
RUN curl -fL -o /tmp/chronicle.deb "$CHRONICLE_DEB" \
    && apt-get update && apt-get install -y /tmp/chronicle.deb \
    && rm /tmp/chronicle.deb && rm -rf /var/lib/apt/lists/*

# The cc32d9 application (Perl writer, PSGI API, SQL schema, per-network setup scripts).
RUN git clone --depth 1 https://github.com/cc32d9/eosio_light_api.git /opt/eosio_light_api

COPY chronicle-config.ini.template /opt/chronicle-config.ini.template
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 5001
ENTRYPOINT ["/entrypoint.sh"]
