const HEADER_LENGTH = 42;
const MAGIC = [0x52, 0x44, 0x50, 0x48];

const canvas = document.querySelector("#desktop");
const context = canvas.getContext("2d", { alpha: false, desynchronized: true });
const status = document.querySelector("#status");
const stats = document.querySelector("#stats");
const stateDot = document.querySelector("#state-dot");
const emptyState = document.querySelector("#empty-state");

let socket;
let frameCount = 0;
let byteCount = 0;
let firstFrameAt = 0;
const decoders = new Map();
const pressedKeys = new Set();

if (!("VideoDecoder" in window)) {
  showError("This browser does not expose WebCodecs VideoDecoder.");
} else {
  connect();
}

function connect() {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  socket = new WebSocket(`${scheme}//${location.host}/ws`);
  socket.binaryType = "arraybuffer";

  socket.addEventListener("open", () => setStatus("Bridge connected", false));
  socket.addEventListener("message", onMessage);
  socket.addEventListener("close", () => {
    closeDecoders();
    showError("Bridge connection closed. Reload to reconnect.");
  });
  socket.addEventListener("error", () => showError("Bridge WebSocket failed."));
}

function onMessage(event) {
  if (typeof event.data === "string") {
    onControl(JSON.parse(event.data));
    return;
  }
  const packet = parsePacket(event.data);
  if (!packet) {
    showError("Bridge sent an invalid video packet.");
    return;
  }
  byteCount += packet.data.byteLength;
  decoderFor(packet.surfaceId).push(packet);
}

function onControl(message) {
  switch (message.type) {
    case "status":
      setStatus(message.message, message.phase === "connected");
      break;
    case "resize":
      resizeCanvas(message.width, message.height);
      break;
    case "egfxNegotiated":
      setStatus(`EGFX ${message.capability} · AVC420`, true);
      break;
    case "warning":
      setStatus(message.message, false);
      break;
    case "error":
      showError(message.message);
      break;
  }
}

function parsePacket(buffer) {
  if (buffer.byteLength < HEADER_LENGTH) return null;
  const bytes = new Uint8Array(buffer);
  if (!MAGIC.every((value, index) => bytes[index] === value) || bytes[4] !== 1) {
    return null;
  }
  const view = new DataView(buffer);
  const dataLength = view.getUint32(38, true);
  if (HEADER_LENGTH + dataLength !== buffer.byteLength || bytes[5] !== 1) {
    return null;
  }
  return {
    keyFrame: (bytes[6] & 1) !== 0,
    timestamp: Number(view.getBigUint64(8, true)),
    frameId: view.getUint32(16, true),
    surfaceId: view.getUint16(20, true),
    x: view.getUint16(22, true),
    y: view.getUint16(24, true),
    width: view.getUint16(26, true),
    height: view.getUint16(28, true),
    outputX: view.getUint32(30, true),
    outputY: view.getUint32(34, true),
    data: new Uint8Array(buffer, HEADER_LENGTH, dataLength),
  };
}

class SurfaceDecoder {
  constructor(surfaceId) {
    this.surfaceId = surfaceId;
    this.pending = [];
    this.metadata = new Map();
    this.configuring = false;
    this.configured = false;
    this.decoder = new VideoDecoder({
      output: (frame) => this.draw(frame),
      error: (error) => showError(`H.264 decoder failed: ${error.message}`),
    });
  }

  push(packet) {
    this.pending.push(packet);
    if (this.configured) {
      this.drain();
    } else if (!this.configuring) {
      void this.configureFromPending();
    }
  }

  async configureFromPending() {
    const keyIndex = this.pending.findIndex((packet) => packet.keyFrame);
    if (keyIndex < 0) return;
    const parameterSets = findParameterSets(this.pending[keyIndex].data);
    if (!parameterSets) return;

    this.configuring = true;
    const config = {
      codec: codecString(parameterSets.sps),
      description: makeAvcConfiguration(parameterSets.sps, parameterSets.pps),
      codedWidth: this.pending[keyIndex].width,
      codedHeight: this.pending[keyIndex].height,
      optimizeForLatency: true,
      hardwareAcceleration: "prefer-hardware",
    };

    try {
      const support = await VideoDecoder.isConfigSupported(config);
      if (!support.supported) {
        showError(`Browser cannot decode ${config.codec}.`);
        return;
      }
      this.decoder.configure(support.config);
      this.configured = true;
      this.pending.splice(0, keyIndex);
      this.drain();
    } catch (error) {
      showError(`Could not configure H.264: ${error.message}`);
    } finally {
      this.configuring = false;
    }
  }

  drain() {
    while (this.pending.length > 0 && this.decoder.decodeQueueSize < 8) {
      const packet = this.pending.shift();
      const key = String(packet.timestamp);
      const queue = this.metadata.get(key) ?? [];
      queue.push(packet);
      this.metadata.set(key, queue);
      try {
        this.decoder.decode(
          new EncodedVideoChunk({
            type: packet.keyFrame ? "key" : "delta",
            timestamp: packet.timestamp,
            data: packet.data,
          }),
        );
      } catch (error) {
        showError(`Could not submit H.264 frame: ${error.message}`);
        return;
      }
    }
  }

  draw(frame) {
    const key = String(frame.timestamp);
    const queue = this.metadata.get(key);
    const packet = queue?.shift();
    if (queue?.length === 0) this.metadata.delete(key);

    if (!packet) {
      frame.close();
      return;
    }

    const sourceWidth = Math.min(packet.width, frame.displayWidth);
    const sourceHeight = Math.min(packet.height, frame.displayHeight);
    context.drawImage(
      frame,
      0,
      0,
      sourceWidth,
      sourceHeight,
      packet.outputX + packet.x,
      packet.outputY + packet.y,
      packet.width,
      packet.height,
    );
    frame.close();

    frameCount += 1;
    if (firstFrameAt === 0) firstFrameAt = performance.now();
    emptyState.classList.add("hidden");
    setStatus("Live · AVC420 passthrough", true);
    updateStats();
    this.drain();
  }

  close() {
    this.decoder.close();
    this.pending.length = 0;
    this.metadata.clear();
  }
}

function decoderFor(surfaceId) {
  let decoder = decoders.get(surfaceId);
  if (!decoder) {
    decoder = new SurfaceDecoder(surfaceId);
    decoders.set(surfaceId, decoder);
  }
  return decoder;
}

function findParameterSets(data) {
  let sps;
  let pps;
  for (const nal of avcNals(data)) {
    const type = nal[0] & 0x1f;
    if (type === 7) sps = nal;
    if (type === 8) pps = nal;
  }
  return sps && pps ? { sps, pps } : null;
}

function* avcNals(data) {
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  let offset = 0;
  while (offset + 4 <= data.byteLength) {
    const length = view.getUint32(offset);
    offset += 4;
    if (length === 0 || offset + length > data.byteLength) return;
    yield data.subarray(offset, offset + length);
    offset += length;
  }
}

function codecString(sps) {
  return `avc1.${[sps[1], sps[2], sps[3]]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("")
    .toUpperCase()}`;
}

function makeAvcConfiguration(sps, pps) {
  const result = new Uint8Array(11 + sps.byteLength + pps.byteLength);
  let offset = 0;
  result[offset++] = 1;
  result[offset++] = sps[1];
  result[offset++] = sps[2];
  result[offset++] = sps[3];
  result[offset++] = 0xff;
  result[offset++] = 0xe1;
  result[offset++] = sps.byteLength >> 8;
  result[offset++] = sps.byteLength & 0xff;
  result.set(sps, offset);
  offset += sps.byteLength;
  result[offset++] = 1;
  result[offset++] = pps.byteLength >> 8;
  result[offset++] = pps.byteLength & 0xff;
  result.set(pps, offset);
  return result;
}

function resizeCanvas(width, height) {
  if (canvas.width === width && canvas.height === height) return;
  canvas.width = width;
  canvas.height = height;
  context.fillStyle = "#080a0c";
  context.fillRect(0, 0, width, height);
}

function setStatus(message, live) {
  status.textContent = message;
  stateDot.classList.toggle("live", live);
  stateDot.classList.remove("error");
}

function showError(message) {
  status.textContent = message;
  stateDot.classList.remove("live");
  stateDot.classList.add("error");
}

function updateStats() {
  const seconds = Math.max((performance.now() - firstFrameAt) / 1000, 0.001);
  stats.textContent = `${frameCount} frames · ${(frameCount / seconds).toFixed(1)} fps · ${(
    byteCount /
    1024 /
    1024
  ).toFixed(1)} MB`;
}

function closeDecoders() {
  for (const decoder of decoders.values()) decoder.close();
  decoders.clear();
}

function send(message) {
  if (socket?.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message));
  }
}

function remotePoint(event) {
  const bounds = canvas.getBoundingClientRect();
  return {
    x: Math.round(((event.clientX - bounds.left) / bounds.width) * canvas.width),
    y: Math.round(((event.clientY - bounds.top) / bounds.height) * canvas.height),
  };
}

let pendingPointer;
let pointerAnimation;
canvas.addEventListener("pointermove", (event) => {
  pendingPointer = remotePoint(event);
  if (pointerAnimation) return;
  pointerAnimation = requestAnimationFrame(() => {
    send({ type: "mouseMove", ...pendingPointer });
    pointerAnimation = undefined;
  });
});

canvas.addEventListener("pointerdown", (event) => {
  canvas.focus();
  canvas.setPointerCapture(event.pointerId);
  send({ type: "mouseMove", ...remotePoint(event) });
  send({
    type: "mouseButton",
    button: ["left", "middle", "right"][event.button] ?? "left",
    pressed: true,
  });
  event.preventDefault();
});

canvas.addEventListener("pointerup", (event) => {
  send({ type: "mouseMove", ...remotePoint(event) });
  send({
    type: "mouseButton",
    button: ["left", "middle", "right"][event.button] ?? "left",
    pressed: false,
  });
  event.preventDefault();
});

canvas.addEventListener(
  "wheel",
  (event) => {
    send({ type: "wheel", dx: event.deltaX, dy: event.deltaY });
    event.preventDefault();
  },
  { passive: false },
);
canvas.addEventListener("contextmenu", (event) => event.preventDefault());

canvas.addEventListener("keydown", (event) => {
  if (!event.repeat) {
    pressedKeys.add(event.code);
    send({ type: "key", code: event.code, pressed: true });
  }
  event.preventDefault();
});

canvas.addEventListener("keyup", (event) => {
  pressedKeys.delete(event.code);
  send({ type: "key", code: event.code, pressed: false });
  event.preventDefault();
});

window.addEventListener("blur", () => {
  for (const code of pressedKeys) {
    send({ type: "key", code, pressed: false });
  }
  pressedKeys.clear();
});
