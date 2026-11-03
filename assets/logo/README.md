# Logo and mascot

| File | What it is | Use it for |
|---|---|---|
| `logo.svg` | The logo: an undo arrow around a terminal prompt, on a square | Discord server icon (Discord crops it to a circle), app icons |
| `logo-rounded.svg` | The same with rounded corners, see-through outside | README, GitHub, websites |
| `mascot.svg` | The mascot: an "oops" face. Its eyes are the terminal chevrons `> <`, its mouth an undo arrow | Discord emoji (`:whoops:`), stickers, fun places |

PNGs are made from the SVGs with `./render.sh` (needs `rsvg-convert`): the logo at 512, 256,
128 and 64 px, the rounded logo and the mascot at 512 px, and the mascot at 128 px for Discord
emoji.

## Colors

| | |
|---|---|
| Coral (the "oops" color) | `#FF7A59` |
| Near-black | `#15161C` |
| White | `#F4F4F5` |
