# assets

Every screenshot here is the terminal running, against the mock chain in `test/`. None of them is a mock-up, and
none is a picture of something the repository does not contain. They are regenerated, not retouched.

| file | what it is |
|---|---|
| `avatar.png`, `banner.png`, `social.png` | the marks: the avatar, the README banner, and the social preview GitHub shows when a link is posted. Drawn by `scripts/marks.js` out of `cli/banner.js`, so they are the same wordmark the terminal prints |
| `tour.png` | `loxley tour`: what it runs against, then chapter one, `doctor` |
| `terminal.png` | chapter five of the tour: `snipe --live`, signing |
| `market.png` | chapter two: `market` over the recorded Robinhood Chain market |
| `guard.png` | chapter eight: `buy`, then `watch --guard` selling on the DEV SOLD siren |
| `profile.png` | chapter ten: `positions` and `profile` over the fills the tour just made |
| `cli-hunt.png` `cli-snipe.png` `cli-follow.png` `cli-grad.png` `cli-scan.png` `cli-fees.png` `cli-dev.png` `cli-wallet.png` `cli-doctor.png` `cli-radar.png` `cli-replay.png` | one command each, run on its own |
| `desk.png` `console.png` `radar.png` `drill.png` `xray.png` | the browser desk (`index.html`): the whole desk, the console, the radar, an exit drill, the wallet x-ray |

To take them again: run the command against `npm run mock` (or `loxley tour`) with `FORCE_COLOR=1` and a fixed
`COLUMNS`, then render the ansi with a line height of exactly 1 on the banner rows, or the quadrant blocks it is
drawn out of will not meet. `node scripts/marks.js` redraws the three marks. The figures move because the mock mints fresh launches each run; the layout does
not, because every command is measured against the window before it prints.
