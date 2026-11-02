import { Component, type ErrorInfo, type ReactNode } from "react";
import { clearStoredLayout } from "../lib/storage";

/**
 * The last line: an error that escapes every tab and panel shows what
 * happened and a way out, instead of a blank window. Saved work lives in
 * the workspace; "Reset window layout" only clears what this window keeps
 * in localStorage (sizes, open panels, chat tabs).
 */
export class AppErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("DataBrain crashed", error, info.componentStack);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div role="alert" className="flex h-screen flex-col items-center justify-center gap-3 p-8 text-center text-[13px]">
        <div className="text-[15px] font-medium">DataBrain hit an error</div>
        <pre className="max-w-2xl whitespace-pre-wrap break-words text-[12px] text-muted">{this.state.error.message}</pre>
        <div className="flex gap-2">
          <button className="btn-primary" onClick={() => window.location.reload()}>
            Reload
          </button>
          <button
            className="btn-ghost border border-line"
            title="Clears window sizes, open panels and chat tabs kept by this window (connections, queries and jobs are kept)"
            onClick={() => {
              clearStoredLayout();
              window.location.reload();
            }}
          >
            Reset window layout and reload
          </button>
        </div>
      </div>
    );
  }
}
