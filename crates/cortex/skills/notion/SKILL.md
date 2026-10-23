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
install:
  curl:
    brew: curl
    apt: curl
  jq:
    brew: jq
    apt: jq
---

# Notion

REST API. Create an integration at https://notion.so/my-integrations, export
the secret as `NOTION_API_KEY`, and share each target page/DB with it via
"•••" → "Connect to" → integration. Without that share you'll get
`object_not_found`.

`Notion-Version: 2025-09-03` is required. Under this version, databases
expose both a `database_id` (parent for new pages) and a `data_source_id`
(for queries — find via `search` or `GET /v1/databases/{id}`).

Wrap requests so you can embed user input safely (`jq -n --arg`, not
heredoc concat):

```bash
notion() {
  curl -sS "$@" \
    -H "Authorization: Bearer $NOTION_API_KEY" \
    -H "Notion-Version: 2025-09-03" \
    -H "Content-Type: application/json"
}
notion -X POST https://api.notion.com/v1/search \
  -d "$(jq -n --arg q "$query" '{query:$q}')"
```

## Search

```bash
notion -X POST https://api.notion.com/v1/search -d '{"query":"roadmap"}'
notion -X POST https://api.notion.com/v1/search \
  -d '{"query":"","filter":{"property":"object","value":"data_source"}}'
```

## Read

```bash
notion https://api.notion.com/v1/pages/PAGE_ID
notion "https://api.notion.com/v1/blocks/PAGE_ID/children?page_size=100"
```

## Append blocks (≤100 children, ≤2 nesting levels, ~500KB body)

```bash
notion -X PATCH https://api.notion.com/v1/blocks/PAGE_ID/children -d '{
  "children":[
    {"object":"block","type":"heading_2","heading_2":{"rich_text":[{"text":{"content":"Notes"}}]}},
    {"object":"block","type":"paragraph","paragraph":{"rich_text":[{"text":{"content":"Hello"}}]}},
    {"object":"block","type":"to_do","to_do":{"rich_text":[{"text":{"content":"Follow up"}}],"checked":false}}
  ]
}'
```

## Create page in DB / query / update

```bash
notion -X POST https://api.notion.com/v1/pages -d '{
  "parent":{"database_id":"DB_ID"},
  "properties":{
    "Name":{"title":[{"text":{"content":"New task"}}]},
    "Status":{"select":{"name":"Todo"}},
    "Due":{"date":{"start":"2026-05-12"}}
  }
}'

notion -X POST https://api.notion.com/v1/data_sources/DS_ID/query -d '{
  "filter":{"property":"Status","select":{"equals":"In Progress"}},
  "sorts":[{"property":"Due","direction":"ascending"}],
  "page_size":50
}'

notion -X PATCH https://api.notion.com/v1/pages/PAGE_ID \
  -d '{"properties":{"Status":{"select":{"name":"Done"}}}}'
```

## Property shapes

| Type | Shape |
|---|---|
| Title | `{"title":[{"text":{"content":"…"}}]}` |
| Rich text | `{"rich_text":[{"text":{"content":"…"}}]}` |
| Select | `{"select":{"name":"X"}}` |
| Multi-select | `{"multi_select":[{"name":"A"},{"name":"B"}]}` |
| Date | `{"date":{"start":"2026-05-05","end":null}}` |
| Checkbox / Number | `{"checkbox":true}` / `{"number":42}` |
| URL / Email | `{"url":"…"}` / `{"email":"…"}` |
| Relation | `{"relation":[{"id":"PAGE_ID"}]}` |

IDs are UUIDs (dashes optional). Rate limit ~3/s — honor `Retry-After` on
429. Pagination via `has_more` / `next_cursor` / `start_cursor`. DB view
filters are UI-only.
