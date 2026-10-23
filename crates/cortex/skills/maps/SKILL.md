---
name: maps
description: "Answer place and travel questions with free OpenStreetMap data, no API key: find coordinates for an address, the address at a point, nearby places (cafes, pharmacies, ATMs...), driving, walking, or cycling distance and directions, and the time zone and local time anywhere."
category: utilities
triggers:
  - "find a cafe near"
  - "how far is it from"
  - "directions from"
  - "what time is it in"
  - "coordinates of"
  - "what's near this location"
requires:
  bins: ["curl", "jq"]
install:
  curl:
    brew: curl
    apt: curl
  jq:
    brew: jq
    apt: jq
---

# Maps

Free public services, no key. Be polite: send a User-Agent, wait 1 second
between Nominatim calls, and do not batch hundreds of lookups.

```bash
UA="Borg maps (personal assistant)"
```

## Address to coordinates (and back)

```bash
curl -s -G -A "$UA" https://nominatim.openstreetmap.org/search \
  --data-urlencode "q=Times Square, New York" -d format=jsonv2 -d limit=3 |
  jq -r '.[] | "\(.lat),\(.lon)  \(.display_name)  [\(.category)/\(.type)]"'

curl -s -A "$UA" "https://nominatim.openstreetmap.org/reverse?lat=48.8584&lon=2.2945&format=jsonv2" |
  jq -r '.display_name'
```

Add the country or state to an ambiguous name or a bare ZIP code
(`-d countrycodes=us`). The result's `boundingbox` gives an area's extent.

## Nearby places

Common tags: `amenity=` cafe, restaurant, bar, fast_food, pharmacy, hospital,
doctors, dentist, atm, bank, fuel, charging_station, parking, police,
post_office, library, toilets; `shop=` supermarket, convenience, bakery;
`tourism=` hotel, museum, attraction; `leisure=` park, fitness_centre,
playground. Results come sorted by distance, with a map link.

```bash
LAT=40.758 LON=-73.9855 RADIUS=800 TAG='"amenity"="pharmacy"'
curl -s -A "$UA" https://overpass-api.de/api/interpreter \
  --data-urlencode "data=[out:json][timeout:25];nwr[$TAG](around:$RADIUS,$LAT,$LON);out center 40;" |
jq -r --argjson la "$LAT" --argjson lo "$LON" '
  def rad: . * 0.017453293;
  [.elements[] | (.lat // .center.lat) as $a | (.lon // .center.lon) as $b
   | (((($a - $la) | rad) / 2 | sin) as $x | ((($b - $lo) | rad) / 2 | sin) as $y
      | 12742000 * (($x * $x + ($la | rad | cos) * ($a | rad | cos) * $y * $y) | sqrt | asin)) as $m
   | {m: ($m | round), name: (.tags.name // "unnamed"), hours: (.tags.opening_hours // ""),
      phone: (.tags.phone // ""), link: "https://www.google.com/maps/search/?api=1&query=\($a),\($b)"}]
  | sort_by(.m)[:10][] | "\(.m) m  \(.name)  \(.hours)  \(.phone)  \(.link)"'
```

If jq reports `Invalid numeric literal`, Overpass sent a busy or rate-limit
page instead of JSON: wait a minute, or use the mirror
`https://overpass.kumi.systems/api/interpreter`.
Opening hours are community data: for "is it open now", say they may be out
of date, or check with `web_search`.

## Distance, travel time, directions

Coordinates go in as `LON,LAT` (longitude first). Profiles: `routed-car`,
`routed-foot`, `routed-bike`.

```bash
FROM="2.2945,48.8583" TO="2.3380,48.8611" MODE=routed-foot
curl -s "https://routing.openstreetmap.de/$MODE/route/v1/driving/$FROM;$TO?steps=true&overview=false" |
jq -r '.routes[0] | "\(.distance / 100 | round / 10) km, about \(.duration / 60 | round) min",
  (.legs[0].steps[] | select(.distance > 0)
   | "- \(.maneuver.type) \(.maneuver.modifier // "") on \(if .name == "" then "unnamed way" else .name end) (\(.distance | round) m)")'
```

Drop `steps=true` and the second jq line when only distance and time are
needed. Car times ignore live traffic; say so for commutes. For a link the
user can open: `https://www.google.com/maps/dir/?api=1&origin=LAT,LON&destination=LAT,LON&travelmode=walking`.

## Time zone and local time

```bash
curl -s "https://timeapi.io/api/timezone/coordinate?latitude=35.68&longitude=139.65" |
  jq -r '"\(.timeZone)  local time \(.currentLocalTime[:16])"'
```

If timeapi.io is down, Open-Meteo returns the zone too:
`curl -s "https://api.open-meteo.com/v1/forecast?latitude=35.68&longitude=139.65&timezone=auto" | jq -r .timezone`,
then `TZ=Asia/Tokyo date` for the local time.

## Pitfalls

- Coordinates: Nominatim and Overpass use `lat,lon`; the routing URL uses `lon,lat`.
- Geocode each place on its own and check the match before routing; a wrong
  "Springfield" gives a confident wrong answer.
- Walking and cycling coverage is best in Europe and North America.
