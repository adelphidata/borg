---
name: hue
description: "Control Philips Hue lights through the bridge on the local network with the openhue CLI: turn lights and rooms on or off, set brightness, colour, and warmth, and activate scenes. Use for any request about the user's Hue lights, including timed lighting."
category: utilities
triggers:
  - "turn on the lights"
  - "turn off the lights"
  - "dim the living room"
  - "set the lights to"
  - "activate the relax scene"
requires:
  bins: ["openhue"]
install:
  openhue:
    brew: "openhue/cli/openhue-cli"
    url: "https://www.openhue.io/cli/setup"
---

# Philips Hue

`openhue` talks to the Hue Bridge over the local network, so Borg must run on
the same network as the bridge.

## First-time pairing

```bash
openhue setup        # finds the bridge, then asks the user to press its link button
```

Pairing needs a person at the bridge. Tell the user to press the button, then
run the command; do not loop on it.

## See what exists

Names are exact (case matters). Look them up first; use `--json` to parse.

```bash
openhue get room
openhue get light
openhue get light --room "Living Room"
openhue get scene
```

## Control

```bash
openhue set light "Desk Lamp" --on
openhue set light "Desk Lamp" --off
openhue set light "Desk Lamp" --on --brightness 40          # 0 to 100
openhue set light "Desk Lamp" --on --temperature 400        # 153 (cool) to 500 (warm), in mirek
openhue set light "Desk Lamp" --on --color powder_blue      # a colour name
openhue set light "Desk Lamp" --on --rgb "#FF8800"
openhue set light --room "Bedroom" "Lamp" --off             # same name in two rooms
openhue set room "Office" --on --brightness 100 --temperature 250
openhue set room "Bedroom" --off
openhue set room "Living Room" --on --brightness 30 --transition-time 10s
openhue set scene "Relax" --room "Living Room"              # --action dynamic for moving scenes
```

Colour works only on colour bulbs; white-only bulbs take brightness and,
on ambiance models, temperature.

## Presets

| Ask | Command |
|---|---|
| Bedtime | `openhue set room "Bedroom" --on --brightness 15 --temperature 480` |
| Focus | `openhue set room "Office" --on --brightness 100 --temperature 230` |
| Movie | `openhue set room "Living Room" --on --brightness 8 --temperature 450` |
| All off | `openhue set room "<each room>" --off` for every room from `get room` |

## Timed lighting

Use the `schedule` tool. A repeating time ("weekdays at 7am") is a `command`
job:

```json
{"action": "create", "type": "command", "name": "Wake lights",
 "command": "openhue set room Bedroom --on --brightness 60 --transition-time 5m",
 "schedule": "0 7 * * 1-5"}
```

A one-off ("in 30 minutes") is a `prompt` job with `schedule_type: "once"` and
a prompt such as "Turn off the living room lights with the hue skill."

Confirm the new state with `openhue get room` after a change the user cannot
see (when they are away).
