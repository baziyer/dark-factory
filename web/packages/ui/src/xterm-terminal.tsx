"use client";

import { useEffect, useRef } from "react";
import type { TerminalSurface } from "./terminal-controller.js";

type XtermModules = Readonly<{
  Terminal: typeof import("@xterm/xterm").Terminal;
  FitAddon: typeof import("@xterm/addon-fit").FitAddon;
}>;

type ResizeWindow = Readonly<{
  addEventListener(type: "resize", listener: () => void): void;
  removeEventListener(type: "resize", listener: () => void): void;
}>;

type XtermCallbacks = Readonly<{
  onSurface: (surface: TerminalSurface | undefined) => void;
  onError: () => void;
  onData?: (value: string) => void;
  onBinary?: (value: string) => void;
  onResize?: (rows: number, cols: number) => void;
}>;

export type XtermTerminalProps = XtermCallbacks;

export function loadXtermModules(): Promise<XtermModules> {
  return Promise.all([import("@xterm/xterm"), import("@xterm/addon-fit")]).then(([xterm, fit]) => {
    const Terminal = moduleConstructor(xterm, "Terminal");
    const FitAddon = moduleConstructor(fit, "FitAddon");
    if (typeof Terminal !== "function" || typeof FitAddon !== "function") {
      throw new Error("terminal display unavailable");
    }
    return { Terminal: Terminal as XtermModules["Terminal"], FitAddon: FitAddon as XtermModules["FitAddon"] };
  });
}

function moduleConstructor(module: Record<string, unknown>, name: "Terminal" | "FitAddon"): unknown {
  if (typeof module[name] === "function") return module[name];
  const defaultExport = module.default;
  if (defaultExport !== null && typeof defaultExport === "object") {
    return (defaultExport as Record<string, unknown>)[name];
  }
  return undefined;
}

/**
 * The display surface is sealed before xterm is disposed. This keeps a late
 * xterm write callback from resolving a client output callback after teardown.
 */
export function createTerminalSurface(terminal: Pick<InstanceType<XtermModules["Terminal"]>, "write">): TerminalSurface {
  type Pending = { resolve: () => void; reject: (error: Error) => void };
  const pendingWrites = new Set<Pending>();
  let aborted = false;

  return {
    write(payload) {
      if (aborted) return Promise.reject(new Error("terminal display disposed"));
      return new Promise<void>((resolve, reject) => {
        if (aborted) {
          reject(new Error("terminal display disposed"));
          return;
        }
        const pending = { resolve, reject };
        pendingWrites.add(pending);
        try {
          terminal.write(payload, () => {
            if (pendingWrites.delete(pending)) resolve();
          });
        } catch {
          pendingWrites.delete(pending);
          reject(new Error("terminal display failed"));
        }
      });
    },
    abort() {
      if (aborted) return;
      aborted = true;
      const error = new Error("terminal display disposed");
      for (const pending of pendingWrites) pending.reject(error);
      pendingWrites.clear();
    },
  };
}

/** Internal lifecycle helper, kept out of the package root exports. */
export function startXtermTerminal(
  container: () => HTMLDivElement | null,
  loadModules: () => Promise<XtermModules>,
  callbacks: XtermCallbacks,
  windowTarget: ResizeWindow,
): () => void {
  let disposed = false;
  let cleanup: (() => void) | undefined;
  void loadModules().then((modules) => {
    if (disposed) return;
    const element = container();
    if (element === null) return;
    cleanup = mountXtermTerminal(element, modules, callbacks, windowTarget);
    if (disposed) {
      cleanup();
      cleanup = undefined;
    }
  }).catch(() => {
    if (disposed) return;
    try { callbacks.onError(); } catch { /* failure reporting cannot own teardown */ }
  });
  return () => {
    disposed = true;
    cleanup?.();
    cleanup = undefined;
  };
}

function mountXtermTerminal(element: HTMLDivElement, modules: XtermModules, callbacks: XtermCallbacks, windowTarget: ResizeWindow): () => void {
  type TerminalInstance = InstanceType<XtermModules["Terminal"]>;
  let terminal: TerminalInstance | undefined;
  let fit: InstanceType<XtermModules["FitAddon"]> | undefined;
  let surface: TerminalSurface | undefined;
  let data: { dispose(): void } | undefined;
  let binary: { dispose(): void } | undefined;
  let resized: { dispose(): void } | undefined;
  let listenerInstalled = false;
  let surfacePublished = false;
  let disposed = false;
  const safe = (action: () => void): void => {
    try { action(); } catch { /* teardown continues through each owned resource */ }
  };
  const onWindowResize = () => { if (!disposed) fit?.fit(); };
  const cleanup = () => {
    if (disposed) return;
    disposed = true;
    if (listenerInstalled) safe(() => windowTarget.removeEventListener("resize", onWindowResize));
    surface?.abort();
    safe(() => data?.dispose());
    safe(() => binary?.dispose());
    safe(() => resized?.dispose());
    if (surfacePublished) safe(() => callbacks.onSurface(undefined));
    safe(() => terminal?.dispose());
  };

  try {
    terminal = new modules.Terminal();
    fit = new modules.FitAddon();
    surface = createTerminalSurface(terminal);
    terminal.loadAddon(fit);
    terminal.open(element);
    let initialResize = true;
    data = terminal.onData((value) => callbacks.onData?.(value));
    binary = terminal.onBinary((value) => callbacks.onBinary?.(value));
    resized = terminal.onResize(({ rows, cols }) => {
      if (!initialResize && !disposed) callbacks.onResize?.(rows, cols);
    });
    fit.fit();
    initialResize = false;
    callbacks.onSurface(surface);
    surfacePublished = true;
    callbacks.onResize?.(terminal.rows, terminal.cols);
    terminal.focus();
    listenerInstalled = true;
    windowTarget.addEventListener("resize", onWindowResize);
  } catch (error) {
    cleanup();
    throw error;
  }
  return cleanup;
}

/** Thin DOM/xterm adapter; terminal authority stays in TerminalController. */
export function XtermTerminal({ onSurface, onError, onData, onBinary, onResize }: XtermTerminalProps) {
  const element = useRef<HTMLDivElement>(null);
  const callbacks = useRef({ onSurface, onError, onData, onBinary, onResize });
  callbacks.current = { onSurface, onError, onData, onBinary, onResize };

  useEffect(() => startXtermTerminal(
    () => element.current,
    loadXtermModules,
    {
      onSurface: (surface) => callbacks.current.onSurface(surface),
      onError: () => callbacks.current.onError(),
      onData: (value) => callbacks.current.onData?.(value),
      onBinary: (value) => callbacks.current.onBinary?.(value),
      onResize: (rows, cols) => callbacks.current.onResize?.(rows, cols),
    },
    window,
  ), []);

  return <div ref={element} className="dfFactoryConsole__terminal" role="application" aria-label="Agent terminal" />;
}
