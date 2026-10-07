import { Component, type ErrorInfo, type ReactNode } from "react";

/** Keeps a crash inside one tab: the tab shows the error, the app keeps working. */
export class TabErrorBoundary extends Component<{ children: ReactNode; label: string }, { error: Error | null }> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(`${this.props.label} crashed`, error, info.componentStack);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center text-[12.5px]">
        <div className="font-medium text-danger">This {this.props.label} hit an error</div>
        <pre className="max-w-xl whitespace-pre-wrap break-words text-[11.5px] text-muted">{this.state.error.message}</pre>
        <button className="btn-ghost" onClick={() => this.setState({ error: null })}>
          Try again
        </button>
      </div>
    );
  }
}
