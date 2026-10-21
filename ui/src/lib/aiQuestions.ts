// Questions sent to the AI assistant about errors (no store imports: testable).

/** Option keys whose values must not go to the AI (passwords in options, tokens…). */
const SECRET_KEY = /pass|secret|token|key|credential|pwd/i;

/** A copy of `v` with secret-looking values replaced. */
export function scrubSecrets(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(scrubSecrets);
  if (v && typeof v === "object")
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, x]) => [k, SECRET_KEY.test(k) && typeof x === "string" && x ? "(hidden)" : scrubSecrets(x)]),
    );
  return v;
}

/** The question sent to the assistant about a connection test that failed. */
export function connectionErrorQuestion(error: string, config: unknown): string {
  return [
    "Testing this database connection in DataBrain failed. Explain what the error means, the most likely causes, and what to change (in the connection settings, on the network, or on the server) to fix it. Be concise and practical.",
    "",
    "Error:",
    "```",
    error,
    "```",
    "",
    "Connection settings (passwords and keys are not included):",
    "```json",
    JSON.stringify(scrubSecrets(config), null, 2),
    "```",
  ].join("\n");
}
