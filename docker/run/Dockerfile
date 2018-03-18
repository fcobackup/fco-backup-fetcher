FROM selenium/node-chrome

USER root

RUN apt-get update && apt-get install -y git libssl-dev

COPY fco-backup-fetcher /fco-backup-fetcher

VOLUME /root/.ssh
