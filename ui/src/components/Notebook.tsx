// Notebook tab: ordered SQL / Markdown cells bound to a connection.
//
// Every SQL cell runs under its own run key (`nb:{notebook}:{cell}`) so it
// keeps its own results, while all cells share one database session
// (`nb:{notebook}`), so temp tables and session settings carry across cells.
//
// The implementation lives in ./notebook/; this module is the public entry.

export { cellKey } from "./notebook/shared";
export { NotebookView } from "./notebook/NotebookView";
export { CellEditor } from "./notebook/CellEditor";
