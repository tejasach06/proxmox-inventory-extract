FROM alpine:3
RUN apk add --no-cache ca-certificates
COPY proxmox-inventory-extract /usr/local/bin/proxmox-inventory-extract
ENTRYPOINT ["/usr/local/bin/proxmox-inventory-extract"]
