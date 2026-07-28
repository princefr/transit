# ban-search

Local **Base Adresse Nationale (BAN)** download, compact index, and autocomplete for France.

Official open data: [adresse.data.gouv.fr](https://adresse.data.gouv.fr)  
CSV dumps: `https://adresse.data.gouv.fr/data/ban/adresses/latest/csv/adresses-{dept}.csv.gz`

## CLI

From the `transit` workspace root:

```bash
# Île-de-France (default departments)
make ban-index

# Paris only (faster)
make ban-index-paris

# Query
make ban-suggest QUERY="12 rue de rivoli paris"
```

Or:

```bash
cargo run -p ban-search --bin ban-index -- \
  --data-dir ./data/ban --download --build

cargo run -p ban-search --bin ban-index -- \
  --data-dir ./data/ban --suggest "bd haussmann"
```

Produces:

- `data/ban/csv/adresses-XX.csv` — raw département dumps  
- `data/ban/index.bin` — bincode snapshot loaded by the transit API at startup  

## Library

```rust
use ban_search::{BanConfig, BanIndex, download_departments};

let cfg = BanConfig::idf_default("./data/ban");
download_departments(&cfg)?;
let index = BanIndex::build_from_csv_dir(&cfg.csv_dir(), &cfg.departments)?;
index.save(&cfg.index_path())?;

let hits = index.suggest("12 rue de rivoli", 8);
```

## GraphQL (transit server)

```graphql
query {
  addresses(search: "12 rue de rivoli", limit: 8) {
    label lat lon kind street postcode city
  }
  places(search: "rivoli", limit: 12) {
    kind label lat lon stopId
  }
}
```

Door-to-door itineraries:

```graphql
mutation {
  # use coordinates from an address hit
}
# actually query:
# itineraries(input: {
#   from: { lat: 48.86, lon: 2.33, name: "12 Rue de Rivoli" }
#   to: { stopId: "idfm:..." }
# })
```

## Config

```toml
[ban]
enabled = true
# data_dir = "./data/ban"
# departments = ["75", "77", "78", "91", "92", "93", "94", "95"]
```
