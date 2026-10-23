
/**
 * Options for {@link createClient}.
 * @typedef {object} ClientOptions
 * @property {string} [baseUrl] Prefix for every path, e.g. "https://api.example.com". Default: same origin.
 * @property {HeadersInit | (() => HeadersInit | Promise<HeadersInit>)} [headers] Extra headers (for example `Authorization`), or a function returning them.
 * @property {typeof fetch} [fetch] A `fetch` implementation. Default: the global one.
 * @property {RequestCredentials} [credentials] Whether to send cookies. Default: `"same-origin"`.
 * @property {false | { cookie?: string, header?: string }} [csrf] In browsers, unsafe requests (POST, PUT, PATCH, DELETE) copy zapline's CSRF cookie into the `x-csrf-token` header, so `Csrf` middleware lets them through. Set the names if you changed them, or `false` to turn it off.
 */

/**
 * Per-call options: the last argument of every method.
 * @typedef {object} CallOptions
 * @property {AbortSignal} [signal] Aborts the request, e.g. `AbortSignal.timeout(5000)`.
 * @property {HeadersInit} [headers] Headers for this call only.
 */

/**
 * A query string or form value.
 * @typedef {string | number | boolean | null | undefined | Array<string | number | boolean>} QueryValue
 */

/**
 * A multipart form value. `Blob`s (and `File`s) are sent as files.
 * @typedef {string | number | boolean | Blob} FormValue
 */

/**
 * One failed validation rule, as listed in a `422` response.
 * @typedef {object} FieldError
 * @property {string} field Path to the field: `email`, `address.zip`, `items[0].name`. Empty for the whole body.
 * @property {string} code The rule that failed: `email`, `length`, `range`, or a custom code.
 * @property {string} message Human-readable message.
 * @property {Record<string, unknown>} params The rule's parameters, e.g. `{ min: 8 }`.
 */

/** A non-2xx response. `code` and `details` come from zapline's error body. */
export class ApiError extends Error {
  /**
   * @param {number} status
   * @param {unknown} body
   * @param {string} [operation]
   */
  constructor(status, body, operation = "") {
    const e = /** @type {{ error?: unknown, code?: unknown, details?: unknown }} */ (
      typeof body === "object" && body ? body : {}
    );
    super(e.error !== undefined ? String(e.error) : `HTTP ${status}`);
    this.name = "ApiError";
    /** The HTTP status. @readonly @type {number} */
    this.status = status;
    /** The whole decoded body. @readonly @type {unknown} */
    this.body = body;
    /** zapline's stable error code, e.g. `"not_found"` or `"validation_error"`. @readonly @type {string | undefined} */
    this.code = typeof e.code === "string" ? e.code : undefined;
    /** Structured details from the error body. @readonly @type {unknown} */
    this.details = e.details;
    /** The client method that failed, e.g. `"createTask"`. @readonly @type {string} */
    this.operation = operation;
  }
}

/**
 * A `422`: the body failed validation (or did not match the expected type).
 * `F` is the body's field names, for autocomplete.
 * @template {string} [F=string]
 */
export class ValidationError extends ApiError {
  /**
   * @param {number} status
   * @param {unknown} body
   * @param {string} operation
   * @param {FieldError[]} errors
   */
  constructor(status, body, operation, errors) {
    super(status, body, operation);
    this.name = "ValidationError";
    /** Every failed rule, sorted by field. @readonly @type {FieldError[]} */
    this.errors = errors;
    /** @type {Record<string, FieldError[]>} */
    const fields = {};
    for (const err of errors) (fields[err.field] ??= []).push(err);
    /** Failed rules by field path. @readonly @type {Partial<Record<F | (string & {}), FieldError[]>>} */
    this.fields = /** @type {Partial<Record<F | (string & {}), FieldError[]>>} */ (fields);
  }

  /**
   * The first message for `field`, to show next to its input.
   * @param {F | (string & {})} field
   * @returns {string | undefined}
   */
  messageFor(field) {
    return this.fields[field]?.[0]?.message;
  }
}

/**
 * Whether `e` is an error response from this API.
 * @param {unknown} e
 * @returns {e is ApiError}
 */
export function isApiError(e) {
  return e instanceof ApiError;
}

/**
 * Whether `e` is a validation error, optionally of one method, whose field
 * names it then knows: `if (isValidationError(e, "createTask")) e.fields.title`.
 * @template {keyof ValidationFields} [K=never]
 * @param {unknown} e
 * @param {K} [operation]
 * @returns {e is ValidationError<[K] extends [never] ? string : ValidationFields[K]>}
 */
export function isValidationError(e, operation) {
  return e instanceof ValidationError && (operation === undefined || e.operation === operation);
}

/**
 * @param {number} status
 * @param {unknown} body
 * @returns {FieldError[] | undefined}
 */
function fieldErrors(status, body) {
  if (status !== 422 || typeof body !== "object" || !body) return undefined;
  const b = /** @type {{ error?: unknown, code?: unknown, details?: unknown }} */ (body);
  if (b.code === "validation_error" && Array.isArray(b.details)) return b.details;
  const path = /** @type {{ path?: unknown } | undefined} */ (b.details)?.path;
  if (b.code === "invalid_body" && typeof path === "string") {
    return [{ field: path, code: "invalid_body", message: String(b.error ?? "invalid value"), params: {} }];
  }
  return undefined;
}

/**
 * @param {string[]} names
 * @returns {string | undefined}
 */
function csrfToken(names) {
  const doc = /** @type {{ document?: { cookie?: unknown } }} */ (/** @type {unknown} */ (globalThis)).document;
  if (!doc || typeof doc.cookie !== "string") return undefined;
  for (const part of doc.cookie.split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && names.includes(part.slice(0, i).trim())) return decodeURIComponent(part.slice(i + 1).trim());
  }
  return undefined;
}

/**
 * @typedef {object} Init
 * @property {object | undefined} [query]
 * @property {unknown} [json]
 * @property {object | undefined} [form]
 * @property {FormData | Record<string, FormValue | FormValue[] | null | undefined> | undefined} [multipart]
 * @property {Blob | BufferSource | string | undefined} [raw]
 * @property {string} [type]
 * @property {"text" | "blob" | "response"} [as]
 */

/** @param {ClientOptions} options */
function makeRequest(options) {
  const base = (options.baseUrl ?? "").replace(/\/$/, "");
  const csrf = options.csrf === false ? undefined : options.csrf ?? {};
  const csrfCookies = csrf?.cookie ? [csrf.cookie] : ["__Host-zapline.csrf", "zapline.csrf"];
  /**
   * @param {string} operation
   * @param {string} method
   * @param {string} path
   * @param {Init} [init]
   * @param {CallOptions | undefined} [call]
   * @returns {Promise<any>}
   */
  return async function request(operation, method, path, init = {}, call = {}) {
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
    /** @type {BodyInit | null} */
    let body = null;
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
      body = /** @type {BodyInit} */ (init.raw);
    }
    if (!headers.has("accept")) headers.set("accept", init.as ? "*/*" : "application/json");
    if (csrf && !["GET", "HEAD", "OPTIONS", "TRACE"].includes(method)) {
      const header = csrf.header ?? "x-csrf-token";
      const token = headers.has(header) ? undefined : csrfToken(csrfCookies);
      if (token) headers.set(header, token);
    }
    /** @type {RequestInit} */
    const req = { method, headers, body };
    if (call.signal) req.signal = call.signal;
    if (options.credentials) req.credentials = options.credentials;
    const res = await (options.fetch ?? fetch)(url, req);
    if (res.ok && init.as === "response") return res;
    if (res.ok && init.as === "blob") return await res.blob();
    const text = await res.text();
    // Error bodies are JSON even when the success body is text.
    const json = (res.headers.get("content-type") ?? "").includes("json") && (!res.ok || init.as !== "text");
    /** @type {unknown} */
    let data = text || undefined;
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
    return data;
  };
}
