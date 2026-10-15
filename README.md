# ErisCountyGISAdapters

A registry of parcel adapters for US counties, and the program that writes it. An adapter is a
small declarative spec that points at a county's public parcel service (or a statewide one) and
maps its attributes to a normalized schema. The registry holds pointers, never parcel data.

`eris-county-gis` runs [ErisHarness](https://github.com/Eriskii/ErisHarness) agents over the
3,222 counties and county equivalents of the 50 states, DC and Puerto Rico. Agents search for each
county's parcel layer, write a spec, and prove it works with a validator that actually queries the
service. A spec reaches the registry only by passing it.

## The registry

```text
registry/adapters/<id>.json        one spec each
registry/unavailable/<fips>.json   why a county has no adapter, and when to look again
registry/index.json                county FIPS → ids of the adapters verified for it
```

A spec:

```json
{
  "version": 1,
  "id": "ga-fulton",
  "publisher": "Fulton County GIS",
  "source": { "kind": "arcgis-feature", "url": "https://gismaps.fultoncountyga.gov/arcgispub/rest/services/OpenData/Tax_Parcels/MapServer/0",
              "page_size": 2000, "geojson": true },
  "coverage": { "counties": ["13121"] },
  "fields": { "apn": "ParcelID", "owner": "Owner", "address": "Address", "acres": "LandAcres", "land_use": "LUCode" },
  "notes": "Fulton County open data",
  "verification": { "verified_at": 1792041362000, "counties": ["13121"], "model": "…", "rung": 0 }
}
```

| Source kind      | Reads                                                                                 |
| ---------------- | ------------------------------------------------------------------------------------- |
| `arcgis-feature` | An ArcGIS REST FeatureServer or MapServer layer, as GeoJSON or Esri JSON, paged        |
| `wfs`            | A WFS 1.x or 2.0 `GetFeature` as GeoJSON, paged, with latitude-first boxes on request  |
| `ogc-features`   | An OGC API Features collection, following `next` links                                |
| `bulk-download`  | A GeoJSON file or zipped shapefile in EPSG 4326, 4269 or 3857, downloaded once         |

`coverage` is a list of county FIPS codes or a list of states. Normalized fields are `apn`,
`owner`, `address`, `acres`, `land_use` and `assessed_value`, each optional; `filter` is an
optional static filter (an ArcGIS `where` clause). Unavailable reasons are `no-public-gis`,
`viewer-only`, `paywalled`, `login-required` and `tos-forbids-automation`; a record is searched
again after 180 days, or 365 for the last two.

## Validation

`validate` runs a spec for one county:

- **query**: the service answers, and pages without repeating itself.
- **features**: at least 5 parcels around a point inside the county (boxes of 0.01°, 0.04°,
  then 0.12°).
- **longitude-latitude**, **location**, **in-county**: coordinates are degrees, lie where they
  were asked for, and mostly inside the county's boundary. Swapped axes and projected
  coordinates fail here.
- **valid-polygons**: 90% of polygons are valid.
- **fields-mapped**, **field-fill**: at least one of `apn`, `address`, `owner` is mapped; every
  mapped attribute exists and is filled for half the parcels or more.
- **paging**: reading the sample in pages of a third returns the same parcels.

The report lists the source's attribute names and sample attributes, which is how agents map
fields. A state's spec is validated against three counties spread across the state and passes
when two do.

All requests go through one client: GET only, an honest User-Agent, 60-second timeouts, size
caps, three requests at a time per host, and no private or loopback addresses (agents choose
the URLs).

## Runs

1. **States first.** Each selected state with open counties gets one agent looking for a
   statewide layer. When it finishes, every adapter claiming one of the state's open counties is
   validated against that county, with no model involved.
2. **Counties.** Each county still open gets an agent, briefed with its boundary, the adapters
   its neighbours use (counties often share a host or vendor), adapters that claim it but fail,
   and what earlier attempts tried.
3. **The model ladder.** Models are listed cheapest first. A county starts on rung 0; an attempt
   that runs out of failed validations (`--validate-budget`, 8 by default), stops without a
   result after one nudge, or fails, is retried on the next rung (after
   `--attempts-per-rung`), briefed with the last attempt's own summary and its validation
   results. The rung that succeeds is recorded in the spec and the journal. The default ladder
   comes from the account's model listing: its small model at medium effort, then its flagship
   at medium and high.

Agents' tools are three command lines, each called with one `command` string, split as a
shell splits words (quotes, backslashes, no expansion) and parsed with clap. Their help is in
the tool description and behind `--help`; a parse error comes back as the tool's error. They
run in-process: there is no shell, no file system and no sandbox.

```text
web get <url> [--offset N] [--grep PATTERN [--context N]]   pages as text, JSON pretty-printed
web search <query…>                                         DuckDuckGo's results
arcgis search <query…> [--state XX]                         ArcGIS Hub and Online, in the job's box
arcgis probe <url> [--grep PATTERN]                         directories, services, layers and fields
spec validate '<json>'                                      run a spec and read its report
spec show                                                   the spec validated last, and its report
spec submit                                                 save the spec validated last, if it passed
spec unavailable <reason> --evidence <url>… --note "…"      county jobs only
```

`--grep` takes a case-insensitive regular expression and is how agents cut a long answer, such as
a services directory, down to the lines they need. Statewide searches get the same `spec` as
`statewide_spec`, without `unavailable`. The terminal view shows each agent's current command
line.

**Checkpoints.** `state/journal.jsonl` gets a line, flushed to disk, when each attempt starts
and ends. The harness keeps its agents in `state/harness`. A run that is killed or stopped
resumes on the next start: running agents go on where they stood, finished attempts are not
repeated, and each county keeps its rung. `--recheck` validates every saved adapter first; a
county whose adapter fails is searched again with the dead spec in its briefing.

## Using it

```sh
cargo build --release
eris-county-gis login                 # ChatGPT, in the browser; --device for a device code
eris-county-gis models                # the account's models and the default ladder
eris-county-gis run                   # every county, in the terminal view
eris-county-gis run --states WI,GA --concurrency 16
eris-county-gis run --counties 13121 --no-tui --ladder gpt-5-codex-mini:medium,gpt-5-codex:high
eris-county-gis run --recheck
eris-county-gis validate spec.json 13121
eris-county-gis status                # the registry by state
eris-county-gis import-login ~/.config/ErisMaps/agents/account.json
eris-county-gis regenerate-counties   # rebuild data/counties.json.gz from census.gov
```

`--registry` (default `registry`) and `--state` (default `state`) choose the directories. The
login lives in `state/accounts/chatgpt.json`, owner-only, kept and refreshed by
[ErisAgentAuth](../../libs/ErisAgentAuth). `ERISCOUNTYGIS_OPENAI_AUTH` and
`ERISCOUNTYGIS_CODEX_API` point at other servers.

The terminal view shows overall progress, each state, live agents with the tool call each is
making, the ladder's rungs with their attempts, results and tokens, and the log. Keys: `p`
pause and resume, `+`/`-` concurrency, `l` sign in with a device code, `L` sign in in the
browser, `q` quit (agents resume on the next run). `--no-tui` prints the log and a progress line
instead, and Ctrl-C stops it the same way.

## Counties

`data/counties.json.gz` holds every county's FIPS, name, state, bounding box, an interior point
and its boundary, from the Census Bureau's 2024 1:500,000 cartographic boundaries, simplified to
about 30 m. It is embedded in the binary; `regenerate-counties` rebuilds it.

## Tests

`cargo test` runs the command-line tools as agents call them (help, parse errors, quoting,
`--grep`, the spec workflow), the validator against a fake county server on loopback (ArcGIS GeoJSON and
Esri JSON, a latitude-first WFS, OGC API Features, and broken variants: projected coordinates,
ignored offsets, refusals, missing fields), and whole runs against a stand-in model: an agent
that gives up and hands over to the next rung, a statewide layer covering its counties without
county agents, a county marked unavailable, and a run stopped mid-turn that resumes its agent.
