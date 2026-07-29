# rdpweb

A deliberately small proof of concept for one question: can an RDP host's
MS-RDPEGFX AVC420 stream reach a browser without the gateway decoding or
re-encoding it?

This project is the only bridge between the RDP server and browser. It:

1. connects with IronRDP;
2. accepts `Microsoft::Windows::RDS::Graphics`;
3. advertises EGFX V8.1 with AVC420 only;
4. removes the `RFX_AVC420_BITMAP_STREAM` wrapper;
5. sends the original length-prefixed H.264 access unit over WebSocket; and
6. gives it to the browser's WebCodecs `VideoDecoder`.

The browser builds an `AVCDecoderConfigurationRecord` from the in-band SPS/PPS,
so the H.264 bytes remain in their original AVC format end to end.

## Run

Use a remotex-compatible config:

```sh
cargo run --release -- --config rdpweb.toml --target desktop
```

Or keep the password out of the process arguments:

```sh
RDPWEB_HOST=192.0.2.10 \
RDPWEB_USERNAME=user \
RDPWEB_PASSWORD='change-me' \
cargo run --release
```

Then open <http://127.0.0.1:8080> in a Chromium-family browser and click the
desktop to send keyboard input.

Set `RUST_LOG=debug` for the RDP/EGFX trace.

## Local smoke test

The FreeRDP `sfreerdp-server` sample completes an RDP handshake but does not
open `Microsoft::Windows::RDS::Graphics`; it only exercises the older
RemoteFX/NSCodec path. The included IronRDP test server opens EGFX and sends a
real AVC420 access unit, so it tests the path this POC actually uses.

Create disposable TLS and H.264 fixtures, then start the test RDP server:

```sh
RDPWEB_TEST_DIR="$(mktemp -d)"

openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$RDPWEB_TEST_DIR/server.key" \
  -out "$RDPWEB_TEST_DIR/server.crt" \
  -days 1 -subj '/CN=localhost'

ffmpeg -hide_banner -loglevel error \
  -f lavfi -i 'testsrc2=size=320x180:rate=1' -frames:v 1 \
  -c:v libx264 -preset ultrafast -tune zerolatency \
  -profile:v baseline -pix_fmt yuv420p \
  -x264-params 'keyint=1:scenecut=0' \
  -f h264 "$RDPWEB_TEST_DIR/fixture.h264"

cargo run --example egfx_test_server -- \
  --cert "$RDPWEB_TEST_DIR/server.crt" \
  --key "$RDPWEB_TEST_DIR/server.key" \
  --h264 "$RDPWEB_TEST_DIR/fixture.h264"
```

In another terminal, point this project at the fixture server:

```sh
RDPWEB_HOST=127.0.0.1 \
RDPWEB_PORT=3390 \
RDPWEB_USERNAME=test \
RDPWEB_PASSWORD=test \
cargo run
```

Open <http://127.0.0.1:8080>. The fixture server sends 12 IDR frames; the page
should report `Live · AVC420 passthrough`.

## POC boundary

- AVC420 only. AVC444 is two dependent H.264 views and needs two decoders plus
  the MS-RDPEGFX chroma reconstruction step.
- No fallback renderer. A host that selects Progressive RemoteFX produces a
  visible warning instead of silently returning to WebP.
- No audio, clipboard, resize/reactivation, remote cursor shapes, auth, or
  multi-client session management.
- The server binds loopback by default because target credentials remain in the
  bridge process and the HTTP/WebSocket endpoint has no authentication.
