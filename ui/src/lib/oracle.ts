/** The Instant Client driver, as the app reads the option (`thick` is its other name; anything else is the thin driver). */
export const isInstantClient = (driver: string | undefined | null) => ["instant_client", "thick"].includes((driver ?? "").trim());
