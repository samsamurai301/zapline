// @ts-check
// Type-checked (`tsc --checkJs`) against the generated api.js by
// tests/client.rs: what an editor shows in a no-build page.
import { api, createClient, isValidationError } from "./api.js";

/** @typedef {import("./api.js").Task} Task */

export async function demo() {
  const page = await api.listTasks({ status: "doing" });
  /** @type {Task | undefined} */
  const first = page.items[0];
  console.log(first?.title.toUpperCase());
  // @ts-expect-error: not a Status
  await api.listTasks({ status: "nope" });

  try {
    await api.createTask({ title: "" });
  } catch (e) {
    if (isValidationError(e, "createTask")) {
      /** @type {string | undefined} */
      const message = e.messageFor("title");
      console.log(message, e.fields.priority);
    }
  }
  // @ts-expect-error: title must be a string
  await api.createTask({ title: 1 });

  /** @type {number} */
  const area = await api.area({ kind: "circle", radius: 1 });
  // @ts-expect-error: a rect needs a height
  await api.area({ kind: "rect", width: 1 });
  console.log(area);

  await api.upload(1, { file: new Blob(["x"]) });
  /** @type {Blob} */
  const blob = await api.export();
  console.log(blob);
  // @ts-expect-error: the id is required
  await api.getTask();

  const custom = createClient({ baseUrl: "https://api.example.com", csrf: false });
  await custom.deleteTask(1, { signal: AbortSignal.timeout(1000) });
}
