// Drives the generated JavaScript client against the representative app
// (tests/client.rs starts it and passes its base URL). Prints one JSON line.
import { ApiError, ValidationError, createClient, isValidationError } from "./api.js";

const baseUrl = process.argv[2];
const api = createClient({ baseUrl });
const out = {};

out.list = (await api.listTasks({ status: "todo", limit: 2 })).items.length;
out.created = (await api.createTask({ title: "write docs", priority: 2 })).title;

try {
  await api.createTask({ title: "", priority: 9 });
} catch (e) {
  out.validation = isValidationError(e, "createTask")
    ? { fields: Object.keys(e.fields).sort(), title: e.messageFor("title"), operation: e.operation }
    : String(e);
}

try {
  await api.updateTask(1, { title: 5 });
} catch (e) {
  out.typeMismatch = isValidationError(e) ? { fields: Object.keys(e.fields), status: e.status } : String(e);
}

try {
  await api.getTask(500);
} catch (e) {
  out.notFound = e instanceof ApiError ? [e.status, e.code ?? null, e.operation] : String(e);
}

out.project = (await api.projectTask("web app", 7)).title;
out.upload = await api.upload(3, { a: new File(["hello"], "a.txt"), note: "x" });
out.login = (await api.login({ email: "ada@example.com", password: "secret" })).name;
try {
  await api.login({ email: "ada@example.com", password: "nope" });
} catch (e) {
  out.badLogin = [e.status, e.code, e instanceof ValidationError];
}
out.area = await api.area({ kind: "rect", width: 2, height: 3 });
out.file = await api.docFile("guide/intro.md");
out.imported = await api.import("a\n\nb\n");
out.exported = (await api.export()).size;
out.events = (await (await api.events()).text()).includes("hello");
out.deleted = (await api.deleteTask(1)) === undefined;

// In a browser, unsafe requests echo the CSRF cookie.
globalThis.document = { cookie: "other=1; zapline.csrf=tok123" };
const seen = [];
const spy = createClient({
  baseUrl,
  fetch: (url, init) => {
    seen.push(new Headers(init.headers).get("x-csrf-token"));
    return fetch(url, init);
  },
});
await spy.deleteTask(1);
await spy.getTask(1);
delete globalThis.document;
out.csrf = { post: seen[0], get: seen[1] };

try {
  await api.listTasks({}, { signal: AbortSignal.abort() });
} catch (e) {
  out.aborted = e.name;
}

console.log(JSON.stringify(out));
