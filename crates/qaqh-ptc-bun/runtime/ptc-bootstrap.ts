/**
 * QAQH PTC runner (bun)。
 *
 * 纯 TS：不 import 任何 `node:` 模块，也不需要构建步骤——`bun <this file>` 直接跑。
 * 协议：stdin/stdout 上的 NDJSON，帧定义见 `src/protocol.rs`。
 *
 * 三条不变量：
 * 1. 控制帧只走 `writeRaw`（打补丁前的 stdout 写句柄），程序输出永不混入通道；
 * 2. 程序的 `console` / `process.stdout` / `process.stderr` 全部折叠成 `log` 帧；
 * 3. 完成值必须是无损 JSON：`undefined` 不携带 value，`null` 携带 `value: null`。
 */

type NamespaceDecl = {
  global: string
  names: string[]
  errorClass?: { name: string; memberNameProperty: string }
}

type RunFrame = {
  t: "run"
  code: string
  namespaces: NamespaceDecl[]
  maxOutputBytes: number
}

type ResultFrame = { t: "result"; id: number; ok: boolean; value?: unknown; error?: string }

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void }

const encoder = new TextEncoder()
const decoder = new TextDecoder()

/** 打补丁前抓到的 stdout 原始写句柄；控制帧只走它。 */
const writeRaw = process.stdout.write.bind(process.stdout)

function send(frame: unknown): void {
  writeRaw(encoder.encode(`${JSON.stringify(frame)}\n`))
}

function describe(error: unknown): string {
  if (error instanceof Error) {
    const stack = typeof error.stack === "string" ? error.stack : error.message
    return stack.length > 8_000 ? `${stack.slice(0, 8_000)}…` : stack
  }
  return String(error)
}

// ── 输出捕获 ────────────────────────────────────────────────────────────────

function render(value: unknown): string {
  if (typeof value === "string") return value
  if (typeof value === "object" && value !== null) {
    try {
      const json = JSON.stringify(value)
      if (json !== undefined) return json
    } catch {
      // 循环引用等：退回 String()
    }
  }
  return String(value)
}

function emitLog(text: string): void {
  if (text.length > 0) send({ t: "log", text })
}

const consoleShim = Object.create(null) as Record<string, (...args: unknown[]) => void>
for (const level of ["log", "info", "warn", "error", "debug"]) {
  consoleShim[level] = (...args: unknown[]): void => {
    emitLog(args.map(render).join(" "))
  }
}
;(globalThis as Record<string, unknown>).console = consoleShim

function patchStream(stream: { write: (...args: never[]) => unknown }): void {
  ;(stream as { write: (chunk: unknown, ...rest: unknown[]) => boolean }).write = (
    chunk: unknown,
    ...rest: unknown[]
  ): boolean => {
    emitLog(typeof chunk === "string" ? chunk : decoder.decode(chunk as Uint8Array))
    const callback = rest.find((arg) => typeof arg === "function") as
      | ((error?: unknown) => void)
      | undefined
    if (callback) queueMicrotask(() => callback(null))
    return true
  }
}

patchStream(process.stdout as unknown as { write: (...args: never[]) => unknown })
patchStream(process.stderr as unknown as { write: (...args: never[]) => unknown })

// ── 无损 JSON ──────────────────────────────────────────────────────────────

function toLosslessJson(value: unknown, seen = new Set<object>()): unknown {
  if (value === null) return null
  switch (typeof value) {
    case "boolean":
    case "string":
      return value
    case "number":
      if (!Number.isFinite(value)) throw new Error("non-finite number is not lossless JSON")
      return value
    case "object": {
      const object = value as object
      if (seen.has(object)) throw new Error("cyclic value is not lossless JSON")
      seen.add(object)
      try {
        if (Array.isArray(value)) return value.map((item) => toLosslessJson(item, seen))
        const out: Record<string, unknown> = {}
        for (const key of Object.keys(value as Record<string, unknown>)) {
          out[key] = toLosslessJson((value as Record<string, unknown>)[key], seen)
        }
        return out
      } finally {
        seen.delete(object)
      }
    }
    default:
      throw new Error(`${typeof value} is not lossless JSON`)
  }
}

// ── 绑定调用 ────────────────────────────────────────────────────────────────

let sequence = 0
const pending = new Map<number, Pending>()

function callBinding(global: string, name: string, args: unknown): Promise<unknown> {
  const id = ++sequence
  return new Promise<unknown>((resolve, reject) => {
    pending.set(id, { resolve, reject })
    send({ t: "call", id, global, name, args })
  })
}

function makeErrorClass(name: string, memberNameProperty: string): new (member: string, message: string) => Error {
  return class BindingCallError extends Error {
    constructor(member: string, message: string) {
      super(message)
      this.name = name
      ;(this as Record<string, unknown>)[memberNameProperty] = member
    }
  }
}

function wrapFailure(errorClass: ReturnType<typeof makeErrorClass> | undefined, member: string, error: unknown): Error {
  const message = error instanceof Error ? error.message : String(error)
  return errorClass ? new errorClass(member, message) : new Error(message)
}

function makeNamespace(
  decl: NamespaceDecl,
  errorClass: ReturnType<typeof makeErrorClass> | undefined,
): Record<string, unknown> {
  const namespace = Object.create(null) as Record<string, unknown>
  for (const name of decl.names) {
    Object.defineProperty(namespace, name, {
      enumerable: true,
      value: async (args: unknown): Promise<unknown> => {
        let detached: unknown
        try {
          detached = toLosslessJson(args === undefined ? {} : args)
        } catch (error) {
          throw wrapFailure(errorClass, name, error)
        }
        try {
          return await callBinding(decl.global, name, detached)
        } catch (error) {
          throw wrapFailure(errorClass, name, error)
        }
      },
    })
  }
  return namespace
}

// ── 程序执行 ────────────────────────────────────────────────────────────────

const AsyncFunction = (async () => {}).constructor as new (
  ...args: string[]
) => (...args: unknown[]) => Promise<unknown>

async function executeRun(frame: RunFrame): Promise<void> {
  const names: string[] = []
  const globals = Object.create(null) as Record<string, unknown>
  for (const decl of frame.namespaces) {
    const errorClass = decl.errorClass
      ? makeErrorClass(decl.errorClass.name, decl.errorClass.memberNameProperty)
      : undefined
    globals[decl.global] = makeNamespace(decl, errorClass)
    names.push(decl.global)
  }

  let program: (...args: unknown[]) => Promise<unknown>
  try {
    program = new AsyncFunction(...names, `"use strict";\n${frame.code}`)
  } catch (error) {
    send({ t: "done", error: { kind: "exception", message: describe(error) } })
    return
  }

  try {
    const value = await program(...names.map((name) => globals[name]))
    if (value === undefined) {
      send({ t: "done" })
      return
    }
    let json: unknown
    try {
      json = toLosslessJson(value)
    } catch (error) {
      send({ t: "done", error: { kind: "invalid-output", message: describe(error) } })
      return
    }
    send({ t: "done", value: json })
  } catch (error) {
    send({ t: "done", error: { kind: "exception", message: describe(error) } })
  }
}

function handleFrame(frame: unknown): void {
  const record = frame as Record<string, unknown> | null
  switch (record?.t) {
    case "run":
      void executeRun(record as unknown as RunFrame)
      return
    case "result": {
      const reply = record as unknown as ResultFrame
      const entry = pending.get(reply.id)
      if (!entry) return
      pending.delete(reply.id)
      if (reply.ok) entry.resolve(reply.value)
      else entry.reject(new Error(reply.error ?? "binding failed"))
      return
    }
    case "shutdown":
      process.exit(0)
  }
}

// ── 主循环 ──────────────────────────────────────────────────────────────────

send({ t: "ready" })

let buffer = ""
for await (const chunk of Bun.stdin.stream()) {
  buffer += decoder.decode(chunk, { stream: true })
  let index = buffer.indexOf("\n")
  while (index >= 0) {
    const line = buffer.slice(0, index)
    buffer = buffer.slice(index + 1)
    if (line.length > 0) {
      try {
        handleFrame(JSON.parse(line))
      } catch (error) {
        send({ t: "done", error: { kind: "protocol", message: describe(error) } })
        process.exit(1)
      }
    }
    index = buffer.indexOf("\n")
  }
}
