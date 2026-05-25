---
name: docker
description: "Build images, run and manage containers"
requires:
  bins: ["docker"]
---

# Docker

Use `docker compose` (v2), not `docker-compose` (v1).

## Containers

```bash
docker ps [-a]                            # running / all
docker run -d --name myapp -p 8080:80 nginx
docker stop|rm myapp
docker exec -it myapp /bin/sh
```

## Images

```bash
docker images
docker build -t myapp:latest [-f Dockerfile.prod] .
docker rmi myapp:latest
docker pull postgres:16
```

## Logs / inspect

```bash
docker logs myapp [--tail 50 -f]
docker inspect myapp --format '{{.State.Status}}'
docker stats --no-stream
```

## Compose

```bash
docker compose up -d
docker compose down
docker compose ps
docker compose logs api --tail 100
docker compose build
docker compose exec api /bin/sh
```

## Cleanup

```bash
docker system df                          # check before prune
docker system prune -f
docker volume ls | rm mydata
docker network ls
```
