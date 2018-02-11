#!/bin/bash -eu

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

image="$(docker build "${script_dir}/docker/build" | awk '$0 ~ /^Successfully built / {print $3}')"
name="$(uuidgen)"
docker run --name="${name}" -v "${script_dir}:/src" -w /src "${image}" cargo build --release
docker cp "${name}:/src/target/release/fco-backup-fetcher" "${script_dir}/docker/run/fco-backup-fetcher"
echo >&2 "Stopping build container: $(docker stop "${name}" || :)"
echo >&2 "Removing build container $(docker rm "${name}")"
