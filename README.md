# Harbor Migration

Rust script to migrate images between two Harbor registries.

## Features:
- Parallel processing: use `tokio` and async for concurrent migrations.
- Progress bars for better UX.
- Dry-run mode to preview what would be migrated.

## Getting started

- Check out the sample config.json:

```json
{
  "source": {
    "url": "https://harbor-a.example.com",
    "username": "robot$migration",
    "password": "source-password"
  },
  "destination": {
    "url": "https://harbor-b.example.com",
    "username": "robot$migration",
    "password": "dest-password"
  },
  "projects": [
    {
      "source": "project1",
      "destination": "project1-migrated"
    },
    {
      "source": "project2",
      "destination": "project2"
    }
  ]
}
```

- Edit file with your actual values, then run:

```sh
cargo run -- --config config.json
```
