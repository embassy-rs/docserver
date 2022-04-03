curl -sfL https://get.k3s.io | sh -

cat > /etc/sysctl.d/ports.conf <<EOF
net.ipv4.ip_unprivileged_port_start=0
EOF

cat > /var/lib/rancher/k3s/server/manifests/traefik-config.yaml <<EOF
apiVersion: helm.cattle.io/v1
kind: HelmChartConfig
metadata:
  name: traefik
  namespace: kube-system
spec:
  valuesContent: |-
    additionalArguments:
    - --certificatesresolvers.letsencrypt.acme.email=letsencrypt@dirbaio.net
    - --certificatesresolvers.letsencrypt.acme.storage=/data/acme.json
    - --certificatesresolvers.letsencrypt.acme.caserver=https://acme-v02.api.letsencrypt.org/directory
    - --certificatesResolvers.letsencrypt.acme.tlschallenge=true
    - --entrypoints.websecure.http.tls.certResolver=letsencrypt
    - --entrypoints.web.http.redirections.entrypoint.to=websecure
    - --entrypoints.web.http.redirections.entrypoint.scheme=https
    - --log.level=DEBUG
    - --accesslog=true
    persistence:
      enabled: true
      accessMode: ReadWriteOnce
      size: 128Mi
      storageClass: local-path
      path: /data
      annotations: {}
    service:
      enabled: false
    hostNetwork: true
    ports:
      web:
        port: 80
      websecure:
        port: 443
EOF