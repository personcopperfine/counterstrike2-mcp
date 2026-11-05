# Thin layer over the cs2-docker runtime image. STUB - fill in per workflows/01-docker-runtime.md.
# Build context: this directory. Base: the published client image, or a custom build from a clone
# of https://github.com/personcopperfine/cs2-docker (`podman build -t <tag> <clone>/client`, then
# `--build-arg BASE=<tag>`).
ARG BASE=ghcr.io/personcopperfine/cs2-docker:latest
FROM ${BASE}
# OCR + headless display candidates (apt names to be confirmed on steamrt4/trixie):
#   tesseract-ocr tesseract-ocr-eng   (OCR)
#   weston xwayland                   (headless Wayland compositor + Xwayland - Wine/DXVK known-good on host Xwayland)
#   xserver-xorg-video-dummy          (alternative: Xorg "dummy" driver with a connected RandR output)
# RUN apt-get update && apt-get install -y --no-install-recommends tesseract-ocr tesseract-ocr-eng && rm -rf /var/lib/apt/lists/*
COPY entrypoint.sh /opt/cs2-mcp/entrypoint.sh
ENTRYPOINT ["/opt/cs2-mcp/entrypoint.sh"]
