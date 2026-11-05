# docker/ — "our own image" bits

The submodules (`cs2-docker/client/Dockerfile`, `workshop/Dockerfile`) own the game/runtime
images (published as `ghcr.io/personcopperfine/cs2-docker` and `ghcr.io/personcopperfine/cs2-workshop`). This directory holds what THIS server adds on top (a thin layer `FROM ghcr.io/personcopperfine/cs2-docker`):
OCR (`tesseract-ocr` + eng data), a headless display stack candidate, helper binaries
(`vtmsend.exe`, `xdump`, `xkey`), and an entrypoint that launches the session the `docker`
runtime backend asks for. See workflows 01 (runtime) and the headless/vision workflows.
