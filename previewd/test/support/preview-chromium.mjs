import { spawn } from "node:child_process";
import { constants as fsConstants } from "node:fs";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const START_TIMEOUT_MS = 15_000;
const MAX_DIAGNOSTIC_BYTES = 32 * 1024;

function boundedAppend(current, chunk) {
  const next = `${current}${String(chunk)}`;
  return next.length <= MAX_DIAGNOSTIC_BYTES
    ? next
    : next.slice(next.length - MAX_DIAGNOSTIC_BYTES);
}

function childExit(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve(true);
  }
  return new Promise((resolvePromise) => {
    const onExit = () => {
      clearTimeout(timer);
      resolvePromise(true);
    };
    const timer = setTimeout(() => {
      child.off("exit", onExit);
      resolvePromise(false);
    }, timeoutMs);
    child.once("exit", onExit);
  });
}

export async function waitForValue(
  description,
  operation,
  { timeoutMs = 10_000, intervalMs = 50 } = {},
) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const value = await operation();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolvePromise) => setTimeout(resolvePromise, intervalMs));
  }
  const detail = lastError instanceof Error ? `; last error: ${lastError.message}` : "";
  throw new Error(`timed out waiting for ${description}${detail}`);
}

async function readDevToolsPort(profileDirectory, child, diagnostics) {
  const path = join(profileDirectory, "DevToolsActivePort");
  return waitForValue(
    "Chrome DevToolsActivePort",
    async () => {
      if (child.exitCode !== null || child.signalCode !== null) {
        throw new Error(
          `Chrome exited before CDP became ready (code=${child.exitCode}, signal=${child.signalCode})\n${diagnostics()}`,
        );
      }
      let contents;
      try {
        contents = await readFile(path, "utf8");
      } catch (error) {
        if (error?.code === "ENOENT") return null;
        throw error;
      }
      const [rawPort] = contents.trim().split(/\r?\n/u);
      const port = Number(rawPort);
      if (!Number.isSafeInteger(port) || port < 1 || port > 65_535) {
        throw new Error(`Chrome wrote an invalid DevTools port: ${rawPort}`);
      }
      try {
        const response = await fetch(`http://127.0.0.1:${port}/json/version`, {
          signal: AbortSignal.timeout(1_000),
        });
        if (!response.ok) return null;
      } catch {
        return null;
      }
      return port;
    },
    { timeoutMs: START_TIMEOUT_MS, intervalMs: 50 },
  );
}

/** Launches an isolated, owned Chrome process with CDP bound to numeric loopback. */
export async function launchPreviewChrome(executable) {
  if (typeof executable !== "string" || executable.trim() === "") {
    throw new Error(
      "WSCRPT_PREVIEW_CHROME must name the Chrome/Chromium executable, not its .app directory",
    );
  }
  try {
    await access(executable, fsConstants.X_OK);
  } catch {
    throw new Error(`WSCRPT_PREVIEW_CHROME is not executable: ${executable}`);
  }

  const profileDirectory = await mkdtemp(join(tmpdir(), "wscrpt-preview-chrome-"));
  const args = [
    `--user-data-dir=${profileDirectory}`,
    "--remote-debugging-address=127.0.0.1",
    "--remote-debugging-port=0",
    "--headless=new",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-features=MediaRouter,Translate",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-device-scale-factor=1",
    "--metrics-recording-only",
    "--mute-audio",
    "--no-proxy-server",
    "--autoplay-policy=no-user-gesture-required",
    "--window-size=1280,720",
    ...(typeof process.getuid === "function" && process.getuid() === 0 ? ["--no-sandbox"] : []),
    "about:blank",
  ];
  const child = spawn(executable, args, {
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  let stdout = "";
  let stderr = "";
  let spawnError = null;
  child.stdout?.on("data", (chunk) => {
    stdout = boundedAppend(stdout, chunk);
  });
  child.stderr?.on("data", (chunk) => {
    stderr = boundedAppend(stderr, chunk);
  });
  child.once("error", (error) => {
    spawnError = error;
  });
  const diagnostics = () => [spawnError?.message, stderr.trim(), stdout.trim()].filter(Boolean).join("\n");

  let port;
  try {
    port = await readDevToolsPort(profileDirectory, child, diagnostics);
  } catch (error) {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    if (!(await childExit(child, 2_000))) child.kill("SIGKILL");
    await childExit(child, 2_000);
    await rm(profileDirectory, { recursive: true, force: true });
    throw error;
  }

  let closed = false;
  return {
    child,
    host: "127.0.0.1",
    port,
    profileDirectory,
    diagnostics,
    async close() {
      if (closed) return;
      closed = true;
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
      if (!(await childExit(child, 5_000))) {
        child.kill("SIGKILL");
        await childExit(child, 2_000);
      }
      await rm(profileDirectory, { recursive: true, force: true });
    },
  };
}

export async function evaluate(client, expression) {
  const result = await client.Runtime.evaluate({
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (result.exceptionDetails) {
    const exception = result.exceptionDetails.exception?.description;
    const description = exception ?? result.exceptionDetails.text ?? "browser evaluation failed";
    throw new Error(description);
  }
  return result.result?.value;
}
