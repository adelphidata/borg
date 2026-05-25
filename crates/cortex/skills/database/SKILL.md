---
name: database
description: "Query SQLite and PostgreSQL databases via CLI"
requires:
  bins: ["sqlite3"]
---

# Database

`sqlite3` for SQLite, `psql` for PostgreSQL. For psql passwords, set
`PGPASSWORD` or use `~/.pgpass`.

## SQLite

```bash
sqlite3 mydb.db "SELECT * FROM users LIMIT 10;"
sqlite3 -header -column mydb.db "SELECT id, name FROM users;"
sqlite3 -header -csv mydb.db "SELECT * FROM users;" > users.csv

# Schema
sqlite3 mydb.db ".tables"
sqlite3 mydb.db ".schema users"
sqlite3 mydb.db "PRAGMA table_info(users);"
```

## PostgreSQL

```bash
psql -h localhost -U postgres -d mydb -c "SELECT * FROM users LIMIT 10;"
psql "postgresql://user:pass@localhost:5432/mydb" -c "SELECT 1;"

# Schema
psql -d mydb -c "\dt"           # tables
psql -d mydb -c "\d users"      # describe
psql -d mydb -c "\l"            # databases

# CSV
psql -d mydb -c "COPY (SELECT * FROM users) TO STDOUT WITH CSV HEADER;" > users.csv
```
