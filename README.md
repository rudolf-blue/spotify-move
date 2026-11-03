# spotify-move

Copies a Spotify library from one account to another: liked songs in order, saved albums, artists and shows, your playlists with their covers and "date added" values, followed playlists, sidebar order and pins.

It drives the web player through Chrome instead of the public Web API, so no developer app is needed. Each account gets its own Chrome profile and you log in once.

## Usage

```
cargo build --release
spotify-move export old -o library.json
spotify-move import new library.json
spotify-move verify new library.json
```

`export` never writes to the account. `import` keeps its progress next to the backup and can be rerun after an interruption. Liked songs are saved one per second to keep their order, so 3,500 songs take about an hour; `--fast` saves them in batches and loses the order within each batch.

Needs Chrome, Chromium or Edge. Pass `--chrome PATH` if it isn't found.

## Limits

- Liked songs get the import date, since Spotify doesn't let clients set it.
- Blends, Wrapped and other playlists made for the old account become static copies.
- Local files, listening history and saved podcast episodes aren't copied.
- It uses undocumented endpoints, so a web player update can break it.
