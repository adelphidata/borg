---
name: weather
description: "Weather forecasts via wttr.in"
requires:
  bins: ["curl"]
---

# Weather Skill

Get current weather conditions and forecasts using wttr.in.

## Current Weather

```bash
# One-line summary
curl -s "wttr.in/London?format=3"

# Detailed current conditions
curl -s "wttr.in/London?0"

# Custom format
curl -s "wttr.in/London?format=%l:+%c+%t+(feels+like+%f),+%w+wind,+%h+humidity"
```

## Forecasts

```bash
# 3-day forecast
curl -s "wttr.in/London"

# Week forecast
curl -s "wttr.in/London?format=v2"

# Specific day (0=today, 1=tomorrow, 2=day after)
curl -s "wttr.in/London?1"
```

## JSON Output

```bash
curl -s "wttr.in/London?format=j1"
```

## Format Codes

- `%c` — Weather condition emoji
- `%t` — Temperature
- `%f` — "Feels like"
- `%w` — Wind
- `%h` — Humidity
- `%p` — Precipitation
- `%l` — Location

## Notes

- No API key needed
- Works for most global cities
- Supports airport codes: `curl wttr.in/ORD`
- Use `+` for spaces in city names: `New+York`
- Rate limited; don't spam requests
