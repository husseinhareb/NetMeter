import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

// Before the first paint, so a dark choice never flashes light on start. A
// per-device view preference, so the browser store rather than config.json.
try {
  const theme = localStorage.getItem("theme");
  if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
} catch {
  // No storage: the system preference applies.
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
