/**
 * wscrpt permission gate for Pi Coding Agent (RPC / TUI).
 *
 * Pi does not prompt before tools by default. Without this extension,
 * wscrpt's Needs You / Esc w A path never fires for tool calls.
 *
 * Loaded by wscrpt via:
 *   pi --mode rpc --extension <path>/wscrpt-permission-gate.ts
 *
 * In RPC mode, ctx.ui.confirm becomes extension_ui_request on stdout;
 * wscrpt answers with extension_ui_response (Esc w A allow / deny).
 *
 * Fail closed when no UI is available (print/json modes).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

/** Tools that may mutate the tree or run shell — always confirm. */
const CONFIRM_TOOLS = new Set(["bash", "write", "edit", "remove", "move", "rename"]);

function summarizeTool(toolName: string, input: Record<string, unknown>): string {
  switch (toolName) {
    case "bash": {
      const cmd = typeof input.command === "string" ? input.command : "";
      const clipped = cmd.length > 200 ? `${cmd.slice(0, 197)}...` : cmd;
      return clipped ? `bash: ${clipped}` : "bash (empty command)";
    }
    case "write":
    case "edit":
    case "read": {
      const path =
        typeof input.path === "string"
          ? input.path
          : typeof input.file_path === "string"
            ? input.file_path
            : "(unknown path)";
      return `${toolName}: ${path}`;
    }
    default: {
      try {
        const raw = JSON.stringify(input);
        const clipped = raw.length > 160 ? `${raw.slice(0, 157)}...` : raw;
        return `${toolName}: ${clipped}`;
      } catch {
        return toolName;
      }
    }
  }
}

export default function (pi: ExtensionAPI) {
  pi.on("tool_call", async (event, ctx) => {
    const toolName = event.toolName ?? "";
    if (!CONFIRM_TOOLS.has(toolName)) {
      return;
    }

    if (!ctx.hasUI) {
      return {
        block: true,
        reason: "wscrpt permission gate: no UI (fail closed)",
      };
    }

    const input =
      event.input && typeof event.input === "object"
        ? (event.input as Record<string, unknown>)
        : {};
    const detail = summarizeTool(toolName, input);
    const ok = await ctx.ui.confirm("wscrpt · Allow tool?", detail);
    if (!ok) {
      return {
        block: true,
        reason: "Denied by user in wscrpt (Esc w n / cancel)",
      };
    }
    // Allow tool to proceed.
  });
}
