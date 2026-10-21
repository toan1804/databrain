import { describe, expect, it } from "vitest";
import { connectionErrorQuestion, scrubSecrets } from "./lib/aiQuestions";

describe("ask AI about a connection error", () => {
  it("sends the error and the settings without secrets", () => {
    const cfg = { kind: "postgres", host: "db.local", auth: { method: "password", user: "app" }, options: { sslpassword: "s3cret", api_token: "t", application_name: "x" } };
    expect(scrubSecrets(cfg)).toEqual({ ...cfg, options: { sslpassword: "(hidden)", api_token: "(hidden)", application_name: "x" } });
    const q = connectionErrorQuestion("connection refused", cfg);
    expect(q).toContain("connection refused");
    expect(q).toContain("db.local");
    expect(q).not.toContain("s3cret");
  });
});
