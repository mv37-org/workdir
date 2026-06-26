/**
 * Minimal TypeScript SDK for workdir. Uses the global `fetch`
 * (Node 18+, Deno, Bun, browsers). Zero dependencies.
 *
 * The default path is one call:
 *
 *   const client = new Client("https://api.sandboxes.example.com", "sk_live_...");
 *   const sandbox = await client.sandboxes.create();
 *   console.log((await sandbox.exec("echo ok")).stdout);
 *   await sandbox.delete();
 *
 * Heavier sandboxes require explicit options (spec §3.4):
 *
 *   const sandbox = await client.sandboxes.create({
 *     resources: { cpu: 2, memoryMb: 4096, diskGb: 16 },
 *     image: "browser",
 *     browser: { enabled: true, vnc: true, cdp: true },
 *     startup: {
 *       git: { url: "https://github.com/acme/app.git", ref: "main", depth: 1 },
 *       commands: [{ name: "install", run: "pnpm install --frozen-lockfile" }],
 *       ports: [3000, 6080],
 *       ready: { http: "http://127.0.0.1:3000", timeout_seconds: 30 },
 *     },
 *   });
 *
 * Opt in to an in-sandbox coding agent (opencode), installed on demand:
 *
 *   const sandbox = await client.sandboxes.create({
 *     codingAgent: { enabled: true },
 *     startup: { secrets: ["ANTHROPIC_API_KEY"] },
 *   });
 *   await sandbox.exec("opencode run 'add a test for utils.py'");
 */

export interface ResourcesInput {
  cpu?: number;
  memoryMb?: number;
  diskGb?: number;
}

export interface MountInput {
  type: "s3";
  bucket: string;
  mount_path: string;
  prefix?: string;
  read_only?: boolean;
  region?: string;
  endpoint?: string;
}

export interface EphemeralFileInput {
  path: string;
  content: string;
  encoding?: "utf8" | "base64";
}

export interface VolumeAttachInput {
  volume_id: string;
  mount_path: string;
}

export interface CreateOptions {
  resources?: ResourcesInput;
  image?: string;
  browser?: { enabled: boolean; vnc?: boolean; cdp?: boolean };
  startup?: Record<string, unknown> | "none";
  auto_stop_seconds?: number;
  snapshot?: boolean;
  image_version?: string;
  /** Run dockerd inside the guest microVM (needs a docker-capable image). */
  docker?: { enabled: boolean };
  /**
   * Install a lightweight coding-agent CLI (opencode) into the guest. Opt-in —
   * not present unless requested. Pass a provider key via `startup.secrets`
   * (e.g. ANTHROPIC_API_KEY) to make it usable.
   */
  codingAgent?: { enabled: boolean; kind?: "opencode"; version?: string };
  /** S3 bucket mounts; credentials come from injected secret env. */
  mounts?: MountInput[];
  /** Inline ephemeral files written into the workspace at boot. */
  files?: EphemeralFileInput[];
  /** Persistent block volumes to attach inside the guest. */
  volumes?: VolumeAttachInput[];
}

export interface ExecResult {
  exit_code: number;
  stdout: string;
  stderr: string;
}

export class SandboxError extends Error {
  constructor(public status: number, public code: string, message: string) {
    super(`[${status} ${code}] ${message}`);
  }
}

class Http {
  constructor(private base: string, private key: string) {
    this.base = base.replace(/\/$/, "");
  }

  async request<T = any>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(`${this.base}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.key}`,
        "Content-Type": "application/json",
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const text = await res.text();
    const data = text ? JSON.parse(text) : {};
    if (!res.ok) {
      const err = (data as any).error ?? {};
      throw new SandboxError(res.status, err.code ?? "error", err.message ?? text);
    }
    return data as T;
  }
}

// Translate SDK camelCase resources to the API's snake_case wire form.
function toWireResources(r?: ResourcesInput): Record<string, unknown> | undefined {
  if (!r) return undefined;
  const out: Record<string, unknown> = {};
  if (r.cpu !== undefined) out.cpu = r.cpu;
  if (r.memoryMb !== undefined) out.memory_mb = r.memoryMb;
  if (r.diskGb !== undefined) out.disk_gb = r.diskGb;
  return out;
}

export class Sandbox {
  constructor(private http: Http, private data: any) {}

  get id(): string { return this.data.id; }
  get state(): string { return this.data.state; }
  get bootPath(): string { return this.data.boot_path; }
  get timings(): Record<string, number> { return this.data.timings ?? {}; }
  get urls(): Record<string, any> { return this.data.urls ?? {}; }
  get price(): Record<string, number> { return this.data.price ?? {}; }
  get network(): Record<string, any> { return this.data.network ?? {}; }

  async refresh(): Promise<Sandbox> {
    this.data = await this.http.request("GET", `/v1/sandboxes/${this.id}`);
    return this;
  }

  async exec(cmd: string, opts: { cwd?: string; env?: Record<string, string>; background?: boolean } = {}): Promise<ExecResult> {
    return this.http.request("POST", `/v1/sandboxes/${this.id}/exec`, {
      cmd,
      cwd: opts.cwd,
      env: opts.env,
      background: opts.background ?? false,
    });
  }

  async writeFile(path: string, content: string): Promise<void> {
    await this.http.request("PUT", `/v1/sandboxes/${this.id}/files`, { path, content });
  }

  async readFile(path: string): Promise<string> {
    const r = await this.http.request<{ content: string }>(
      "GET", `/v1/sandboxes/${this.id}/files?path=${encodeURIComponent(path)}`);
    return r.content;
  }

  async exposePort(port: number): Promise<string> {
    const r = await this.http.request<{ url: string }>(
      "POST", `/v1/sandboxes/${this.id}/ports/${port}/expose`);
    return r.url;
  }

  async browser(): Promise<any> {
    return this.http.request("GET", `/v1/sandboxes/${this.id}/browser`);
  }

  async metrics(): Promise<any> {
    return this.http.request("GET", `/v1/sandboxes/${this.id}/metrics`);
  }

  async snapshot(): Promise<any> {
    return this.http.request("POST", `/v1/sandboxes/${this.id}/snapshot`);
  }

  async fork(): Promise<Sandbox> {
    return new Sandbox(this.http, await this.http.request("POST", `/v1/sandboxes/${this.id}/fork`));
  }

  async pause(): Promise<Sandbox> {
    this.data = await this.http.request("POST", `/v1/sandboxes/${this.id}/pause`);
    return this;
  }

  async resume(): Promise<Sandbox> {
    this.data = await this.http.request("POST", `/v1/sandboxes/${this.id}/resume`);
    return this;
  }

  async delete(): Promise<void> {
    await this.http.request("DELETE", `/v1/sandboxes/${this.id}`);
  }
}

class Sandboxes {
  constructor(private http: Http) {}

  async create(options: CreateOptions = {}): Promise<Sandbox> {
    const { codingAgent, ...rest } = options;
    const body: Record<string, unknown> = { ...rest };
    if (options.resources) body.resources = toWireResources(options.resources);
    if (codingAgent) body.coding_agent = codingAgent;
    const data = await this.http.request("POST", "/v1/sandboxes", body);
    return new Sandbox(this.http, data);
  }

  async get(id: string): Promise<Sandbox> {
    return new Sandbox(this.http, await this.http.request("GET", `/v1/sandboxes/${id}`));
  }

  async list(): Promise<Sandbox[]> {
    const data = await this.http.request<{ sandboxes: any[] }>("GET", "/v1/sandboxes");
    return data.sandboxes.map((s) => new Sandbox(this.http, s));
  }
}

class Images {
  constructor(private http: Http) {}
  create(
    name: string,
    source: Record<string, unknown>,
    resourcesHint?: Record<string, unknown>,
    opts: { ephemeral?: boolean; ttl_seconds?: number } = {},
  ) {
    return this.http.request("POST", "/v1/images", {
      name,
      source,
      resources_hint: resourcesHint,
      ...opts,
    });
  }
  get(id: string) { return this.http.request("GET", `/v1/images/${id}`); }
  list() { return this.http.request("GET", "/v1/images"); }
  delete(id: string) { return this.http.request("DELETE", `/v1/images/${id}`); }
}

class Volumes {
  constructor(private http: Http) {}
  create(name: string, sizeGb: number) {
    return this.http.request("POST", "/v1/volumes", { name, size_gb: sizeGb });
  }
  get(id: string) { return this.http.request("GET", `/v1/volumes/${id}`); }
  list() { return this.http.request("GET", "/v1/volumes"); }
  delete(id: string) { return this.http.request("DELETE", `/v1/volumes/${id}`); }
}

class Nodes {
  constructor(private http: Http) {}
  list() { return this.http.request("GET", "/v1/nodes"); }
  joinToken() { return this.http.request("POST", "/v1/nodes/join-token"); }
  drain(id: string) { return this.http.request("POST", `/v1/nodes/${id}/drain`); }
}

/** Org-scoped secrets. Values are encrypted at rest and never returned. */
class Secrets {
  constructor(private http: Http) {}
  set(name: string, value: string) { return this.http.request("PUT", `/v1/secrets/${name}`, { value }); }
  list() { return this.http.request("GET", "/v1/secrets"); }
  delete(name: string) { return this.http.request("DELETE", `/v1/secrets/${name}`); }
}

export class Client {
  readonly sandboxes: Sandboxes;
  readonly images: Images;
  readonly volumes: Volumes;
  readonly nodes: Nodes;
  readonly secrets: Secrets;
  private http: Http;

  constructor(baseUrl: string, apiKey: string) {
    this.http = new Http(baseUrl, apiKey);
    this.sandboxes = new Sandboxes(this.http);
    this.images = new Images(this.http);
    this.volumes = new Volumes(this.http);
    this.nodes = new Nodes(this.http);
    this.secrets = new Secrets(this.http);
  }

  usage() { return this.http.request("GET", "/v1/usage"); }
}
