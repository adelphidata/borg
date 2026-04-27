---
name: docker
description: "Build images, run and manage containers"
requires:
  bins: ["docker"]
---

# Docker Skill

Use `docker` and `docker compose` for container management.

## Container Operations

```bash
docker ps                                # running containers
docker ps -a                             # all containers including stopped
docker run -d --name myapp -p 8080:80 nginx
docker stop myapp
docker rm myapp
docker exec -it myapp /bin/sh            # shell into running container
```

## Images

```bash
docker images
docker build -t myapp:latest .
docker build -t myapp:latest -f Dockerfile.prod .
docker rmi myapp:latest
docker pull postgres:16
```

## Logs & Inspection

```bash
docker logs myapp
docker logs myapp --tail 50 -f           # last 50 lines, follow
docker inspect myapp --format '{{.State.Status}}'
docker stats --no-stream                 # resource usage snapshot
```

## Docker Compose

```bash
docker compose up -d                     # start stack in background
docker compose down                      # stop and remove stack
docker compose ps                        # list services
docker compose logs api --tail 100       # logs for specific service
docker compose build                     # rebuild images
docker compose exec api /bin/sh          # shell into service
```

## Cleanup

```bash
docker system prune -f                   # remove unused data
docker volume ls                         # list volumes
docker volume rm mydata                  # remove specific volume
docker network ls                        # list networks
```

## Notes

- Use `docker compose` (v2) not `docker-compose` (v1)
- Always use `-d` for background when starting services
- Use `--tail` with logs to avoid overwhelming output
- Use `docker system df` to check disk usage before pruning
