// The Pattern guide in Generate test data: how to write a column pattern,
// by column type, with examples that go into the pattern field last clicked.
import { PATTERN_GUIDE } from "../lib/testDataPatterns";

export function PatternGuide({ target, onPick }: { target: string | null; onPick: (pattern: string) => void }) {
  return (
    <div className="space-y-2 rounded-md border border-line bg-hover/40 px-2.5 py-2 text-[11.5px]" role="region" aria-label="Pattern guide">
      <p className="text-muted">
        A pattern is the rule a column's values follow. Leave it empty for DataBrain's usual values; keys and foreign keys follow their own rule.{" "}
        {target ? (
          <>
            Click an example to use it for <span className="font-mono text-fg">{target}</span>.
          </>
        ) : (
          "Click a column's pattern field, then an example to use it there."
        )}
      </p>
      {PATTERN_GUIDE.map((s) => (
        <div key={s.title} className="space-y-0.5">
          <div>
            <span className="font-medium">{s.title}</span> <span className="text-muted">{s.how}</span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            {s.examples.map((e) => (
              <button
                key={e.pattern}
                type="button"
                className="rounded border border-line bg-panel px-1.5 py-0.5 text-left hover:border-accent disabled:opacity-60"
                disabled={!target}
                title={target ? `Use ${e.pattern} for ${target}` : "Click a column's pattern field first"}
                onClick={() => onPick(e.pattern)}
              >
                <span className="font-mono">{e.pattern}</span> <span className="text-muted">→ {e.gives}</span>
              </button>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}
