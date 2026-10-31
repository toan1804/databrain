import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";
import { installPlainTextFields } from "./lib/textInput";
import { installAppContextMenu } from "./lib/contextMenu";

installPlainTextFields();
installAppContextMenu();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
