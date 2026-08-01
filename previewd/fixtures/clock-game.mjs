const canvas = document.querySelector("canvas#clock-game");
if (!(canvas instanceof HTMLCanvasElement) || canvas.width !== 1280 || canvas.height !== 720) {
  throw new Error("clock fixture requires exactly one 1280x720 canvas#clock-game");
}

const context = canvas.getContext("2d", { alpha: false, desynchronized: true });
if (!context) throw new Error("clock fixture 2D context is unavailable");

const FIXTURE_FLASH_REQUEST_EVENT = "wscrpt-preview-fixture-flash-v1";
const FIXTURE_FLASH_RESULT_EVENT = "wscrpt-preview-fixture-flash-result-v1";
const FIXTURE_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
const FIXTURE_REQUEST_ID = /^[a-f0-9]{32}$/u;

const state = {
  sequence: 0,
  startedAt: performance.now(),
  lastDrawAt: 0,
  flashId: null,
  flashRgb: [18, 30, 48],
};

function drawSequenceBits(sequence) {
  const x = 96;
  const y = 566;
  const size = 22;
  for (let bit = 0; bit < 32; bit += 1) {
    context.fillStyle = (sequence >>> bit) & 1 ? "#f5c45b" : "#182337";
    context.fillRect(x + bit * (size + 8), y, size, 52);
  }
}

function render(now) {
  state.sequence = (state.sequence + 1) >>> 0;
  state.lastDrawAt = now;
  const elapsed = now - state.startedAt;
  const phase = (state.sequence % 240) / 240;

  context.fillStyle = "#07101c";
  context.fillRect(0, 0, canvas.width, canvas.height);

  context.fillStyle = "#0d1c2d";
  for (let y = 0; y < canvas.height; y += 80) context.fillRect(0, y, canvas.width, 1);
  for (let x = 0; x < canvas.width; x += 80) context.fillRect(x, 0, 1, canvas.height);

  context.fillStyle = "#dce7f5";
  context.font = "600 54px ui-monospace, SFMono-Regular, Menlo, monospace";
  context.fillText("wscrpt · exact-session clock", 96, 150);
  context.fillStyle = "#7f96b2";
  context.font = "30px ui-monospace, SFMono-Regular, Menlo, monospace";
  context.fillText("canvas#clock-game · 1280×720", 96, 208);

  context.fillStyle = "#63d6b5";
  context.font = "700 88px ui-monospace, SFMono-Regular, Menlo, monospace";
  context.fillText(String(state.sequence).padStart(10, "0"), 96, 360);
  context.fillStyle = "#91a5bd";
  context.font = "32px ui-monospace, SFMono-Regular, Menlo, monospace";
  context.fillText(`${elapsed.toFixed(1).padStart(12, " ")} ms source time`, 100, 414);

  context.fillStyle = "#17273a";
  context.fillRect(96, 468, 1024, 34);
  context.fillStyle = "#67a7ff";
  context.fillRect(96, 468, Math.max(4, Math.floor(1024 * phase)), 34);
  drawSequenceBits(state.sequence);

  // The request-to-glass probe samples pixel (0, 0); keep the entire corner
  // patch flat and un-antialiased so video compression remains detectable.
  const [red, green, blue] = state.flashRgb;
  context.fillStyle = `rgb(${red}, ${green}, ${blue})`;
  context.fillRect(0, 0, 64, 64);
}

function animate(now) {
  render(now);
  requestAnimationFrame(animate);
}

function flash({ id, rgb }) {
  if (
    typeof id !== "string" ||
    !Array.isArray(rgb) ||
    rgb.length !== 3 ||
    rgb.some((component) => !Number.isFinite(component))
  ) {
    throw new Error("invalid latency flash request");
  }
  state.flashId = id;
  state.flashRgb = rgb.map((component) => Math.max(0, Math.min(255, Math.round(component))));
  render(performance.now());
  return { id, rgb: [...state.flashRgb], sequence: state.sequence, sourceAt: state.lastDrawAt };
}

function snapshot() {
  return {
    sequence: state.sequence,
    sourceAt: state.lastDrawAt,
    flashId: state.flashId,
    flashRgb: [...state.flashRgb],
    width: canvas.width,
    height: canvas.height,
  };
}

document.addEventListener(FIXTURE_FLASH_REQUEST_EVENT, (event) => {
  if (typeof event?.detail !== "string" || event.detail.length > 512) return;
  let request;
  try {
    request = JSON.parse(event.detail);
  } catch {
    return;
  }
  if (
    !request ||
    typeof request !== "object" ||
    Array.isArray(request) ||
    Object.keys(request).some(
      (key) => key !== "requestId" && key !== "id" && key !== "rgb",
    ) ||
    !FIXTURE_REQUEST_ID.test(request.requestId ?? "") ||
    !FIXTURE_ID.test(request.id ?? "") ||
    !Array.isArray(request.rgb) ||
    request.rgb.length !== 3 ||
    request.rgb.some(
      (component) => !Number.isInteger(component) || component < 0 || component > 255,
    )
  ) {
    return;
  }
  const result = flash({ id: request.id, rgb: request.rgb });
  document.dispatchEvent(
    new CustomEvent(FIXTURE_FLASH_RESULT_EVENT, {
      detail: JSON.stringify({ requestId: request.requestId, sequence: result.sequence }),
    }),
  );
});

globalThis.__wscrptPreviewFixture = Object.freeze({ version: 1, flash, snapshot });
requestAnimationFrame(animate);
