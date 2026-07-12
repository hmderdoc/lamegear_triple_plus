# Box art cache

Populated locally by the sysop — images are never bundled or fetched by the
door itself:

```sh
python3 tools/fetch_game_art.py --roms roms --output art
```

Art is keyed per system (`art/gg`, `art/sms`, `art/sg`, `art/md`, `art/nes`,
`art/sfc`, `art/gba`, `art/pce`) from the Libretro thumbnail repositories,
matched by ROM filename. The menu shows it as a SIXEL preview on capable
terminals and CP437 half-blocks everywhere else; `A` on a shelf shows it
full-screen. Missing art just means no preview — nothing breaks.
