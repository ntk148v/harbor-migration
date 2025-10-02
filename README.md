# Harbor Migration

Rust script to migrate images between two Harbor registries.

## Features:
- Parallel processing: use `tokio` and async for concurrent migrations.
- Progress bars for better UX.
- Dry-run mode to preview what would be migrated.

## Getting started

- You can generate an example config file:

```sh
cargo run -- --generate-config config.json
```

- This creates `config.json`:

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
    "project1",
    "project2",
    "project3"
  ]
}
```

- Edit file with your actual values, then run:

```sh
cargo run -- --config config.json
```

- Or you want to use environment variables. Set environment variables like this:

```sh
export SOURCE_HARBOR_URL="https://harbor-a.example.com"
export SOURCE_HARBOR_USERNAME="robot\$migration"
export SOURCE_HARBOR_PASSWORD="your-source-password"

export DEST_HARBOR_URL="https://harbor-b.example.com"
export DEST_HARBOR_USERNAME="robot\$migration"
export DEST_HARBOR_PASSWORD="your-dest-password"

export PROJECTS="project1,project2,project3"
```

- Then run:

```rust
cargo run -- --env
```
