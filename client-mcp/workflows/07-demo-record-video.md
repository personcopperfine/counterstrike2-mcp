# 07 — Demo recording -> video

## Goal
Record what a test or playtest did as a SourceTV demo, then turn a demo into a video file the
model (or a human) can review, using the engine's own recorder; artifacts land in the run dir.

## Prerequisites
- 03, 04. Read `../idabins/worknotes/movie-scratch.md` (full `startmovie` option list, `CMovieRecorder`,
  fixed-timestep recording, `playdemo --startmovie`), `worknotes/vtest/suites/movie_demo.txt`,
  `check_movie_demo.py`. `ffprobe`/`ffmpeg` in our image layer (01) for judging/transcoding.

## Steps
1. **Record**: `demo_record_start {name}`: requires `tv_enable 1` **before** `map` (server-side
   SourceTV; client `record` freezes input early in a map — do not use it); `tv_delay 0`;
   `tv_record <name>` -> `demo_record_stop` = `tv_stoprecord`; returns host path
   `<csgo>/<name>.dem`. Wire the preamble into `match_setup`/`run_testscript` options.
   Implemented (`tools/video.rs`): both tools read the console.log cursor before sending, then
   confirm from that cursor: start waits up to 2 s for `[HLTV Server] Recording SourceTV replay from`
   (`recording: true`; timeout is `RecordingNotStarted`, retry-safe, carrying the tv_enable note; no
   resolvable log is `recording: "unconfirmed"`); stop waits up to 5 s for
   `[HLTV Server] Completed SourceTV demo "<name>.dem"` (timeout is `DemoNotCompleted`). Without a log,
   stop falls back to the .dem size settling. A .dem older than the call is `DemoStale`.
2. **Video**: `demo_to_video {demo, name, framerate:30, format:webm, width?, height?, bitrate?, countFrames?, timeoutMs?}`
   (ffprobe and ffmpeg: `--ffprobe` / `--ffmpeg` or PATH; absent tools mean their checks are listed in `skipped`)
   runs a suite (always under wine-tools, on every flavour: the proven engine encoder path; `timeoutMs`
   bounds it as in run_testscript; `countFrames` opts into `ffprobe -count_frames`, default counts packets):
   `playdemo <dem> --startmovie <name> webm framerate 30 no_unique_dir` and waits for
   `DemoPlaybackFinished` (recorder auto-`endmovie`s), then `quit`. Output `<csgo>/<name>.webm`
   (AV1 via CPU AOM under Wine + Vorbis). Judge with `ffprobe` (stream count, frames ~= recorder's
   "Finishing movie" count) and a contact sheet (`research/contact.py` as `check_movie_demo.py`).
3. **Frame-accurate variants**: `startpaused` + unpause on a checkpoint, or manual `startmovie`
   after `demo_gototick`, to cut the loading-screen pre-roll (337 vs 330 frames observed);
   `cl_drawhud 0`/`demoui` off for clean frames; `spec_player`/`spec_mode` for POV.
4. **Linux**: no `libvalve_webm.so`/`wmf`/`avi` in the Linux bin dir -> use `tga|png|jpg` frame
   dumps (`startmovie <name> png framerate 30`) + `ffmpeg` in-container to mux; verify `startmovie`
   is available without `-tools` (it is `FCVAR_DEVELOPMENTONLY`; retail untested).
   **Decision**: `demo_to_video` does not refuse webm on linux-client; it always runs under wine-tools.
   **TODO**: the Linux frame-dump runner (png frames + in-container ffmpeg mux) is not written.
5. **Playtest tie-in** (08): `observe` can run while `tv_record` is on so every playtest has a demo;
   `demo_to_video` afterwards for review; optional per-step PNGs instead.
6. Tests: option builder unit tests; live test on the move_jump demo.

## Definition of done
- `demo_record_*` around a test yields a `.dem`; `demo_to_video` yields a `.webm` whose frame count
  matches the recorder's log within 2%, with a contact sheet PNG — on WineTools; Linux path has a
  go/no-go note.

## Open questions
- `mp4` (Media Foundation under Wine, likely absent), NVENC under Wine (CUDA init fails -> CPU), live
  (non-demo) `startmovie` during a test.
- Retail (non-`-tools`) availability of `startmovie` (`FCVAR_DEVELOPMENTONLY`).
- Linux encoder backends: confirm by `strings libengine2.so | grep -i webm` and a run.

## Evidence
- Two PASS runs: `playdemo vtest_src --startmovie vtest_movie webm framerate 30 no_unique_dir` ->
  AV1 1024x720@30 + Vorbis, decoded frames == recorder count [proven, 2026-10-10].
