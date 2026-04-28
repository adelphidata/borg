---
name: linear
description: "Manage Linear issues, projects, and cycles via the GraphQL API"
requires:
  bins: ["curl", "jq"]
  env: ["LINEAR_API_KEY"]
---

# Linear Skill

Drive Linear issue tracking via its GraphQL API. Get a personal API key at https://linear.app/settings/api and export it as `LINEAR_API_KEY`. The endpoint is `https://api.linear.app/graphql`; auth is the raw key in the `Authorization` header (no `Bearer` prefix).

## Helper

```bash
linear_gql() {
  curl -sS -X POST https://api.linear.app/graphql \
    -H "Authorization: $LINEAR_API_KEY" \
    -H "Content-Type: application/json" \
    -d "$1"
}
```

## Identify Yourself / Teams

```bash
# Current user (id, name, email)
linear_gql '{"query":"{ viewer { id name email } }"}' | jq

# List teams (id and key — the key like "ENG" is what shows in issue IDs)
linear_gql '{"query":"{ teams { nodes { id key name } } }"}' | jq
```

## Search & Read Issues

```bash
# Issues assigned to me, not done
linear_gql '{"query":"{ viewer { assignedIssues(filter:{state:{type:{nin:[\"completed\",\"canceled\"]}}}) { nodes { identifier title state { name } url } } } }"}' | jq

# Issue by identifier (e.g. ENG-123)
linear_gql '{"query":"query($id:String!){ issue(id:$id){ id identifier title description state{name} assignee{name} url } }","variables":{"id":"ENG-123"}}' | jq

# Search by text
linear_gql '{"query":"query($q:String!){ issues(filter:{title:{containsIgnoreCase:$q}},first:20){ nodes{ identifier title url } } }","variables":{"q":"login bug"}}' | jq
```

## Create an Issue

```bash
# Need a teamId from the teams query above
linear_gql '{
  "query":"mutation($i:IssueCreateInput!){ issueCreate(input:$i){ success issue{ identifier url } } }",
  "variables":{"i":{
    "teamId":"TEAM_UUID_HERE",
    "title":"Investigate flaky test",
    "description":"Repro steps:\n1. ..."
  }}
}' | jq
```

## Update / Comment / Close

```bash
# Update title or assignee (issue UUID, not identifier)
linear_gql '{"query":"mutation($id:String!,$i:IssueUpdateInput!){ issueUpdate(id:$id,input:$i){ success } }","variables":{"id":"ISSUE_UUID","i":{"title":"New title"}}}' | jq

# Add a comment
linear_gql '{"query":"mutation($i:CommentCreateInput!){ commentCreate(input:$i){ success comment{ url } } }","variables":{"i":{"issueId":"ISSUE_UUID","body":"Looks fixed in main"}}}' | jq

# Move to a workflow state (need stateId — query team.states first)
linear_gql '{"query":"query($k:String!){ team(id:$k){ states{ nodes{ id name type } } } }","variables":{"k":"TEAM_UUID"}}' | jq
```

## Notes

- Mutations take the **UUID** (`issue.id`); display lookups use the **identifier** (`ENG-123`).
- Pagination: most connections accept `first` / `after` and return `pageInfo { hasNextPage endCursor }`.
- Rate limit: ~1500 requests / hour per key. Cache team/state IDs after the first lookup.
- The GraphQL schema is documented at https://developers.linear.app/docs/graphql/working-with-the-graphql-api — check there for fields not shown here.
