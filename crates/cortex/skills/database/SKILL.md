---
name: database
description: "Query SQLite and PostgreSQL databases via CLI"
requires:
  bins: ["sqlite3"]
---

# Database Skill

Use `sqlite3` for SQLite databases and `psql` for PostgreSQL.

## SQLite

```bash
# Open and query
sqlite3 mydb.db "SELECT * FROM users LIMIT 10;"

# Schema inspection
sqlite3 mydb.db ".tables"
sqlite3 mydb.db ".schema users"
sqlite3 mydb.db "PRAGMA table_info(users);"

# Formatted output
sqlite3 -header -column mydb.db "SELECT id, name, email FROM users;"

# CSV export
sqlite3 -header -csv mydb.db "SELECT * FROM users;" > users.csv
```

## PostgreSQL

```bash
# Connect and query
psql -h localhost -U postgres -d mydb -c "SELECT * FROM users LIMIT 10;"

# Schema inspection
psql -h localhost -U postgres -d mydb -c "\dt"           # list tables
psql -h localhost -U postgres -d mydb -c "\d users"      # describe table
psql -h localhost -U postgres -d mydb -c "\l"             # list databases

# CSV export
psql -h localhost -U postgres -d mydb \
  -c "COPY (SELECT * FROM users) TO STDOUT WITH CSV HEADER;" > users.csv
```

## Connection via URL

```bash
psql "postgresql://user:pass@localhost:5432/mydb" -c "SELECT 1;"
```

## Useful Queries

```bash
# Row counts
sqlite3 mydb.db "SELECT COUNT(*) FROM users;"

# Recent records
sqlite3 mydb.db "SELECT * FROM events ORDER BY created_at DESC LIMIT 20;"

# Table sizes (PostgreSQL)
psql -d mydb -c "SELECT relname, pg_size_pretty(pg_total_relation_size(relid)) FROM pg_catalog.pg_statio_user_tables ORDER BY pg_total_relation_size(relid) DESC;"
```

## Notes

- Use `-header -column` with sqlite3 for readable output
- For PostgreSQL, set `PGPASSWORD` env var or use `~/.pgpass` to avoid password prompts
- Always use `LIMIT` when exploring unfamiliar tables to avoid large result sets
- If `psql` is not available, the skill still works for SQLite operations
