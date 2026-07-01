# @mv37/workdir

TypeScript SDK for [workdir](https://workdir.dev).

```bash
npm install @mv37/workdir
```

```ts
import { Client } from "@mv37/workdir";

const workdir = new Client("https://api.workdir.dev", process.env.WORKDIR_API_KEY!);

const box = await workdir.sandboxes.create();
const { stdout } = await box.exec("echo hello");
console.log(stdout);

const job = await box.exec("npm test", { background: true });
let status = await box.execStatus(job.cmd_id);
while (status.state === "running") {
  await new Promise((resolve) => setTimeout(resolve, 1000));
  status = await box.execStatus(job.cmd_id);
}
console.log(await box.execLogs(job.cmd_id));
await box.delete();
```

The SDK uses the global `fetch` API and supports Node.js 18+, Deno, Bun, and browsers.
