#!/usr/bin/env bash
# Fetch the US Census TIGER/Line 2023 block groups (public domain) for the
# `us_block_groups` demo. Nothing this writes is ever committed.
#
#   demos/data/fetch_us_block_groups.sh [--states 44,06,...] [--source DIR]
#                                       [--cache DIR] [--geojson [FILE]] [--ndjson]
#                                       [--no-income] [--no-names]
#
# 1. Downloads tl_2023_XX_bg.zip for every state/territory (56 archives,
#    ~1.1 GB) from https://www2.census.gov/geo/tiger/TIGER2023/BG/ into
#    $CACHE/zip, skipping any archive already present. `--source DIR` (or
#    US_BG_SOURCE) names an existing copy of those archives to read from
#    instead of downloading.
# 2. Unpacks each archive's small members (.dbf .shx .prj .cpg) into
#    $CACHE/shp. The big .shp is symlinked when --source already holds it
#    extracted, and unzipped otherwise. This is what the demo reads by default:
#    no conversion, the shapefiles as the Census publishes them.
# 3. `--geojson [FILE]` additionally converts the selected states with ogr2ogr
#    to ONE GeoJSON FeatureCollection (EPSG:4326, only GEOID, ALAND, AWATER),
#    for the demo's `--geojson FILE` path. `--ndjson` writes newline-delimited
#    GeoJSON instead. The national file is ~3-5 GB, so the conversion refuses
#    to start unless the target disk keeps 8 GB free afterwards.
# 4. Median household income (ACS 5-year 2023, B19013_001E) for every block
#    group, joined by the demo on GEOID. One Census API request per state
#    (`api.census.gov/data/2023/acs/acs5?get=B19013_001E&for=block group:*
#    &in=state:SS&in=county:*&in=tract:*`), 0.5 s apart, each cached as
#    $CACHE/acs/api/SS.json. The API now answers every keyless request with
#    a redirect to missing_key.html, so set CENSUS_API_KEY (free) to use it;
#    without a key, or when it refuses, the script falls back to the Census
#    table-based summary file for the same table (acsdt5y2023-b19013.dat,
#    ~18 MB, one download, every block group, no key). Either way the result
#    is $CACHE/acs/income_bg_2023.csv: `GEOID,income`, GEOID the 12-digit
#    state+county+tract+block group, income empty when the Census reports a
#    negative sentinel (-666666666 etc: no estimate). The island areas
#    (60 66 69 78) are not in the ACS and simply have no rows. Skipped when
#    the CSV already exists. `--no-income` skips this step and 6-7. The
#    steps run in this order: 4, then 5, then 6 and 7.
# 5. County (and county-equivalent) names for the hover popup's title line:
#    the Census's 2023 county gazetteer
#    (2023_Gazetteer/2023_Gaz_counties_national.zip, one ~650 KB file, no
#    key needed), unzipped to $CACHE/names/2023_Gaz_counties_national.txt --
#    tab-delimited `USPS GEOID ANSICODE NAME ...`, GEOID the 5-digit
#    state+county FIPS, ~3,220 rows covering every state, DC, Puerto Rico's
#    78 municipios and Connecticut's 2022+ FIPS 09110-09190 planning
#    regions (which replaced its eight counties in TIGER2023 -- the older
#    `codes2020/national_county2020.txt` this demo could otherwise use
#    still lists only the pre-2022 counties, so it was rejected). Skipped
#    when the file already exists. `--no-names` skips this step; the demo
#    then falls back to "County NNN" in the popup instead of a name.
# 6. Total population (ACS 5-year 2023, B01003_001E) the same way, for the
#    demo's population-weighted breakdowns: $CACHE/acs/population_bg_2023.csv
#    (`GEOID,population`), from the API into $CACHE/acs/api-b01003/SS.json or
#    from acsdt5y2023-b01003.dat (~18 MB).
# 7. County -> metropolitan statistical area: the Census CBSA delineation of
#    July 2023 (list1_2023.xlsx, ~140 KB), converted with python3+openpyxl (or
#    duckdb) to $CACHE/cbsa/county_metro_2023.tsv (`county<TAB>cbsa<TAB>title`,
#    metro areas only; Connecticut by its 2022 planning regions, as TIGER 2023).
#    With neither converter it says so and goes on; the script exits 1.
#
# Defaults: CACHE=${US_BG_CACHE:-~/.cache/gpui-map/demos/us-block-groups},
# every state. Keep CACHE on a fast internal disk.
set -euo pipefail

BASE_URL="https://www2.census.gov/geo/tiger/TIGER2023/BG"
ALL_STATES="01 02 04 05 06 08 09 10 11 12 13 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 44 45 46 47 48 49 50 51 53 54 55 56 60 66 69 72 78"

CACHE="${US_BG_CACHE:-$HOME/.cache/gpui-map/demos/us-block-groups}"
SOURCE="${US_BG_SOURCE:-}"
STATES="$ALL_STATES"
GEOJSON=""
FORMAT="GeoJSON"
OGR2OGR="${OGR2OGR:-$(command -v ogr2ogr || echo /usr/local/bin/ogr2ogr)}"
SPARE_BYTES=$((8 * 1024 * 1024 * 1024))
INCOME=1
NAMES=1
ACS_API="https://api.census.gov/data/2023/acs/acs5"
ACS_SUMMARY_DIR="https://www2.census.gov/programs-surveys/acs/summary_file/2023/table-based-SF/data/5YRData"
NAMES_URL="https://www2.census.gov/geo/docs/maps-data/data/gazetteer/2023_Gazetteer/2023_Gaz_counties_national.zip"
CBSA_DELINEATION="https://www2.census.gov/programs-surveys/metro-micro/geographies/reference-files/2023/delineation-files/list1_2023.xlsx"

while [ $# -gt 0 ]; do
  case "$1" in
    --states) STATES="$(echo "$2" | tr ',' ' ')"; shift 2 ;;
    --all) STATES="$ALL_STATES"; shift ;;
    --source) SOURCE="$2"; shift 2 ;;
    --cache) CACHE="$2"; shift 2 ;;
    --geojson)
      if [ $# -gt 1 ] && [ "${2#--}" = "$2" ]; then GEOJSON="$2"; shift 2; else GEOJSON="auto"; shift; fi ;;
    --no-income) INCOME=0; shift ;;
    --no-names) NAMES=0; shift ;;
    --ndjson) FORMAT="GeoJSONSeq"; [ -n "$GEOJSON" ] || GEOJSON="auto"; shift ;;
    -h|--help) sed -n '2,61p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "$CACHE/zip" "$CACHE/shp"

for fips in $STATES; do
  name="tl_2023_${fips}_bg"
  zip="$CACHE/zip/$name.zip"
  if [ -n "$SOURCE" ] && [ -s "$SOURCE/$name.zip" ]; then
    zip="$SOURCE/$name.zip"
  elif [ ! -s "$zip" ]; then
    echo "download $name.zip"
    curl -fsSL --retry 3 -o "$zip.part" "$BASE_URL/$name.zip"
    mv "$zip.part" "$zip"
  fi

  if [ ! -s "$CACHE/shp/$name.dbf" ]; then
    unzip -qo "$zip" "$name.dbf" "$name.shx" "$name.prj" "$name.cpg" -d "$CACHE/shp"
  fi
  if [ ! -e "$CACHE/shp/$name.shp" ]; then
    if [ -n "$SOURCE" ] && [ -s "$SOURCE/$name.shp" ]; then
      ln -s "$SOURCE/$name.shp" "$CACHE/shp/$name.shp"
    else
      unzip -qo "$zip" "$name.shp" -d "$CACHE/shp"
    fi
  fi
done
echo "shapefiles ready in $CACHE/shp"

# --- 4 and 6. ACS block-group tables (income, population) -------------------
# fetch_acs TABLE VARIABLE API_DIR CSV COLUMN: one ACS 5-year 2023 estimate
# for every block group, as `GEOID,COLUMN`, from the API with a key or the
# table-based summary file without one. Skipped when CSV already exists.
fetch_acs() {
  local table="$1" var="$2" api="$CACHE/acs/$3" csv="$CACHE/acs/$4" column="$5"
  if [ -s "$csv" ]; then echo "$column ready in $csv"; return 0; fi
  mkdir -p "$api"
  local api_ok=0
  if [ -n "${CENSUS_API_KEY:-}" ]; then
    api_ok=1
    for fips in $ALL_STATES; do
      case "$fips" in 60|66|69|78) continue ;; esac  # not in the ACS
      out="$api/$fips.json"
      [ -s "$out" ] && continue
      url="$ACS_API?get=$var&for=block%20group:*&in=state:$fips&in=county:*&in=tract:*&key=$CENSUS_API_KEY"
      if curl -fsS --retry 3 -o "$out.part" "$url" && head -c 2 "$out.part" | grep -q '\[\['; then
        mv "$out.part" "$out"
      else
        rm -f "$out.part"; api_ok=0
        echo "census API refused state $fips; falling back to the summary file" >&2
        break
      fi
      sleep 0.5
    done
  fi
  if [ "$api_ok" -eq 1 ]; then
    # [["B19013_001E","state","county","tract","block group"],["63750","01","001","020100","1"],...]
    cat "$api"/*.json | tr -d '[]"' | tr ',' ' ' | awk -v var="$var" -v column="$column" '
      BEGIN { print "GEOID," column }
      NF == 5 && $1 != var {
        v = ($1 + 0 < 0) ? "" : $1; printf "%s%s%s%s,%s\n", $2, $3, $4, $5, v }' > "$csv.part"
  else
    local lower dat
    lower="$(echo "$table" | tr 'A-Z' 'a-z')"
    dat="$CACHE/acs/acsdt5y2023-$lower.dat"
    if [ ! -s "$dat" ]; then
      echo "download acsdt5y2023-$lower.dat"
      curl -fsSL --retry 3 -o "$dat.part" "$ACS_SUMMARY_DIR/acsdt5y2023-$lower.dat"
      mv "$dat.part" "$dat"
    fi
    # GEO_ID|<TABLE>_E001|<TABLE>_M001; block groups are summary level 150:
    # 1500000US + the 12-digit GEOID.
    awk -F'|' -v column="$column" '
      BEGIN { print "GEOID," column }
      substr($1, 1, 9) == "1500000US" {
        v = ($2 == "" || $2 + 0 < 0) ? "" : $2; printf "%s,%s\n", substr($1, 10), v }' "$dat" > "$csv.part"
  fi
  mv "$csv.part" "$csv"
  rows=$(($(wc -l < "$csv") - 1)); missing=$(grep -c ',$' "$csv" || true)
  echo "$column ready in $csv ($rows block groups, $missing without an estimate)"
}

# --- 7. county -> metropolitan statistical area ------------------------------
# The Census CBSA delineation of July 2023 (OMB Bulletin 23-01; list1_2023.xlsx,
# the current file -- the Census publishes no CSV of it), which already uses
# Connecticut's nine planning regions (09110..09190), the county equivalents
# TIGER 2023 carries. Converted once to $CACHE/cbsa/county_metro_2023.tsv:
# `county<TAB>cbsa<TAB>title`, the 5-digit state+county FIPS, METROPOLITAN
# statistical areas only (micropolitan areas are dropped). Needs python3 with
# openpyxl, or duckdb (its excel extension) as the fallback.
fetch_cbsa() {
  local dir="$CACHE/cbsa" tsv="$CACHE/cbsa/county_metro_2023.tsv" xlsx="$CACHE/cbsa/list1_2023.xlsx"
  if [ -s "$tsv" ]; then echo "metros ready in $tsv"; return 0; fi
  mkdir -p "$dir"
  if [ ! -s "$xlsx" ]; then
    echo "download list1_2023.xlsx"
    curl -fsSL --retry 3 -o "$xlsx.part" "$CBSA_DELINEATION" || return 1
    mv "$xlsx.part" "$xlsx"
  fi
  if python3 -c 'import openpyxl' 2>/dev/null; then
    python3 - "$xlsx" > "$tsv.part" <<'PY'
import sys, openpyxl
rows = openpyxl.load_workbook(sys.argv[1], read_only=True).active.iter_rows(values_only=True)
print("county\tcbsa\ttitle")
for r in rows:
    # CBSA Code, Div, CSA, CBSA Title, Metro/Micro, Div title, CSA title,
    # county name, state name, FIPS state, FIPS county, central/outlying.
    if len(r) < 11 or r[4] != "Metropolitan Statistical Area":
        continue
    print(f"{r[9]}{r[10]}\t{r[0]}\t{r[3]}")
PY
    [ $? -eq 0 ] || return 1
  elif command -v duckdb >/dev/null; then
    duckdb -noheader -list -separator "$(printf '\t')" -c "
      INSTALL excel; LOAD excel;
      SELECT 'county', 'cbsa', 'title';
      SELECT \"FIPS State Code\" || \"FIPS County Code\", \"CBSA Code\", \"CBSA Title\"
        FROM read_xlsx('$xlsx', range = 'A3:L2000', all_varchar = true)
        WHERE \"Metropolitan/Micropolitan Statistical Area\" = 'Metropolitan Statistical Area';" > "$tsv.part" || return 1
  else
    echo "list1_2023.xlsx needs python3 with openpyxl (pip3 install openpyxl) or duckdb to convert" >&2
    return 1
  fi
  mv "$tsv.part" "$tsv"
  counties=$(($(wc -l < "$tsv") - 1)); metros=$(tail -n +2 "$tsv" | cut -f2 | sort -u | wc -l | tr -d ' ')
  echo "metros ready in $tsv ($counties counties in $metros metropolitan statistical areas)"
}

[ "$INCOME" -eq 0 ] || fetch_acs B19013 B19013_001E api income_bg_2023.csv income

# --- 5. county (and county-equivalent) names -------------------------------
fetch_county_names() {
  local dir="$CACHE/names" txt="$CACHE/names/2023_Gaz_counties_national.txt"
  if [ -s "$txt" ]; then
    rows=$(($(wc -l < "$txt") - 1))
    echo "county names ready in $txt ($rows counties and county-equivalents)"
    return 0
  fi
  mkdir -p "$dir"
  local zip="$dir/2023_Gaz_counties_national.zip"
  echo "download 2023_Gaz_counties_national.zip"
  curl -fsSL --retry 3 -o "$zip.part" "$NAMES_URL"
  mv "$zip.part" "$zip"
  unzip -qo "$zip" -d "$dir"
  rm -f "$zip"
  rows=$(($(wc -l < "$txt") - 1))
  echo "county names ready in $txt ($rows counties and county-equivalents)"
}
[ "$NAMES" -eq 0 ] || fetch_county_names

# 6 and 7, after the names: the breakdowns panel's tables are optional, so a
# missing xlsx converter is reported and the script goes on (the popup's
# county names above are already in), exiting non-zero at the end.
BREAKDOWNS_STATUS=0
if [ "$INCOME" -eq 1 ]; then
  fetch_acs B01003 B01003_001E api-b01003 population_bg_2023.csv population
  if ! fetch_cbsa; then
    rm -f "$CACHE/cbsa/county_metro_2023.tsv.part"
    echo "metro table NOT written; the demo's breakdowns panel will say so. Everything else is ready." >&2
    BREAKDOWNS_STATUS=1
  fi
fi

[ -n "$GEOJSON" ] || exit "$BREAKDOWNS_STATUS"

if [ "$GEOJSON" = "auto" ]; then
  if [ "$STATES" = "$ALL_STATES" ]; then tag="all"; else tag="$(echo $STATES | tr ' ' '_')"; fi
  ext="geojson"; [ "$FORMAT" = "GeoJSONSeq" ] && ext="geojsonl"
  GEOJSON="$CACHE/us_block_groups_${tag}.${ext}"
fi

# GeoJSON is ~2.5x the .shp at full precision; refuse rather than fill the disk.
shp_bytes=0
for fips in $STATES; do
  s=$(stat -Lf %z "$CACHE/shp/tl_2023_${fips}_bg.shp")
  shp_bytes=$((shp_bytes + s))
done
estimate=$((shp_bytes * 5 / 2))
out_dir="$(dirname "$GEOJSON")"
mkdir -p "$out_dir"
free=$(($(df -Pk "$out_dir" | awk 'NR==2 {print $4}') * 1024))
echo "estimated output $((estimate / 1048576)) MB, $((free / 1048576)) MB free on $out_dir"
if [ $((free - estimate)) -lt $SPARE_BYTES ]; then
  echo "refusing: writing $GEOJSON would leave less than 8 GB free" >&2
  exit 1
fi

rm -f "$GEOJSON"
first=1
for fips in $STATES; do
  src="$CACHE/shp/tl_2023_${fips}_bg.shp"
  if [ $first -eq 1 ]; then
    "$OGR2OGR" -f "$FORMAT" -t_srs EPSG:4326 -select GEOID,ALAND,AWATER \
      -lco RFC7946=NO -lco COORDINATE_PRECISION=7 "$GEOJSON" "$src"
    first=0
  else
    "$OGR2OGR" -f "$FORMAT" -append -t_srs EPSG:4326 -select GEOID,ALAND,AWATER "$GEOJSON" "$src"
  fi
done
echo "wrote $GEOJSON ($(du -h "$GEOJSON" | cut -f1))"
exit "$BREAKDOWNS_STATUS"
