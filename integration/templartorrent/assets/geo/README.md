# Bundled IP-to-country database

`dbip-country-lite-2026-11.mmdb.gz` is DB-IP's "IP to Country Lite" database,
November 2026 edition, in MaxMind DB format, gzip-compressed.

- Source: https://db-ip.com/db/download/ip-to-country-lite
  (downloaded from https://download.db-ip.com/free/dbip-country-lite-2026-11.mmdb.gz
  on 2026-11-05).
- SHA-256 of the uncompressed `.mmdb`:
  `145ce5dfb812d8de28547e6289bb5c5f8b3df2f1dea21d324d5b575e0431311f`
  (8,515,063 bytes).
- License: Creative Commons Attribution 4.0 International
  (https://creativecommons.org/licenses/by/4.0/). Attribution required by DB-IP:
  "IP Geolocation by DB-IP" with a link to https://db-ip.com. The Peers tab shows
  that line whenever this database is the one in use.

`build.rs` inflates the file into `OUT_DIR` at compile time and `geo.rs` embeds it
with `include_bytes!`, so lookups read the binary's own pages: no runtime
decompression, no network, no file next to the executable. A database the user
supplies (`--geoip-db`, `TEMPLARTORRENT_GEOIP_DB`, or GeoLite2 in the data
directory) still takes precedence.

To update: download the current month's `dbip-country-lite-YYYY-MM.mmdb.gz`,
replace this file, update `GEO_BUNDLED_FILE` in `build.rs` and the edition in
`geo.rs`, and this README's checksum.
