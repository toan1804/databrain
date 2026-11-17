import { describe, expect, it } from "vitest";
import { isRunMoment, today, wallClockUse } from "./runDate";

describe("run date", () => {
  it("finds SQL that reads today's date, as each database spells it", () => {
    expect(wallClockUse("select * from sales where day = current_date - 1")).toBe("current_date");
    expect(wallClockUse("where ts >= NOW() - interval '1 day'")).toBe("now()");
    expect(wallClockUse("where d = trunc(SYSDATE) - 1")).toBe("sysdate");
    expect(wallClockUse("where d = cast(getdate () as date)")).toBe("getdate()");
    expect(wallClockUse("where d = today()")).toBe("today()");
  });

  it("doesn't count it in strings, quoted names or comments, or in other names", () => {
    expect(wallClockUse("select 'current_date' as label -- now()\n/* sysdate */ from t")).toBeNull();
    expect(wallClockUse('select "current_date", [getdate()] from t')).toBeNull();
    expect(wallClockUse("select known() , snow(1), current_date_col from t where day = :run_date - 1")).toBeNull();
  });

  it("knows the parameters the runner fills", () => {
    expect(isRunMoment("run_date")).toBe(true);
    expect(isRunMoment("RUN_TIME")).toBe(true);
    expect(isRunMoment("day")).toBe(false);
    expect(today(new Date(2026, 0, 5, 23, 59))).toBe("2026-01-05");
  });
});
