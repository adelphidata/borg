---
name: weather
description: "Weather forecasts via wttr.in"
requires:
  bins: ["curl"]
---

# Weather

`wttr.in` — no API key. Accepts city names (use `+` for spaces:
`New+York`) and airport codes (`ORD`). Rate-limited.

```bash
curl -s "wttr.in/London?format=3"               # one-line summary
curl -s "wttr.in/London?0"                      # detailed current
curl -s "wttr.in/London"                        # 3-day forecast
curl -s "wttr.in/London?format=v2"              # week
curl -s "wttr.in/London?1"                      # tomorrow (0/1/2)
curl -s "wttr.in/London?format=j1"              # JSON
```

Custom format codes: `%l` location, `%c` condition emoji, `%t` temp,
`%f` feels-like, `%w` wind, `%h` humidity, `%p` precip.

```bash
curl -s "wttr.in/London?format=%l:+%c+%t+(feels+like+%f),+%w,+%h"
```
