# Paris, 1888: benchmark of PR #75

Frames and pages produced by the benchmark run described in the comment on
https://github.com/samuelfaj/distill/pull/75 (2026-11-17).

- `frames/<mode>-<build>.jpg`: the frame at 7 s, from `window.renderAt(7)`, 1280x720 scaled to 960x540.
- `pages/<mode>-<build>.html`: the page each run produced. GitHub shows HTML as text: download a file and
  open it in a browser. Pages of about 40 KB load three.js from a CDN and need network access.
- `main` is the build at 2ff8b021, `pr` the build at 33865367. Prompt, models and setup are in the comment.
