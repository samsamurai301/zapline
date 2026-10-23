
/** Options for {@link createClient}. */
export interface ClientOptions {
  /** Prefix for every path, e.g. "https://api.example.com". Default: same origin. */
  baseUrl?: string;
  /** Extra headers (for example `Authorization`), or a function returning them. */
  headers?: HeadersInit | (() => HeadersInit | Promise<HeadersInit>);
  /** A `fetch` implementation. Default: the global one. */
  fetch?: typeof fetch;
  /** Whether to send cookies. Default: `"same-origin"`. */
  credentials?: RequestCredentials;
  /**
   * In browsers, unsafe requests (POST, PUT, PATCH, DELETE) copy zapline's
   * CSRF cookie into the `x-csrf-token` header, so `Csrf` middleware lets
   * them through. Set the names if you changed them, or `false` to turn it off.
   */
  csrf?: false | { cookie?: string; header?: string };
}

/** Per-call options: the last argument of every method. */
export interface CallOptions {
  /** Aborts the request, e.g. `AbortSignal.timeout(5000)`. */
  signal?: AbortSignal;
  /** Headers for this call only. */
  headers?: HeadersInit;
}

/** A query string or form value. */
export type QueryValue = string | number | boolean | null | undefined | Array<string | number | boolean>;

/** A multipart form value. `Blob`s (and `File`s) are sent as files. */
export type FormValue = string | number | boolean | Blob;

/** One failed validation rule, as listed in a `422` response. */
export interface FieldError {
  /** Path to the field: `email`, `address.zip`, `items[0].name`. Empty for the whole body. */
  field: string;
  /** The rule that failed: `email`, `length`, `range`, or a custom code. */
  code: string;
  /** Human-readable message. */
  message: string;
  /** The rule's parameters, e.g. `{ min: 8 }`. */
  params: Record<string, unknown>;
}

/** A non-2xx response. `code` and `details` come from zapline's error body. */
export class ApiError extends Error {
  /** The HTTP status. */
  readonly status: number;
  /** zapline's stable error code, e.g. `"not_found"` or `"validation_error"`. */
  readonly code: string | undefined;
  /** Structured details from the error body. */
  readonly details: unknown;
  /** The whole decoded body. */
  readonly body: unknown;
  /** The client method that failed, e.g. `"createTask"`. */
  readonly operation: string;
  constructor(status: number, body: unknown, operation = "") {
    const e = (typeof body === "object" && body ? body : {}) as { error?: unknown; code?: unknown; details?: unknown };
    super(e.error !== undefined ? String(e.error) : `HTTP ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
    this.code = typeof e.code === "string" ? e.code : undefined;
    this.details = e.details;
    this.operation = operation;
  }
}

type FieldKey<F extends string> = F | (string & {});

/**
 * A `422`: the body failed validation (or did not match the expected type).
 * `F` is the body's field names, for autocomplete.
 */
export class ValidationError<F extends string = string> extends ApiError {
  /** Every failed rule, sorted by field. */
  readonly errors: FieldError[];
  /** Failed rules by field path. */
  readonly fields: Partial<Record<FieldKey<F>, FieldError[]>>;
  constructor(status: number, body: unknown, operation: string, errors: FieldError[]) {
    super(status, body, operation);
    this.name = "ValidationError";
    this.errors = errors;
    const fields: Record<string, FieldError[]> = {};
    for (const err of errors) (fields[err.field] ??= []).push(err);
    this.fields = fields as Partial<Record<FieldKey<F>, FieldError[]>>;
  }
  /** The first message for `field`, to show next to its input. */
  messageFor(field: FieldKey<F>): string | undefined {
    return this.fields[field]?.[0]?.message;
  }
}

/** Whether `e` is an error response from this API. */
export function isApiError(e: unknown): e is ApiError {
  return e instanceof ApiError;
}

/**
 * Whether `e` is a validation error, optionally of one method, whose field
 * names it then knows: `if (isValidationError(e, "createTask")) e.fields.title`.
 */
export function isValidationError<K extends keyof ValidationFields = never>(
  e: unknown,
  operation?: K,
): e is ValidationError<[K] extends [never] ? string : ValidationFields[K]> {
  return e instanceof ValidationError && (operation === undefined || e.operation === operation);
}

function fieldErrors(status: number, body: unknown): FieldError[] | undefined {
  if (status !== 422 || typeof body !== "object" || !body) return undefined;
  const b = body as { error?: unknown; code?: unknown; details?: unknown };
  if (b.code === "validation_error" && Array.isArray(b.details)) return b.details as FieldError[];
  const path = (b.details as { path?: unknown } | undefined)?.path;
  if (b.code === "invalid_body" && typeof path === "string") {
    return [{ field: path, code: "invalid_body", message: String(b.error ?? "invalid value"), params: {} }];
  }
  return undefined;
}

function csrfToken(names: string[]): string | undefined {
  const doc = (globalThis as { document?: { cookie?: unknown } }).document;
  if (!doc || typeof doc.cookie !== "string") return undefined;
  for (const part of doc.cookie.split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && names.includes(part.slice(0, i).trim())) return decodeURIComponent(part.slice(i + 1).trim());
  }
  return undefined;
}

interface Init {
  query?: object | undefined;
  json?: unknown;
  form?: object | undefined;
  multipart?: FormData | Record<string, FormValue | FormValue[] | null | undefined> | undefined;
  raw?: Blob | BufferSource | string | undefined;
  type?: string;
  as?: "text" | "blob" | "response";
}

function makeRequest(options: ClientOptions) {
  const base = (options.baseUrl ?? "").replace(/\/$/, "");
  const csrf = options.csrf === false ? undefined : options.csrf ?? {};
  const csrfCookies = csrf?.cookie ? [csrf.cookie] : ["__Host-zapline.csrf", "zapline.csrf"];
  return async function request<T>(
    operation: string,
    method: string,
    path: string,
    init: Init = {},
    call: CallOptions | undefined = {},
  ): Promise<T> {
    let url = base + path;
    if (init.query) {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(init.query)) {
        if (v === undefined || v === null) continue;
        for (const item of Array.isArray(v) ? v : [v]) qs.append(k, String(item));
      }
      const s = qs.toString();
      if (s) url += "?" + s;
    }
    const extra = typeof options.headers === "function" ? await options.headers() : options.headers;
    const headers = new Headers(extra);
    new Headers(call.headers).forEach((v, k) => headers.set(k, v));
    let body: BodyInit | null = null;
    if (init.json !== undefined) {
      headers.set("content-type", "application/json");
      body = JSON.stringify(init.json);
    } else if (init.form !== undefined) {
      const form = new URLSearchParams();
      for (const [k, v] of Object.entries(init.form)) {
        if (v === undefined || v === null) continue;
        for (const item of Array.isArray(v) ? v : [v]) form.append(k, String(item));
      }
      body = form;
    } else if (init.multipart !== undefined) {
      if (init.multipart instanceof FormData) {
        body = init.multipart;
      } else {
        const form = new FormData();
        for (const [k, v] of Object.entries(init.multipart)) {
          if (v === undefined || v === null) continue;
          for (const item of Array.isArray(v) ? v : [v]) form.append(k, item instanceof Blob ? item : String(item));
        }
        body = form;
      }
    } else if (init.raw !== undefined) {
      if (init.type && !headers.has("content-type")) headers.set("content-type", init.type);
      body = init.raw as BodyInit;
    }
    if (!headers.has("accept")) headers.set("accept", init.as ? "*/*" : "application/json");
    if (csrf && !["GET", "HEAD", "OPTIONS", "TRACE"].includes(method)) {
      const header = csrf.header ?? "x-csrf-token";
      const token = headers.has(header) ? undefined : csrfToken(csrfCookies);
      if (token) headers.set(header, token);
    }
    const req: RequestInit = { method, headers, body };
    if (call.signal) req.signal = call.signal;
    if (options.credentials) req.credentials = options.credentials;
    const res = await (options.fetch ?? fetch)(url, req);
    if (res.ok && init.as === "response") return res as T;
    if (res.ok && init.as === "blob") return (await res.blob()) as T;
    const text = await res.text();
    // Error bodies are JSON even when the success body is text.
    const json = (res.headers.get("content-type") ?? "").includes("json") && (!res.ok || init.as !== "text");
    let data: unknown = text || undefined;
    if (text && json) {
      try {
        data = JSON.parse(text);
      } catch {
        // Not JSON after all: keep the text.
      }
    }
    if (!res.ok) {
      const errors = fieldErrors(res.status, data);
      throw errors ? new ValidationError(res.status, data, operation, errors) : new ApiError(res.status, data, operation);
    }
    return data as T;
  };
}
