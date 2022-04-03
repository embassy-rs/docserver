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

ssh root@docs.embassy.dev -- kubectl apply -f - <<EOF
---
apiVersion: v1
kind: Service
metadata:
  name: docserver

spec:
  ports:
    - protocol: TCP
      name: web
      port: 3000
  selector:
    app: docserver

---
kind: Deployment
apiVersion: apps/v1
metadata:
  namespace: default
  name: docserver
  labels:
    app: docserver

spec:
  replicas: 2
  selector:
    matchLabels:
      app: docserver
  template:
    metadata:
      labels:
        app: docserver
    spec:
      containers:
      - name: docserver
        image: $IMAGE
        imagePullPolicy: Never
        ports:
        - name: web
          containerPort: 3000
        env:
        - name: DOCSERVER_STATIC_PATH
          value: /data/static
        - name: DOCSERVER_CRATES_PATH
          value: /data/crates
        volumeMounts:
        - name: data
          mountPath: /data
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: docserver

---
apiVersion: traefik.containo.us/v1alpha1
kind: IngressRoute
metadata:
  name: docserver
  namespace: default
spec:
  entryPoints:
    - websecure
  routes:
  - match: Host("docs.embassy.dev")
    kind: Rule
    services:
    - name: docserver
      port: 3000

---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: docserver
spec:
  accessModes:
  - ReadWriteOnce
  resources:
    requests:
      storage: 128Mi
  storageClassName: local-path

EOF