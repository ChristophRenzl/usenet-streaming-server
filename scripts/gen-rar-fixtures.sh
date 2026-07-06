#!/usr/bin/env bash
# Regenerates the committed RAR test fixtures under tests/fixtures/rar/.
#
# Layout produced:
#   rar5-store.rar               RAR5, store (-m0), payload.bin + small.txt
#   rar5-store-multi.part1..4.rar  RAR5, store, 1 MiB volumes
#   rar5-store-multi-real.part1..4.rar  RAR5, store, real-release-shaped:
#                                companion file first + real playable MKV split
#                                across 1 MiB volumes (+ committed .mkv/.companion)
#   rar5-compressed-multi.part1..4.rar  RAR5, -m3, multi-volume (rejection fixture)
#   rar5-compressed.rar          RAR5, -m5 (rejection fixture)
#   rar5-encrypted.rar           RAR5, -hp encrypted headers (rejection fixture)
#   rar4-store.rar               RAR4, store, payload.bin + small.txt
#   rar4-store-multi.rar,.r00-2  RAR4, store, old-style volume naming
#   rar4-compressed.rar          RAR4 header declaring method 0x33 (rejection fixture)
#   rar4-encrypted.rar           RAR4 main header with MHD_PASSWORD (rejection fixture)
#
# The payload is deterministic (xorshift64* PRNG, seed 0x9E3779B97F4A7C15) and is
# regenerated identically by the Rust test support code (tests/it/support/mod.rs),
# so the raw payload is not committed. The script prints its CRC32, which must
# match `PAYLOAD_CRC32` in the test support module.
#
# RAR 7.x can no longer create RAR4 archives (-ma4 was removed), so the RAR4
# store fixtures are written by the embedded python writer below and then
# validated with `unrar t` (unrar still fully supports reading RAR4). The two
# RAR4 rejection fixtures are header-only synthetics: the parser under test only
# reads headers, and no RAR4-producing tool is available anymore.
#
# Requirements: python3, RARLab rar + unrar (brew install --cask rar).

set -euo pipefail

RAR=${RAR:-/opt/homebrew/bin/rar}
UNRAR=${UNRAR:-/opt/homebrew/bin/unrar}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/tests/fixtures/rar"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$OUT"
# Remove generated archives only. The committed real-release media source files
# (rar5-store-multi-real.mkv / .companion) are preserved unless REGEN_MEDIA is
# set, because ffmpeg output is not bit-reproducible across versions.
rm -f "$OUT"/rar4-*.rar "$OUT"/rar4-*.r[0-9][0-9] "$OUT"/rar5-*.rar

# ---- deterministic inputs ----------------------------------------------------
python3 - "$WORK" <<'PY'
import sys, zlib
work = sys.argv[1]

def xorshift_bytes(n, seed=0x9E3779B97F4A7C15):
    m = (1 << 64) - 1
    x = seed
    out = bytearray(n)
    for i in range(n):
        x ^= x >> 12
        x = (x ^ (x << 25)) & m
        x ^= x >> 27
        out[i] = ((x * 0x2545F4914F6CDD1D) & m) >> 56
    return bytes(out)

payload = xorshift_bytes(3 * 1024 * 1024)
open(f"{work}/payload.bin", "wb").write(payload)
open(f"{work}/small.txt", "wb").write(b"usenet-streaming-server RAR fixture companion file.\n" * 16)
open(f"{work}/compressible.bin", "wb").write(
    (b"The quick brown fox jumps over the lazy dog. 0123456789\n" * ((512 * 1024) // 57 + 1))[: 512 * 1024]
)
print(f"payload.bin crc32 = 0x{zlib.crc32(payload) & 0xFFFFFFFF:08X}")
PY

# ---- RAR5 fixtures (real rar CLI) ---------------------------------------------
(
  cd "$WORK"
  "$RAR" a -ma5 -m0 -ep -idq -y "$OUT/rar5-store.rar" payload.bin small.txt
  "$RAR" a -ma5 -m0 -v1m -ep -idq -y "$OUT/rar5-store-multi.rar" payload.bin small.txt
  "$RAR" a -ma5 -m5 -ep -idq -y "$OUT/rar5-compressed.rar" compressible.bin
  "$RAR" a -ma5 -m0 -hpsecret -ep -idq -y "$OUT/rar5-encrypted.rar" small.txt
)

# ---- Real-release-shaped RAR5 fixtures ---------------------------------------
# Mirror an actual scene release: a small companion file stored *first*, then a
# real, playable MKV split across several store-mode volumes with a QO
# quick-open service block + end-of-archive trailer (verified against the real
# 16-volume "Dune.Part.2.2024...DarQ-HONE" set). The MKV lets ffprobe validate
# byte-alignment; random payloads (used above) can never be probed.
#
# The MKV (rar5-store-multi-real.mkv) and companion (.companion) are committed
# because ffmpeg output is not bit-reproducible across versions. Regenerate them
# only when FFMPEG is set; otherwise the committed copies are repacked.
FFMPEG=${FFMPEG:-ffmpeg}
if [ -n "${REGEN_MEDIA:-}" ] && command -v "$FFMPEG" >/dev/null 2>&1; then
  "$FFMPEG" -y -v error \
    -f lavfi -i "testsrc2=duration=60:size=480x360:rate=24" \
    -f lavfi -i "sine=frequency=440:duration=60" \
    -map 0:v -map 1:a -c:v libx264 -preset medium -crf 28 -g 24 -pix_fmt yuv420p \
    -c:a aac -b:a 96k "$OUT/rar5-store-multi-real.mkv"
  python3 - "$OUT" <<'PY'
import sys
out = sys.argv[1]
open(f"{out}/rar5-store-multi-real.companion", "wb").write(bytes((i * 7) % 256 for i in range(9000)))
PY
fi
(
  cd "$OUT"
  cp rar5-store-multi-real.mkv "$WORK/feature.mkv"
  cp rar5-store-multi-real.companion "$WORK/cover.jpg"
  cd "$WORK"
  rm -f "$OUT"/rar5-store-multi-real.part*.rar "$OUT"/rar5-compressed-multi.part*.rar
  # Companion FIRST, media SECOND (so media data_offset is past the companion).
  "$RAR" a -ma5 -m0 -v1m -qo+ -ep -idq -y "$OUT/rar5-store-multi-real.rar" cover.jpg feature.mkv
  # Compressed multi-volume: must be rejected, not mis-mapped. Use a ~1.4 MB
  # head of the MKV with 512 KiB volumes to keep the rejection fixture compact.
  head -c 1400000 feature.mkv > head.mkv
  "$RAR" a -ma5 -m3 -v512k -qo+ -ep -idq -y "$OUT/rar5-compressed-multi.rar" head.mkv
)

# ---- RAR4 fixtures (python writer, validated with unrar) -----------------------
python3 - "$WORK" "$OUT" <<'PY'
import struct, sys, zlib

work, out = sys.argv[1], sys.argv[2]
payload = open(f"{work}/payload.bin", "rb").read()
small = open(f"{work}/small.txt", "rb").read()

MARKER4 = bytes([0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00])
STORE, FAKE_COMPRESSED = 0x30, 0x33


def block(btype, flags, fields=b"", data=b""):
    size = 7 + len(fields)
    body = struct.pack("<BHH", btype, flags, size) + fields
    crc = zlib.crc32(body) & 0xFFFF
    return struct.pack("<H", crc) + body + data


def main_head(volume=False, encrypted=False):
    flags = (0x0001 if volume else 0) | (0x0080 if encrypted else 0)
    return block(0x73, flags, struct.pack("<HI", 0, 0))


def file_head(name, part, full_size, split_before=False, split_after=False, method=STORE, crc=None):
    # Split semantics (validated against unrar): parts with SPLIT_AFTER store the
    # CRC of that part's data; the final part stores the whole-file CRC.
    flags = 0x8000 | (0x0001 if split_before else 0) | (0x0002 if split_after else 0)
    ftime = ((2026 - 1980) << 25) | (7 << 21) | (4 << 16) | (12 << 11) | (30 << 5)
    fields = struct.pack(
        "<IIBIIBBHI",
        len(part),                      # pack_size (this volume)
        full_size,                      # unp_size (whole file)
        0,                              # host_os: MS-DOS
        crc if crc is not None else zlib.crc32(part) & 0xFFFFFFFF,
        ftime,
        20,                             # version to extract: 2.0
        method,
        len(name),
        0x20,                           # DOS archive attribute
    )
    return block(0x74, flags, fields + name.encode(), part)


def endarc(next_volume=False):
    return block(0x7B, 0x0001 if next_volume else 0)


# Single-volume store archive.
with open(f"{out}/rar4-store.rar", "wb") as f:
    f.write(MARKER4 + main_head())
    f.write(file_head("payload.bin", payload, len(payload)))
    f.write(file_head("small.txt", small, len(small)))
    f.write(endarc())

# Multi-volume store archive, old-style .rar/.r00/.r01 naming, ~1 MiB of data
# per volume.
chunk = 1_000_000
parts = [payload[i : i + chunk] for i in range(0, len(payload), chunk)]
names = [f"{out}/rar4-store-multi.rar"] + [
    f"{out}/rar4-store-multi.r{i:02d}" for i in range(len(parts) - 1)
]
full_crc = zlib.crc32(payload) & 0xFFFFFFFF
for i, (name, part) in enumerate(zip(names, parts)):
    last = i == len(parts) - 1
    with open(name, "wb") as f:
        f.write(MARKER4 + main_head(volume=True))
        f.write(
            file_head(
                "payload.bin",
                part,
                len(payload),
                split_before=i > 0,
                split_after=not last,
                crc=full_crc if last else None,
            )
        )
        if last:
            f.write(file_head("small.txt", small, len(small)))
        f.write(endarc(next_volume=not last))

# Rejection fixtures (header-only synthetics, see file comment).
with open(f"{out}/rar4-compressed.rar", "wb") as f:
    f.write(MARKER4 + main_head())
    f.write(file_head("compressed.bin", b"\xa5" * 1024, 4096, method=FAKE_COMPRESSED))
    f.write(endarc())
with open(f"{out}/rar4-encrypted.rar", "wb") as f:
    f.write(MARKER4 + main_head(encrypted=True))
    f.write(b"\x5a" * 64)  # opaque: real archives encrypt everything after this
PY

# ---- validate everything unrar can read ----------------------------------------
# `unrar t -idq` prints nothing on success; require both exit 0 and no output
# (some checksum problems are reported with a zero exit status).
for a in rar5-store.rar rar5-store-multi.part1.rar rar5-compressed.rar \
         rar5-store-multi-real.part1.rar rar5-compressed-multi.part1.rar \
         rar4-store.rar rar4-store-multi.rar; do
  result=$("$UNRAR" t -y -idq "$OUT/$a" 2>&1) && [ -z "$result" ] \
    || { echo "unrar validation FAILED for $a: $result"; exit 1; }
  echo "unrar t OK: $a"
done

ls -la "$OUT"
