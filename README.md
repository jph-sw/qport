# qport

A lightweight Rust service that watches [gluetun](https://github.com/qdm12/gluetun)'s forwarded port file and automatically updates [qBittorrent](https://www.qbittorrent.org/)'s listen port via its Web API.

When you use a VPN with port forwarding (e.g. through gluetun), the forwarded port can change. qport detects those changes and keeps qBittorrent in sync without any manual intervention.

## How it works

1. On startup, reads the current port from the port file and applies it to qBittorrent.
2. Watches the directory containing the port file for changes.
3. When the file is created or modified, reads the new port and updates qBittorrent (retries up to 3 times with 5s delay).
4. Skips the update if the port hasn't changed.

## Usage with Docker Compose

The typical setup runs qport alongside gluetun and qBittorrent, sharing the gluetun volume so qport can read the forwarded port file.

```yaml
services:
  gluetun:
    image: qmcgaw/gluetun
    volumes:
      - gluetun_data:/gluetun
    # ... your VPN config

  qbittorrent:
    image: lscr.io/linuxserver/qbittorrent
    environment:
      - WEBUI_PORT=8080
    # ...

  qport:
    image: ghcr.io/jph-sw/qport:latest
    environment:
      PORT_FILE: /gluetun/forwarded_port
      QB_URL: http://qbittorrent:8080
      QB_USER: admin
      QB_PASS: adminadmin
    volumes:
      - gluetun_data:/gluetun:ro
    depends_on:
      - gluetun
      - qbittorrent
    restart: unless-stopped

volumes:
  gluetun_data:
```

## Environment variables

| Variable    | Default                   | Description                                           |
| ----------- | ------------------------- | ----------------------------------------------------- |
| `PORT_FILE` | `/gluetun/forwarded_port` | Path to the file containing the port number           |
| `QB_URL`    | `http://localhost:8080`   | qBittorrent Web UI base URL                           |
| `QB_USER`   | `admin`                   | qBittorrent username                                  |
| `QB_PASS`   | `adminadmin`              | qBittorrent password                                  |
| `RUST_LOG`  | `info`                    | Log level (`error`, `warn`, `info`, `debug`, `trace`) |

## Building the Docker image

```bash
docker build -t qport .
```
