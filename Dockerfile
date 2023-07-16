FROM selenium/node-chrome:latest as build

USER root

RUN apt-get update && apt-get install -y build-essential

RUN /bin/bash -c "curl https://sh.rustup.rs -sSf | bash -s -- -y"

COPY . /src

RUN /bin/bash -c "source \"\${HOME}/.cargo/env\" && cd /src && cargo build --release"

FROM selenium/node-chrome:latest

USER root

RUN apt-get update && apt-get install -y git

COPY --from=build /src/target/release/fco-backup-fetcher /fco-backup-fetcher
