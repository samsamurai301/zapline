// Type-checked against the generated api.ts by tests/client.rs. Every
// `@ts-expect-error` must be a real error, so the types are proven to bite.
import {
  api,
  createClient,
  isApiError,
  isValidationError,
  type Client,
  type FieldError,
  type Shape,
  type Status,
  type Task,
  type TaskPatch,
} from "./api";

export async function demo(): Promise<void> {
  const page = await api.listTasks({ status: "doing", limit: 10 });
  const first: Task | undefined = page.items[0];
  const status: Status | undefined = first?.status;
  const next: string | null = page.next;
  console.log(status, next);
  // @ts-expect-error: not a Status
  await api.listTasks({ status: "nope" });

  const created: Task = await api.createTask({ title: "x", priority: 1 });
  console.log(created.meta["label"]?.toUpperCase());
  // @ts-expect-error: title must be a string
  await api.createTask({ title: 1 });
  // @ts-expect-error: title is required
  await api.createTask({ priority: 1 });

  try {
    await api.createTask({ title: "" });
  } catch (e) {
    if (isValidationError(e, "createTask")) {
      const errors: FieldError[] | undefined = e.fields.title;
      const message: string | undefined = e.messageFor("priority");
      console.log(errors, message, e.status, e.operation);
    } else if (isValidationError(e)) {
      console.log(e.fields["anything.at.all"]);
    } else if (isApiError(e)) {
      const code: string | undefined = e.code;
      console.log(code);
    }
    // @ts-expect-error: not a method name
    isValidationError(e, "noSuchMethod");
  }

  const patch: TaskPatch = { assignee: null, status: "done" };
  await api.updateTask(1, patch);
  await api.deleteTask(1);
  const byNumber: Task = await api.projectTask("web", 7);
  console.log(byNumber);
  // @ts-expect-error: the id is required
  await api.getTask();

  const shape: Shape = { kind: "rect", width: 1, height: 2 };
  const area: number = await api.area(shape);
  console.log(area);
  // @ts-expect-error: a circle has a radius, not a width
  await api.area({ kind: "circle", width: 1 });

  await api.upload(1, { file: new Blob(["x"]), note: "n" });
  await api.upload(1, new FormData());
  await api.login({ email: "a@b.c", password: "pw" });
  const text: string = await api.docFile("a/b.md");
  const imported: number = await api.import("a\nb");
  const blob: Blob = await api.export();
  const stream: Response = await api.events();
  console.log(text, imported, blob, stream);

  const custom: Client = createClient({
    baseUrl: "https://api.example.com",
    headers: async () => ({ authorization: "Bearer t" }),
    credentials: "include",
    csrf: { cookie: "my.csrf" },
  });
  await custom.getTask(1, { signal: AbortSignal.timeout(1000), headers: { "x-trace": "1" } });
  createClient({ csrf: false });
}
