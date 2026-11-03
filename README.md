# spotify-move

Copies a Spotify library from one account to another, getting liked songs, saved albums, artists and shows, playlists made, followed playlists, sidebar order and pins.

It drives the web player through Chrome instead of the public Web API.
## Usage

```
cargo build --release
spotify-move export old -o library.json
spotify-move import new library.json
spotify-move verify new library.json
```

`import` keeps its progress next to the backup and can be rerun after an interruption. Liked songs are saved one per second to keep their order, so 3,500 songs take about an hour; `--fast` saves them in batches by ignoring order

Needs Chrome, Chromium or Edge. Pass `--chrome PATH` if it isn't found.

## Limits

- Blends, Wrapped and other playlists made for the old account become static copies
- Local files, listening history and saved podcast episodes aren't copied
- It uses undocumented endpoints, so it might break in the future
