---
name: notion
description: "Read and write Notion pages, databases, and blocks via the REST API"
triggers:
  - "create a notion page"
  - "search my notion"
  - "update notion database"
  - "append to notion page"
requires:
  bins: ["curl", "jq"]
  env: ["NOTION_API_KEY"]
---

# Notion Skill

Drive Notion via its REST API. Create an integration at https://notion.so/my-integrations, copy the secret (starts with `ntn_` or `secret_`), and export it as `NOTION_API_KEY`. Then in Notion, share each target page or database with the integration ("•••" → "Connect to" → integration name) — without that share, the API returns `object_not_found`.

The `Notion-Version` header is required on every call. This skill targets `2025-09-03`, where databases are exposed as **data sources**: each database has both a `database_id` (used as a parent when creating pages) and a `data_source_id` (used to query rows).

## Helper

```bash
notion() {
  curl -sS "$@" \
    -H "Authorization: Bearer $NOTION_API_KEY" \
    -H "Notion-Version: 2025-09-03" \
    -H "Content-Type: application/json"
}
```

When a request body embeds a value the user supplied (search query, page title, property text), build the JSON with `jq -n --arg ...` rather than string-concatenating into a heredoc — otherwise quotes or backslashes in the input break the payload.

```bash
notion -X POST https://api.notion.com/v1/search \
  -d "$(jq -n --arg q "$query" '{query:$q}')"
```

## Search

```bash
# Search all shared pages/databases by title
notion -X POST https://api.notion.com/v1/search \
  -d '{"query":"roadmap"}' | jq '.results[] | {object, id, title: (.properties.Name.title[0].plain_text // .title[0].plain_text)}'

# Filter to databases (data sources) only
notion -X POST https://api.notion.com/v1/search \
  -d '{"query":"","filter":{"property":"object","value":"data_source"}}' | jq
```

## Read a Page

```bash
# Page metadata + properties
notion https://api.notion.com/v1/pages/PAGE_ID | jq

# Page body (blocks)
notion "https://api.notion.com/v1/blocks/PAGE_ID/children?page_size=100" | jq '.results[] | {type, id}'
```

## Append Blocks

```bash
notion -X PATCH https://api.notion.com/v1/blocks/PAGE_ID/children -d '{
  "children":[
    {"object":"block","type":"heading_2","heading_2":{"rich_text":[{"text":{"content":"Notes"}}]}},
    {"object":"block","type":"paragraph","paragraph":{"rich_text":[{"text":{"content":"Hello from Borg."}}]}},
    {"object":"block","type":"to_do","to_do":{"rich_text":[{"text":{"content":"Follow up"}}],"checked":false}}
  ]
}'
```

Up to 100 children per call, two levels of nesting per request, ~500KB payload cap.

## Create a Page in a Database

```bash
notion -X POST https://api.notion.com/v1/pages -d '{
  "parent":{"database_id":"DATABASE_ID"},
  "properties":{
    "Name":{"title":[{"text":{"content":"New task"}}]},
    "Status":{"select":{"name":"Todo"}},
    "Due":{"date":{"start":"2026-05-12"}}
  }
}'
```

## Query a Database

Queries use the `data_source_id` (not the database id). Find it via `search` or `GET /v1/databases/{id}`.

```bash
notion -X POST https://api.notion.com/v1/data_sources/DATA_SOURCE_ID/query -d '{
  "filter":{"property":"Status","select":{"equals":"In Progress"}},
  "sorts":[{"property":"Due","direction":"ascending"}],
  "page_size":50
}' | jq '.results[] | {id, name: .properties.Name.title[0].plain_text}'
```

## Update Page Properties

```bash
notion -X PATCH https://api.notion.com/v1/pages/PAGE_ID -d '{
  "properties":{"Status":{"select":{"name":"Done"}}}
}'
```

## Property Shapes (cheat sheet)

- Title: `{"title":[{"text":{"content":"..."}}]}`
- Rich text: `{"rich_text":[{"text":{"content":"..."}}]}`
- Select: `{"select":{"name":"Option"}}`
- Multi-select: `{"multi_select":[{"name":"A"},{"name":"B"}]}`
- Date: `{"date":{"start":"2026-05-05","end":null}}`
- Checkbox: `{"checkbox":true}`
- Number: `{"number":42}`
- URL / Email: `{"url":"https://..."}` / `{"email":"a@b.com"}`
- Relation: `{"relation":[{"id":"PAGE_ID"}]}`

## Notes

- IDs are UUIDs — dashes optional.
- Rate limit ~3 req/s; on `429`, honor `Retry-After`.
- Pagination: responses include `has_more` and `next_cursor`; pass `start_cursor=...` to continue.
- Database view filters cannot be set via API — that's UI-only.
- If a request returns `object_not_found`, the integration almost certainly hasn't been shared with that page or database.
