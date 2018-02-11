FROM alpine:3.7

RUN echo http://dl-cdn.alpinelinux.org/alpine/edge/testing >> /etc/apk/repositories

RUN apk --update add firefox git openssh-client openssl-dev

RUN wget https://github.com/mozilla/geckodriver/releases/download/v0.19.1/geckodriver-v0.19.1-linux64.tar.gz -O - | tar xzC /usr/local/bin

ENV MOZ_HEADLESS=1

COPY fco-backup-fetcher /fco-backup-fetcher

VOLUME /root/.ssh
