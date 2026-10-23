---
name: linear
description: "Manage Linear issues, projects, and cycles via the GraphQL API"
requires:
  bins: ["curl", "jq"]
  env: ["LINEAR_API_KEY"]
install:
  curl:
    brew: curl
    apt: curl
  jq:
    brew: jq
    apt: jq
---

# Linear

GraphQL at `https://api.linear.app/graphql`. Auth is the raw key in
`Authorization` (no `Bearer`). Mutations take the **UUID** (`issue.id`);
display lookups use the **identifier** (`ENG-123`). Cache team/state IDs —
rate limit ~1500/hr per key. Schema:
https://developers.linear.app/docs/graphql/working-with-the-graphql-api.

```bash
linear_gql() {
  curl -sS -X POST https://api.linear.app/graphql \
    -H "Authorization: $LINEAR_API_KEY" \
    -H "Content-Type: application/json" \
    -d "$1"
}
```

## Identify

```bash
linear_gql '{"query":"{ viewer { id name email } }"}'
linear_gql '{"query":"{ teams { nodes { id key name } } }"}'
```

## Read

```bash
# My open issues
linear_gql '{"query":"{ viewer { assignedIssues(filter:{state:{type:{nin:[\"completed\",\"canceled\"]}}}) { nodes { identifier title state{name} url } } } }"}'

# By identifier
linear_gql '{"query":"query($id:String!){ issue(id:$id){ id identifier title description state{name} assignee{name} url } }","variables":{"id":"ENG-123"}}'

# Search
linear_gql '{"query":"query($q:String!){ issues(filter:{title:{containsIgnoreCase:$q}},first:20){ nodes{ identifier title url } } }","variables":{"q":"login bug"}}'
```

## Create

```bash
linear_gql '{
  "query":"mutation($i:IssueCreateInput!){ issueCreate(input:$i){ success issue{ identifier url } } }",
  "variables":{"i":{"teamId":"TEAM_UUID","title":"Investigate flaky test","description":"Repro:\n1. …"}}
}'
```

## Update / comment / state

```bash
# Update fields
linear_gql '{"query":"mutation($id:String!,$i:IssueUpdateInput!){ issueUpdate(id:$id,input:$i){ success } }","variables":{"id":"ISSUE_UUID","i":{"title":"New title"}}}'

# Comment
linear_gql '{"query":"mutation($i:CommentCreateInput!){ commentCreate(input:$i){ success comment{ url } } }","variables":{"i":{"issueId":"ISSUE_UUID","body":"Fixed in main"}}}'

# Workflow states (lookup before transitioning)
linear_gql '{"query":"query($k:String!){ team(id:$k){ states{ nodes{ id name type } } } }","variables":{"k":"TEAM_UUID"}}'
```

Pagination: connections accept `first` / `after`, return
`pageInfo { hasNextPage endCursor }`.
