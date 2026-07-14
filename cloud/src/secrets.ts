import { createCipheriv, createDecipheriv, createHash, randomBytes } from "node:crypto";
import type { Database } from "better-sqlite3";

// Per-account secrets vault: envelope encryption with a control-plane KEK
// (RUDDER_SECRETS_KEY fly secret) wrapping one data key per account, values
// encrypted under the account key with AES-256-GCM. The SQLite rows only ever
// hold ciphertext, so the S3-persisted copy of the database is defense in
// depth rather than the security boundary.

export type SecretKind = "env" | "file";

export type SecretItemInput = {
  name: string;
  kind: SecretKind;
  filePath?: string;
  value: Buffer;
  source?: string;
};

export type SecretMetadata = {
  name: string;
  kind: SecretKind;
  filePath?: string;
  sizeBytes: number;
  sha256: string;
  source?: string;
  createdAt: string;
  updatedAt: string;
};

export type WorkerSecrets = {
  version: number;
  env: Record<string, string>;
  files: Array<{ path: string; contentBase64: string; mode: number }>;
};

export const MAX_SECRET_BYTES = 1024 * 1024;
export const MAX_ACCOUNT_SECRETS = 500;
export const MAX_ACCOUNT_TOTAL_BYTES = 15 * 1024 * 1024;

// Never store private key material that grants access beyond what a cloud
// workspace needs, even if a client asks. Mirrors the client-side snapshot
// guardrails (SECRET_PATH_PARTS in src/cloud.ts) but enforced server-side.
const BLOCKED_FILE_PARTS = new Set([".ssh", ".gnupg", "keychains"]);

const ENV_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

export class SecretsVaultError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

export type SecretsVault = ReturnType<typeof createSecretsVault>;

export function createSecretsVault(database: Database, kekBase64: string) {
  const kek = Buffer.from(kekBase64, "base64");
  if (kek.length !== 32) {
    throw new Error("RUDDER_SECRETS_KEY must be 32 bytes of base64");
  }

  database.exec(`
    create table if not exists rudder_account_keys (
      account_id text primary key,
      wrapped_dek text not null,
      kek_version integer not null default 1,
      secrets_version integer not null default 0,
      created_at text not null,
      updated_at text not null
    );
    create table if not exists rudder_secrets (
      account_id text not null,
      name text not null,
      kind text not null,
      file_path text,
      ciphertext text not null,
      size_bytes integer not null,
      sha256 text not null,
      source text,
      created_at text not null,
      updated_at text not null,
      primary key (account_id, name)
    );
  `);

  const findAccountKey = database.prepare("select * from rudder_account_keys where account_id = ?");
  const insertAccountKey = database.prepare(`
    insert into rudder_account_keys (account_id, wrapped_dek, kek_version, secrets_version, created_at, updated_at)
    values (@accountId, @wrappedDek, 1, 0, @now, @now)
  `);
  const bumpVersion = database.prepare(
    "update rudder_account_keys set secrets_version = secrets_version + 1, updated_at = ? where account_id = ?",
  );
  const listRows = database.prepare("select * from rudder_secrets where account_id = ? order by name");
  const findRow = database.prepare("select * from rudder_secrets where account_id = ? and name = ?");
  const accountUsage = database.prepare(
    "select count(*) as count, coalesce(sum(size_bytes), 0) as bytes from rudder_secrets where account_id = ?",
  );
  const upsertRow = database.prepare(`
    insert into rudder_secrets (account_id, name, kind, file_path, ciphertext, size_bytes, sha256, source, created_at, updated_at)
    values (@accountId, @name, @kind, @filePath, @ciphertext, @sizeBytes, @sha256, @source, @now, @now)
    on conflict(account_id, name) do update set
      kind = @kind,
      file_path = @filePath,
      ciphertext = @ciphertext,
      size_bytes = @sizeBytes,
      sha256 = @sha256,
      source = @source,
      updated_at = @now
  `);
  const deleteRow = database.prepare("delete from rudder_secrets where account_id = ? and name = ?");

  function seal(key: Buffer, plaintext: Buffer, aad: string): string {
    const iv = randomBytes(12);
    const cipher = createCipheriv("aes-256-gcm", key, iv);
    cipher.setAAD(Buffer.from(aad, "utf8"));
    const ciphertext = Buffer.concat([cipher.update(plaintext), cipher.final()]);
    return Buffer.concat([iv, cipher.getAuthTag(), ciphertext]).toString("base64");
  }

  function open(key: Buffer, sealed: string, aad: string): Buffer {
    const raw = Buffer.from(sealed, "base64");
    if (raw.length < 12 + 16) {
      throw new SecretsVaultError(500, "corrupt secret ciphertext");
    }
    const decipher = createDecipheriv("aes-256-gcm", key, raw.subarray(0, 12));
    decipher.setAAD(Buffer.from(aad, "utf8"));
    decipher.setAuthTag(raw.subarray(12, 28));
    return Buffer.concat([decipher.update(raw.subarray(28)), decipher.final()]);
  }

  function valueAad(accountId: string, name: string, kind: string, filePath: string | undefined): string {
    return `${accountId}\0${name}\0${kind}\0${filePath ?? ""}`;
  }

  function accountDek(accountId: string): Buffer {
    const row = findAccountKey.get(accountId) as Record<string, unknown> | undefined;
    if (row) {
      return open(kek, String(row.wrapped_dek), `account:${accountId}`);
    }
    const dek = randomBytes(32);
    insertAccountKey.run({
      accountId,
      wrappedDek: seal(kek, dek, `account:${accountId}`),
      now: new Date().toISOString(),
    });
    return dek;
  }

  function secretsVersion(accountId: string): number {
    const row = findAccountKey.get(accountId) as Record<string, unknown> | undefined;
    return row ? Number(row.secrets_version) : 0;
  }

  function normalizeFilePath(filePath: string): string {
    const trimmed = filePath.trim();
    if (!trimmed.startsWith("~/")) {
      throw new SecretsVaultError(400, `file secret path must start with ~/: ${trimmed}`);
    }
    const parts = trimmed.slice(2).split("/").filter(Boolean);
    if (parts.length === 0) {
      throw new SecretsVaultError(400, "file secret path is empty");
    }
    for (const part of parts) {
      if (part === "." || part === "..") {
        throw new SecretsVaultError(400, `file secret path may not contain relative segments: ${trimmed}`);
      }
      if (BLOCKED_FILE_PARTS.has(part.toLowerCase())) {
        throw new SecretsVaultError(400, `refusing to store secrets under ${part}`);
      }
    }
    return `~/${parts.join("/")}`;
  }

  function validate(item: SecretItemInput): SecretItemInput {
    if (item.value.length === 0) {
      throw new SecretsVaultError(400, `secret ${item.name} is empty`);
    }
    if (item.value.length > MAX_SECRET_BYTES) {
      throw new SecretsVaultError(413, `secret ${item.name} exceeds ${MAX_SECRET_BYTES} bytes`);
    }
    if (item.kind === "env") {
      if (!ENV_NAME_RE.test(item.name)) {
        throw new SecretsVaultError(400, `invalid env var name: ${item.name}`);
      }
      return { ...item, filePath: undefined };
    }
    if (item.kind === "file") {
      const filePath = normalizeFilePath(item.filePath ?? item.name);
      return { ...item, name: filePath, filePath };
    }
    throw new SecretsVaultError(400, `unknown secret kind: ${String(item.kind)}`);
  }

  function put(accountId: string, input: SecretItemInput): SecretMetadata {
    const item = validate(input);
    const usage = accountUsage.get(accountId) as { count: number; bytes: number };
    const existing = findRow.get(accountId, item.name) as Record<string, unknown> | undefined;
    const countAfter = usage.count + (existing ? 0 : 1);
    const bytesAfter = usage.bytes - (existing ? Number(existing.size_bytes) : 0) + item.value.length;
    if (countAfter > MAX_ACCOUNT_SECRETS) {
      throw new SecretsVaultError(413, `account secret limit reached (${MAX_ACCOUNT_SECRETS})`);
    }
    if (bytesAfter > MAX_ACCOUNT_TOTAL_BYTES) {
      throw new SecretsVaultError(413, `account secret storage limit reached (${MAX_ACCOUNT_TOTAL_BYTES} bytes)`);
    }
    const dek = accountDek(accountId);
    const now = new Date().toISOString();
    upsertRow.run({
      accountId,
      name: item.name,
      kind: item.kind,
      filePath: item.filePath ?? null,
      ciphertext: seal(dek, item.value, valueAad(accountId, item.name, item.kind, item.filePath)),
      sizeBytes: item.value.length,
      sha256: createHash("sha256").update(item.value).digest("hex").slice(0, 8),
      source: item.source ?? "manual",
      now,
    });
    bumpVersion.run(now, accountId);
    return {
      name: item.name,
      kind: item.kind,
      filePath: item.filePath,
      sizeBytes: item.value.length,
      sha256: createHash("sha256").update(item.value).digest("hex").slice(0, 8),
      source: item.source ?? "manual",
      createdAt: existing ? String(existing.created_at) : now,
      updatedAt: now,
    };
  }

  function list(accountId: string): { secrets: SecretMetadata[]; version: number } {
    const secrets = (listRows.all(accountId) as Record<string, unknown>[]).map((row) => ({
      name: String(row.name),
      kind: String(row.kind) as SecretKind,
      filePath: row.file_path ? String(row.file_path) : undefined,
      sizeBytes: Number(row.size_bytes),
      sha256: String(row.sha256),
      source: row.source ? String(row.source) : undefined,
      createdAt: String(row.created_at),
      updatedAt: String(row.updated_at),
    }));
    return { secrets, version: secretsVersion(accountId) };
  }

  function remove(accountId: string, name: string): boolean {
    const result = deleteRow.run(accountId, name);
    if (result.changes > 0) {
      bumpVersion.run(new Date().toISOString(), accountId);
      return true;
    }
    return false;
  }

  function exportForWorker(accountId: string): WorkerSecrets {
    const rows = listRows.all(accountId) as Record<string, unknown>[];
    if (rows.length === 0) {
      return { version: secretsVersion(accountId), env: {}, files: [] };
    }
    const dek = accountDek(accountId);
    const env: Record<string, string> = {};
    const files: WorkerSecrets["files"] = [];
    for (const row of rows) {
      const name = String(row.name);
      const kind = String(row.kind);
      const filePath = row.file_path ? String(row.file_path) : undefined;
      const plaintext = open(dek, String(row.ciphertext), valueAad(accountId, name, kind, filePath));
      if (kind === "env") {
        env[name] = plaintext.toString("utf8");
      } else {
        files.push({ path: filePath ?? name, contentBase64: plaintext.toString("base64"), mode: 0o600 });
      }
    }
    return { version: secretsVersion(accountId), env, files };
  }

  return { put, list, remove, exportForWorker };
}
