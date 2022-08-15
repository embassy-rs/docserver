#!/bin/bash

set -euxo pipefail

cargo build --release --target x86_64-unknown-linux-musl

rm -rf docker/rootfs
mkdir docker/rootfs

cp target/x86_64-unknown-linux-musl/release/server docker/rootfs
cp -r templates docker/rootfs

IMAGE=embassy.dev/docserver:$(date '+%Y%m%d%H%M%S')

docker build -t $IMAGE docker

docker save $IMAGE | pv | ssh root@docs.embassy.dev -- ctr -n=k8s.io images import /dev/stdin
sed "s@\$IMAGE@$IMAGE@g" deploy.yaml | ssh root@docs.embassy.dev -- kubectl apply -f -
