import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { commandExists, expandHome, pathExists, promptSelect, promptText, readJson, runCommand, runCommandSync, shortenHome, } from "./util.js";
import { authStorePath, loadAuthStore, loadConfig, saveAuthStore, saveConfig } from "./state.js";
const CLAUDE_KEYCHAIN_SERVICE = "Claude Code-credentials";
const CODEX_KEYCHAIN_SERVICE = "Codex Auth";
export async function detectEnvironment() {
    const claudeCommand = commandExists("claude");
    const codexCommand = commandExists("codex");
    const acpxCommand = commandExists("acpx");
    const jjCommand = commandExists("jj");
    const acpxVersion = acpxCommand
        ? (await runCommand("acpx", ["--version"], {
            allowFailure: true,
        })).stdout.trim()
        : undefined;
    const jjVersion = jjCommand
        ? (await runCommand("jj", ["--version"], {
            allowFailure: true,
        })).stdout.trim()
        : undefined;
    const npmAcpxLatest = (await runCommand("npm", ["view", "acpx", "version"], {
        allowFailure: true,
    })).stdout.trim();
    const claude = await readClaudeCliCredential();
    const codex = await readCodexCliCredential();
    return {
        claudeCommand,
        codexCommand,
        acpxCommand,
        jjCommand,
        acpxVersion,
        jjVersion,
        npmAcpxLatest,
        anthropicEnv: Boolean(process.env.ANTHROPIC_API_KEY?.trim()),
        openaiEnv: Boolean(process.env.OPENAI_API_KEY?.trim()),
        claudeCredential: claude?.credential,
        claudeCredentialSource: claude?.source,
        codexCredential: codex?.credential,
        codexCredentialSource: codex?.source,
    };
}
export async function syncExternalCredentials() {
    const store = await loadAuthStore();
    const claude = await readClaudeCliCredential();
    if (claude?.credential) {
        store.profiles["anthropic:claude-code"] = claude.credential;
    }
    const codex = await readCodexCliCredential();
    if (codex?.credential) {
        store.profiles["openai-codex:default"] = codex.credential;
    }
    if (process.env.ANTHROPIC_API_KEY?.trim()) {
        store.profiles["anthropic:env"] = {
            type: "api_key",
            provider: "anthropic",
            key: process.env.ANTHROPIC_API_KEY.trim(),
        };
    }
    if (process.env.OPENAI_API_KEY?.trim()) {
        store.profiles["openai:env"] = {
            type: "api_key",
            provider: "openai",
            key: process.env.OPENAI_API_KEY.trim(),
        };
    }
    await saveAuthStore(store);
    return store;
}
export async function runDoctor(options) {
    const detection = await detectEnvironment();
    const store = await syncExternalCredentials();
    const config = await loadConfig();
    const payload = {
        commands: {
            claude: detection.claudeCommand,
            codex: detection.codexCommand,
            acpx: detection.acpxCommand,
            acpxVersion: detection.acpxVersion || null,
            jj: detection.jjCommand,
            jjVersion: detection.jjVersion || null,
            acpxLatest: detection.npmAcpxLatest || null,
        },
        auth: {
            storePath: authStorePath(),
            profiles: Object.keys(store.profiles).sort(),
            claudeCredentialSource: detection.claudeCredentialSource || null,
            codexCredentialSource: detection.codexCredentialSource || null,
        },
        config,
    };
    if (options?.json) {
        console.log(JSON.stringify(payload, null, 2));
        return;
    }
    console.log("Rudder doctor");
    console.log(`  claude: ${status(detection.claudeCommand)}`);
    console.log(`  codex:  ${status(detection.codexCommand)}`);
    console.log(`  acpx:   ${status(detection.acpxCommand)}${detection.acpxVersion ? ` (${detection.acpxVersion})` : ""}${detection.npmAcpxLatest ? ` latest=${detection.npmAcpxLatest}` : ""}`);
    console.log(`  jj:     ${status(detection.jjCommand)}${detection.jjVersion ? ` (${detection.jjVersion})` : ""}`);
    console.log(`  auth:   ${shortenHome(authStorePath())}`);
    for (const [profileId, credential] of Object.entries(store.profiles).sort()) {
        console.log(`    - ${profileId} (${credential.provider}/${credential.type})`);
    }
    if (!detection.acpxCommand) {
        console.log("  fix:    run `npm install -g acpx@latest` or `rudder onboard`");
    }
    if (!detection.jjCommand) {
        console.log("  note:   jj is optional; git worktree mode still works without it.");
    }
}
export async function runOnboard(options) {
    const config = await loadConfig();
    let store = await syncExternalCredentials();
    const detection = await detectEnvironment();
    const acpxBehind = detection.acpxCommand &&
        detection.acpxVersion &&
        detection.npmAcpxLatest &&
        detection.acpxVersion !== detection.npmAcpxLatest;
    if (!detection.acpxCommand || acpxBehind) {
        console.log(detection.acpxCommand
            ? `Updating acpx ${detection.acpxVersion} to latest ${detection.npmAcpxLatest}...`
            : "Installing acpx@latest...");
        await runCommand("npm", ["install", "-g", "acpx@latest"]);
    }
    if (!options?.nonInteractive && process.stdin.isTTY) {
        printDetectedAgentAuth(detection);
        store = await configureAnthropic(store, config);
        store = await configureOpenAI(store, config);
    }
    await saveAuthStore(store);
    await saveConfig(config);
    if (options?.json) {
        console.log(JSON.stringify({
            config,
            authStore: authStorePath(),
            profiles: Object.keys(store.profiles).sort(),
        }, null, 2));
        return;
    }
    console.log("Rudder configured");
    console.log(`  auth: ${shortenHome(authStorePath())}`);
    console.log(`  profiles: ${Object.keys(store.profiles).sort().join(", ") || "(none)"}`);
    console.log('  try: rudder "fix the failing tests"');
}
function printDetectedAgentAuth(detection) {
    console.log("Agent auth");
    console.log(`  claude: ${detection.claudeCredentialSource ? `detected (${detection.claudeCredentialSource})` : detection.anthropicEnv ? "detected (ANTHROPIC_API_KEY)" : "not detected"}`);
    console.log(`  codex:  ${detection.codexCredentialSource ? `detected (${detection.codexCredentialSource})` : detection.openaiEnv ? "detected (OPENAI_API_KEY)" : "not detected"}`);
    if (!detection.claudeCredentialSource && !detection.anthropicEnv) {
        console.log("    Claude can be set up later from Claude Code or when choosing Claude models.");
    }
    if (!detection.codexCredentialSource && !detection.openaiEnv) {
        console.log("    Codex can be set up later from Codex or when choosing Codex models.");
    }
}
async function configureAnthropic(store, config) {
    if (store.profiles["anthropic:claude-code"] || store.profiles["anthropic:env"]) {
        config.backends.claude = {
            ...(config.backends.claude ?? {}),
            profileId: store.profiles["anthropic:claude-code"] ? "anthropic:claude-code" : "anthropic:env",
            model: config.backends.claude?.model ?? "sonnet",
            effort: "xhigh",
        };
        return store;
    }
    const choice = await promptSelect("Anthropic / Claude Code auth", [
        {
            value: "setup-token",
            label: "Paste setup-token",
            hint: "Run `claude setup-token`, then paste the generated token",
        },
        { value: "api-key", label: "Anthropic API key", hint: "Direct API key" },
        { value: "skip", label: "Skip Anthropic for now" },
    ], "skip");
    if (choice === "setup-token") {
        const token = await promptText("Paste Anthropic setup-token");
        store.profiles["anthropic:default"] = {
            type: "token",
            provider: "anthropic",
            token,
        };
        config.backends.claude = { ...(config.backends.claude ?? {}), profileId: "anthropic:default" };
    }
    if (choice === "api-key") {
        const key = await promptText("Paste Anthropic API key");
        store.profiles["anthropic:default"] = {
            type: "api_key",
            provider: "anthropic",
            key,
        };
        config.backends.claude = { ...(config.backends.claude ?? {}), profileId: "anthropic:default" };
    }
    return store;
}
async function configureOpenAI(store, config) {
    if (store.profiles["openai-codex:default"] || store.profiles["openai:env"]) {
        config.backends.codex = {
            ...(config.backends.codex ?? {}),
            profileId: store.profiles["openai-codex:default"] ? "openai-codex:default" : "openai:env",
            model: config.backends.codex?.model ?? "gpt-5.5",
            reasoningEffort: "xhigh",
        };
        return store;
    }
    const choice = await promptSelect("OpenAI / Codex auth", [
        { value: "codex-login", label: "Run Codex login", hint: "Uses official Codex CLI auth" },
        { value: "api-key", label: "OpenAI API key", hint: "Direct API key" },
        { value: "skip", label: "Skip OpenAI for now" },
    ], "skip");
    if (choice === "codex-login") {
        await runCommand("codex", ["login"]);
        const codex = await readCodexCliCredential();
        if (codex?.credential) {
            store.profiles["openai-codex:default"] = codex.credential;
            config.backends.codex = { ...(config.backends.codex ?? {}), profileId: "openai-codex:default" };
        }
    }
    if (choice === "api-key") {
        const key = await promptText("Paste OpenAI API key");
        store.profiles["openai:default"] = {
            type: "api_key",
            provider: "openai",
            key,
        };
        config.backends.codex = { ...(config.backends.codex ?? {}), profileId: "openai:default" };
    }
    return store;
}
export async function readClaudeCliCredential() {
    const keychain = readClaudeKeychainCredential();
    if (keychain) {
        return { credential: keychain, source: "macOS Keychain Claude Code-credentials" };
    }
    const filePath = expandHome("~/.claude/.credentials.json");
    const raw = await readJson(filePath);
    const parsed = parseClaudeOauth(raw?.claudeAiOauth);
    return parsed ? { credential: parsed, source: shortenHome(filePath) } : null;
}
function readClaudeKeychainCredential() {
    if (process.platform !== "darwin") {
        return null;
    }
    const result = runCommandSync("security", ["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"], {
        allowFailure: true,
    });
    if (result.code !== 0 || !result.stdout.trim()) {
        return null;
    }
    try {
        const raw = JSON.parse(result.stdout.trim());
        return parseClaudeOauth(raw.claudeAiOauth);
    }
    catch {
        return null;
    }
}
function parseClaudeOauth(raw) {
    if (!raw || typeof raw !== "object") {
        return null;
    }
    const entry = raw;
    const access = entry.accessToken;
    const refresh = entry.refreshToken;
    const expires = entry.expiresAt;
    if (typeof access !== "string" || !access || typeof expires !== "number") {
        return null;
    }
    if (typeof refresh === "string" && refresh) {
        return {
            type: "oauth",
            provider: "anthropic",
            access,
            refresh,
            expires,
        };
    }
    return {
        type: "token",
        provider: "anthropic",
        token: access,
        expires,
    };
}
export async function readCodexCliCredential() {
    const keychain = readCodexKeychainCredential();
    if (keychain) {
        return { credential: keychain, source: "macOS Keychain Codex Auth" };
    }
    const filePath = codexAuthPath();
    const raw = await readJson(filePath);
    const parsed = parseCodexAuth(raw, filePath);
    return parsed ? { credential: parsed, source: shortenHome(filePath) } : null;
}
function readCodexKeychainCredential() {
    if (process.platform !== "darwin") {
        return null;
    }
    const account = computeCodexKeychainAccount();
    const result = runCommandSync("security", ["find-generic-password", "-s", CODEX_KEYCHAIN_SERVICE, "-a", account, "-w"], { allowFailure: true });
    if (result.code !== 0 || !result.stdout.trim()) {
        return null;
    }
    try {
        return parseCodexAuth(JSON.parse(result.stdout.trim()), codexAuthPath());
    }
    catch {
        return null;
    }
}
function parseCodexAuth(raw, authPathForExpiry) {
    if (!raw || typeof raw !== "object") {
        return null;
    }
    const data = raw;
    const tokens = data.tokens;
    if (!tokens || typeof tokens !== "object") {
        return null;
    }
    const tokenRecord = tokens;
    const access = tokenRecord.access_token;
    const refresh = tokenRecord.refresh_token;
    if (typeof access !== "string" || !access || typeof refresh !== "string" || !refresh) {
        return null;
    }
    const lastRefresh = data.last_refresh;
    const lastRefreshMs = typeof lastRefresh === "string" || typeof lastRefresh === "number"
        ? new Date(lastRefresh).getTime()
        : Number.NaN;
    let expires = Number.isFinite(lastRefreshMs) ? lastRefreshMs + 60 * 60 * 1000 : Date.now() + 60 * 60 * 1000;
    try {
        if (Number.isNaN(lastRefreshMs)) {
            expires = fs.statSync(authPathForExpiry).mtimeMs + 60 * 60 * 1000;
        }
    }
    catch {
        // Keep fallback expiry.
    }
    return {
        type: "oauth",
        provider: "openai-codex",
        access,
        refresh,
        expires,
        ...(typeof tokenRecord.account_id === "string" ? { accountId: tokenRecord.account_id } : {}),
    };
}
function codexHome() {
    const configured = process.env.CODEX_HOME?.trim();
    const home = configured ? expandHome(configured) : expandHome("~/.codex");
    try {
        return fs.realpathSync.native(home);
    }
    catch {
        return home;
    }
}
function codexAuthPath() {
    return path.join(codexHome(), "auth.json");
}
function computeCodexKeychainAccount() {
    const hash = createHash("sha256").update(codexHome()).digest("hex");
    return `cli|${hash.slice(0, 16)}`;
}
function status(ok) {
    return ok ? "ok" : "missing";
}
export async function authStoreExists() {
    return await pathExists(authStorePath());
}
//# sourceMappingURL=auth.js.map