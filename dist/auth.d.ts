import type { AuthProfileCredential, AuthProfileStore, OAuthCredential } from "./types.js";
type Detection = {
    claudeCommand: boolean;
    codexCommand: boolean;
    acpxCommand: boolean;
    jjCommand: boolean;
    acpxVersion?: string;
    jjVersion?: string;
    npmAcpxLatest?: string;
    anthropicEnv?: boolean;
    openaiEnv?: boolean;
    claudeCredential?: AuthProfileCredential;
    claudeCredentialSource?: string;
    codexCredential?: OAuthCredential;
    codexCredentialSource?: string;
};
export declare function detectEnvironment(): Promise<Detection>;
export declare function syncExternalCredentials(): Promise<AuthProfileStore>;
export declare function runDoctor(options?: {
    json?: boolean;
}): Promise<void>;
export declare function runOnboard(options?: {
    nonInteractive?: boolean;
    json?: boolean;
}): Promise<void>;
export declare function readClaudeCliCredential(): Promise<{
    credential: AuthProfileCredential;
    source: string;
} | null>;
export declare function readCodexCliCredential(): Promise<{
    credential: OAuthCredential;
    source: string;
} | null>;
export declare function authStoreExists(): Promise<boolean>;
export {};
