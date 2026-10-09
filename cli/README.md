# kob — koblem CLI

Call koblem's image / video / background-removal / TTS / STT / music / sfx /
avatar / chat endpoints from the terminal or scripts. Takes file paths in and
writes files out — no base64 juggling.

## Install

```bash
cargo install --path cli        # from the koblem repo root
# or just build it:
make cli                         # fmt + clippy + build → cli/target/debug/kob
```

## Configure

Create an API key in koblem's web UI (**Settings → API Keys**), then:

```bash
kob config set --url http://your-host:8090 --token <key>
kob config show                  # token masked
```

Resolution order for URL/token: `--url`/`--token` flags → `KOBLEM_URL`/`KOBLEM_TOKEN`
env → `~/.config/kob/config.toml` → default `http://localhost:8090`.

### Per-command defaults

So common calls aren't long flag lists. Run `kob config defaults` to print the
full annotated list of every tunable key, or `kob config edit` to open the file
(a fresh one is pre-seeded with that same commented template):

```toml
url = "http://localhost:8090"
token = "..."

[defaults.image]
width = 1024
height = 1024
steps = 12

[defaults.tts]
voice = "default"
format = "mp3"
```

Precedence: explicit flag > config default > server default.

## Examples

```bash
kob chat "explain X in 3 bullets" --system "be terse"
kob chat "what's in this?" --image screenshot.png

kob image generate "a red apple on white" --width 1024 -o apple.png
kob image generate "logo" --rmbg --bg-mode color --bg-color "#00ff00"
kob image suggest "a cozy cabin at dusk"
kob image props

kob rmbg photo.png                       # → photo-cutout.png
kob rmbg a.png b.png c.png --sensitivity 0.6

kob tts "hello there" --voice default -o hi.mp3
kob tts voices                           # list cloned voices
kob tts voices add my-voice sample.wav --engine higgs
kob tts voices delete my-voice

kob stt recording.wav --granularity word --timestamps

kob music generate "lofi piano, rainy" --duration 30
kob music generate "power ballad" --lyrics @lyrics.txt
kob music transform song.wav --mode cover --caption "synthwave"
kob music understand song.wav
kob music suggest "a song about late-night drives"
kob music draft-lyrics --caption "dreamy synthwave"

kob song generate "dream pop, 90bpm" --lyrics @lyrics.txt
kob song clone --full-mix ref.wav --description "the same but brighter"
kob song separate mix.wav                # → vocal + bgm stems

kob sfx generate "a heavy wooden door slam" --seconds 4
kob sfx edit room.wav --mode a2a "the same room, underwater"

kob video generate "a dragon banks over a city" --image opener.png
kob video generate "shot one" "shot two" --engine ltx --save-partials
kob video generate "a slow dolly in" --engine wan --mode i2v --image face.png
kob video jobs --engine ltx              # list, or --id <job> to re-attach
kob video props --engine wan
kob video relip --video clip.mp4 --audio vo.wav

kob avatar --image face.png --tts-script "hi there" -o clip.webm
kob avatar --asset-id <id> --audio voice.wav
kob avatar assets                        # list saved portraits

kob vision classify cat.png --prompt "a cat" --prompt "a dog"

kob gen list --service images
kob gen download <id> -o out.png

kob health                               # koblem + every GPU service
kob gpu status                           # scheduler / VRAM budget
kob keys create ci-bot                   # mint an API key
```

`@PATH` reads the value from a file (`@-` from stdin) for the long-text flags —
`--lyrics`, `--tts-script`, `--refine-source`.

Every render command takes `--no-save` to keep the result out of the gallery.
`kob video generate` and `kob avatar` run as async jobs: `--detach` prints the
job id instead of waiting, and `kob video jobs --id <id>` re-attaches to a render
still in flight (a dropped connection no longer kills the client).

Video flags are engine-specific and the CLI rejects the ones an engine cannot
honour rather than letting the server drop them silently: `--model`, `--profile`,
`--upscaler-ratio`, `--dims-are-target`, `--audio`, `--bgm` and `--save-partials`
are LTX-2.3 only; `--mode`, `--frames`, `--quant` and `--reference` are Wan only.

## Global flags

- `--json` — print the raw JSON response instead of writing files.
- `-o, --output PATH` — override the output path (per command).
- `--no-wait` — fail immediately on `503` (GPU busy) instead of retrying.
- `--max-wait <secs>` — cap total time spent waiting out 503s (default 600).

## Shell completions

```bash
kob completions bash > /etc/bash_completion.d/kob
kob completions zsh  > ~/.zfunc/_kob
kob completions fish > ~/.config/fish/completions/kob.fish
```

## Output handling

- Image / rmbg results (base64-in-JSON) are decoded to PNGs; batches get
  `-1`/`-2` suffixes; chained cutouts become `<name>-cutout.png`.
- Music / song / sfx / avatar return server-saved URLs — the CLI downloads them
  (authed) to the server's filename, or to `-o` when there is exactly one file.
  A batch keeps the server names and says so rather than dropping `-o` quietly.
- TTS audio streams to `speech.<format>` (or `-o`).
- Video clips land at `<engine>-<job id>.webm` (or `-o`), so back-to-back
  renders don't overwrite each other. `--save-partials` writes each shot beside
  the clip as `<stem>-shot<N>.webm`.
