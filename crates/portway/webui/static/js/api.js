// The one module that knows the server's routes. Every non-2xx answer
// carries `{"error": ...}`, which becomes the ApiError's message.

export const CSRF = "x-portway-console";

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function call(method, path, body) {
  const headers = {};
  if (method !== "GET") headers[CSRF] = "1";
  if (body !== undefined) headers["content-type"] = "application/json";
  let response;
  try {
    response = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      credentials: "same-origin",
      cache: "no-store",
    });
  } catch (err) {
    throw new ApiError(0, "the console is not answering");
  }
  if (response.status === 204) return null;
  let data = null;
  try {
    data = await response.json();
  } catch {
    data = null;
  }
  if (!response.ok) {
    throw new ApiError(response.status, data?.error ?? `${response.status} ${response.statusText}`);
  }
  return data;
}

const query = (params) => {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== "") search.set(key, value);
  }
  const text = search.toString();
  return text ? `?${text}` : "";
};

export const api = {
  health: () => call("GET", "/api/health"),
  session: (token) => call("POST", "/api/session", { token }),
  snapshot: () => call("GET", "/api/snapshot"),
  events: (before, limit = 1000) => call("GET", `/api/events${query({ before, limit })}`),
  usage: (range) => call("GET", `/api/usage${query({ range })}`),
  report: (since, model, text = false) =>
    call("GET", `/api/report${query({ since, model, text: text ? "1" : "" })}`),
  reload: () => call("POST", "/api/reload"),
  stop: () => call("POST", "/api/stop"),
  streamUrl: (after) => `/api/stream${query({ after })}`,
};

/**
 * Take `#token=` off the address before anything else runs, so it is never
 * in history, a bookmark or a screenshot, and trade it for the cookie.
 * Returns false when there is neither a token nor a session.
 */
export async function signIn() {
  const hash = window.location.hash;
  const match = /(?:^#|&)token=([0-9a-f]{16,128})/.exec(hash);
  if (match) {
    const rest = hash.replace(/(?:^#|&)token=[0-9a-f]{16,128}/, "").replace(/^&/, "");
    history.replaceState(null, "", `${location.pathname}${location.search}${rest ? `#${rest}` : ""}`);
    try {
      await api.session(match[1]);
      return true;
    } catch (err) {
      if (err.status !== 401) throw err;
    }
  }
  const health = await api.health();
  return Boolean(health?.session);
}
