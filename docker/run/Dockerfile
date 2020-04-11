FROM selenium/node-chrome

USER root

RUN apt-get update && apt-get install -y git

COPY fco-backup-fetcher /fco-backup-fetcher

VOLUME /root/.ssh
