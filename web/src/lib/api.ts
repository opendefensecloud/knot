import { readCookie } from "./csrf";

export type ApiError = {
  code: string;
  message: string;
  details: Record<string, unknown>;
  status: number;
};

export type ApiResult<T> = { ok: T } | { error: ApiError };

/** An `ApiError` thrown, for code that fails a mutation by throwing. The
 *  message is the error's code, as a plain `new Error(code)` had it. */
export class ApiRequestError extends Error {
  readonly apiError: ApiError;

  constructor(apiError: ApiError) {
    super(apiError.code);
    this.name = "ApiRequestError";
    this.apiError = apiError;
  }
}

/** The error a failed response's parsed body describes: the server's
 *  `{"error":{code,message,details}}` envelope, else a bare `http_error`. */
export function apiErrorFrom(status: number, body: unknown): ApiError {
  const env = body as { error?: Partial<ApiError> } | undefined;
  const envelope = typeof env === "object" && env !== null ? env.error : undefined;
  return {
    code: envelope?.code ?? "http_error",
    message: envelope?.message ?? `HTTP ${status}`,
    details: envelope?.details ?? {},
    status,
  };
}

/** Reads a failed response's body into an `ApiError`, for the paths that
 *  fetch by hand (downloads, uploads). */
export async function readApiError(res: Response): Promise<ApiError> {
  const text = await res.text().catch(() => "");
  let body: unknown;
  try { body = JSON.parse(text); } catch { body = undefined; }
  return apiErrorFrom(res.status, body);
}

const NETWORK_ERROR: ApiError = { code: "network", message: "Network error", details: {}, status: 0 };

/** GETs a file the browser should save: its body as a blob, or the error,
 *  so a refused download (a 429, say) can be shown instead of a failed
 *  browser download. */
export async function fetchBlob(path: string): Promise<ApiResult<Blob>> {
  let res: Response;
  try {
    res = await fetch(path, { credentials: "include" });
  } catch {
    return { error: { ...NETWORK_ERROR } };
  }
  if (!res.ok) return { error: await readApiError(res) };
  try {
    return { ok: await res.blob() };
  } catch {
    return { error: { ...NETWORK_ERROR } };
  }
}

type Body = Record<string, unknown> | string | null;

type Opts = {
  method?: "GET" | "POST" | "PATCH" | "PUT" | "DELETE";
  body?: Body;
  contentType?: string;
  parser?: (data: unknown) => unknown;
};

const UNSAFE = new Set(["POST", "PUT", "PATCH", "DELETE"]);

export async function apiFetch<T>(path: string, opts: Opts = {}): Promise<ApiResult<T>> {
  const method = opts.method ?? "GET";
  const headers: Record<string, string> = { Accept: "application/json" };
  let body: BodyInit | undefined = undefined;
  if (opts.body !== undefined && opts.body !== null) {
    if (typeof opts.body === "string") {
      body = opts.body;
      headers["Content-Type"] = opts.contentType ?? "text/plain";
    } else {
      body = JSON.stringify(opts.body);
      headers["Content-Type"] = "application/json";
    }
  }
  if (UNSAFE.has(method)) {
    const csrf = readCookie("csrf");
    if (csrf) headers["X-CSRF-Token"] = csrf;
  }
  const res = await fetch(path, { method, credentials: "include", headers, body });
  const text = await res.text();
  let data: unknown = undefined;
  if (text.length > 0) {
    if (res.headers.get("content-type")?.includes("application/json")) {
      try { data = JSON.parse(text); } catch { data = text; }
    } else {
      data = text;
    }
  }
  if (!res.ok) return { error: apiErrorFrom(res.status, data) };
  const parsed = opts.parser ? opts.parser(data) : data;
  return { ok: parsed as T };
}
